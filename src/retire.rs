//! Retiring a worktree: the guarded path `agent-fleet retire`, a closed pane and a failed spawn
//! take (the TUI `x`, `dev clean` and the pre-spawn sweep do not yet: #120).
//!
//! Real uncommitted changes STOP a retire and are shown — each path and where its content
//! already lives — until someone decides `delete`, `keep` or `stash`; stash writes a patch first.
//! Two locks are held across the whole retire, always in this order: (A) this worktree's, so two
//! fleet retires of one tree never overlap, then (B) the repository's
//! `<git-common-dir>/agent-fleet-retire.lock`, the one a repo's own retire script may take too.
//! B's open file is handed to the `retire` hook (`AGENT_FLEET_RETIRE_LOCK_FD`), so a script that
//! takes B can prove it already holds it and re-enter instead of waiting on its own parent.

use anyhow::{Context, Result, bail};
use rustix::fs::{FlockOperation, flock};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::client::Held;
use crate::event::Envelope;

/// The repository-wide lock file, in the git common dir. A repo's own retire may take it too.
pub const REPO_LOCK: &str = "agent-fleet-retire.lock";
/// The variable naming the fd of B the hook inherits.
pub const LOCK_FD_ENV: &str = "AGENT_FLEET_RETIRE_LOCK_FD";
/// How long a retire waits for another to finish with the same tree or repository.
pub const WAIT: Duration = Duration::from_secs(90);
/// How long the host's own close-retire waits: past the 5 min a hook may run, so a retire
/// `agent-fleet retire` is running when it closes the pane is waited out, not reported.
pub const HOST_WAIT: Duration = Duration::from_secs(6 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decide {
    Delete,
    Keep,
    Stash,
}

impl std::str::FromStr for Decide {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "delete" => Ok(Decide::Delete),
            "keep" => Ok(Decide::Keep),
            "stash" => Ok(Decide::Stash),
            _ => Err(format!("unknown decision {s:?}: delete, keep or stash")),
        }
    }
}

impl std::fmt::Display for Decide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Decide::Delete => "delete",
            Decide::Keep => "keep",
            Decide::Stash => "stash",
        })
    }
}

/// One uncommitted change and where its content already lives: `in main` (the base ref holds
/// the same bytes at that path, or both lack it), `in its PR` (the same against
/// `origin/<branch>`), else `work`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub status: String,
    pub path: String,
    pub kind: String,
}

/// What a guarded retire did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Retired,
    /// Retired after the uncommitted changes were saved as this patch.
    Stashed(PathBuf),
    /// `keep`: nothing removed.
    Kept,
    /// The tree was already gone when the locks were held (another retire removed it).
    Gone,
    /// Stopped before the hook ran; the text says why and what to decide.
    Refused(String),
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_bytes(dir: &Path, args: &[&str], index: Option<&Path>) -> Option<Vec<u8>> {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let out = c.output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// `git status` pinned so config cannot hide work: untracked files shown, submodules not ignored.
fn status(wt: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["-c", "status.showUntrackedFiles=normal", "-C"])
        .arg(wt)
        .args(["status", "--porcelain", "--ignore-submodules=none"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_gitlink(wt: &Path, p: &str) -> bool {
    git(wt, &["ls-files", "-s", "--", p]).is_some_and(|o| o.starts_with("160000"))
}

/// A moved submodule pointer alone is not work: ` M <gitlink>` whose submodule is clean and has
/// every commit on a remote. Anything else — another path, a dirty or unpushed submodule — is.
fn clean_pointer(wt: &Path, line: &str) -> bool {
    let Some(sub) = line.strip_prefix(" M ") else {
        return false;
    };
    let inner = wt.join(sub);
    is_gitlink(wt, sub)
        && status(&inner).is_some_and(|s| s.trim().is_empty())
        && git(
            &inner,
            &["rev-list", "--count", "--all", "--not", "--remotes"],
        )
        .is_some_and(|n| n.trim() == "0")
}

/// Every uncommitted change in `wt` that is work, labelled against `base` and `origin/<branch>`.
/// Err when git cannot answer — which is never a clean tree.
pub fn changes(wt: &Path, base: &str, branch: &str) -> Result<Vec<Change>> {
    let out =
        status(wt).with_context(|| format!("git status of {} could not be read", wt.display()))?;
    let pr = format!("origin/{branch}");
    let has = |r: &str| git(wt, &["rev-parse", "-q", "--verify", r]).is_some();
    let (base_ok, pr_ok) = (has(base), !branch.is_empty() && has(&pr));
    let here = |p: &str| git(wt, &["hash-object", "--", p]).map(|o| o.trim().to_string());
    let at = |r: &str, p: &str| {
        git(wt, &["rev-parse", "-q", "--verify", &format!("{r}:{p}")]).map(|o| o.trim().to_string())
    };
    Ok(out
        .split('\n')
        .filter(|l| !l.is_empty() && !clean_pointer(wt, l))
        .map(|l| {
            let status = l.get(..2).unwrap_or("").to_string();
            let rest = l.get(3..).unwrap_or("");
            let p = rest
                .rsplit(" -> ")
                .next()
                .unwrap_or(rest)
                .trim_matches('"')
                .to_string();
            let mine = here(&p);
            // "Absent here" is a fact only for a deletion; a directory or anything git cannot
            // hash is absent to hash-object too and must not match a ref that lacks the path.
            let known = mine.is_some() || status.contains('D');
            let same = |r: &str| known && !p.ends_with('/') && at(r, &p) == mine;
            let kind = if base_ok && same(base) {
                "in main"
            } else if pr_ok && same(&pr) {
                "in its PR"
            } else {
                "work"
            };
            Change {
                status,
                path: p,
                kind: kind.into(),
            }
        })
        .collect())
}

/// What a refused retire says: each change, the diff stat, and the command that decides.
pub fn report(wt: &Path, changes: &[Change], decide_cmd: &str) -> String {
    let mut s = format!(
        "{} has {} uncommitted change{}; nothing removed.\n",
        wt.display(),
        changes.len(),
        if changes.len() == 1 { "" } else { "s" }
    );
    for c in changes {
        s += &format!("  {} {} — {}\n", c.status, c.path, c.kind);
    }
    for l in git(wt, &["diff", "--no-ext-diff", "HEAD", "--stat"])
        .unwrap_or_default()
        .lines()
    {
        s += &format!("  {l}\n");
    }
    s += &format!("decide: {decide_cmd} --decide=delete|keep|stash");
    s
}

/// `stash`: every uncommitted change — tracked edits against HEAD and each untracked file — as
/// ONE binary patch in `dir`, written before anything is removed and checked to apply to HEAD.
/// The bytes are git's own (no text decoding), and colour, prefixes, external diffs and
/// textconv are pinned so no config changes them. Err, and nothing left written, for what a patch
/// cannot carry: work inside a submodule, an untracked nested repository, an untracked file git
/// cannot diff, a patch that does not apply. A stash that leaves work out is a delete.
pub fn save_patch(wt: &Path, dir: &Path, name: &str, changes: &[Change]) -> Result<PathBuf> {
    if let Some(c) = changes.iter().find(|c| is_gitlink(wt, &c.path)) {
        bail!(
            "{} holds work inside the submodule, which a patch cannot carry",
            c.path
        );
    }
    const PIN: [&str; 6] = [
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--binary",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    let mut args = vec!["diff"];
    args.extend(PIN);
    args.push("HEAD");
    let mut patch = git_bytes(wt, &args, None).context("git diff HEAD failed")?;
    let others = git(wt, &["ls-files", "--others", "--exclude-standard", "-z"])
        .context("git ls-files failed")?;
    for f in others.split('\0').filter(|f| !f.is_empty()) {
        if f.ends_with('/') {
            bail!("{f} is a repository of its own, which a patch cannot carry");
        }
        // `--no-index` exits 1 when the two differ, which against /dev/null is always.
        let o = Command::new("git")
            .arg("-C")
            .arg(wt)
            .arg("diff")
            .args(PIN)
            .args(["--no-index", "--", "/dev/null", f])
            .output()
            .with_context(|| format!("diff {f}"))?;
        if o.status.code() != Some(1) || o.stdout.is_empty() {
            bail!("could not diff the untracked {f}");
        }
        patch.extend_from_slice(&o.stdout);
    }
    if patch.iter().all(u8::is_ascii_whitespace) {
        bail!("the patch came out empty");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let file = dir.join(format!("{}-{stamp}.patch", name.replace('/', "-")));
    std::fs::write(&file, &patch).with_context(|| format!("write {}", file.display()))?;
    // Proof it gives the work back: applied to HEAD in a scratch index, never this tree's.
    let index = dir.join(format!(".check-{stamp}.index"));
    let applies = git_bytes(wt, &["read-tree", "HEAD"], Some(&index)).is_some()
        && git_bytes(
            wt,
            &["apply", "--cached", "--check", &file.to_string_lossy()],
            Some(&index),
        )
        .is_some();
    let _ = std::fs::remove_file(&index);
    if !applies {
        let _ = std::fs::remove_file(&file);
        bail!("the patch does not apply back to HEAD");
    }
    Ok(file)
}

/// An exclusive flock on `path`, waiting up to `wait`. `what` names the holder in the refusal.
async fn lock_file(path: &Path, wait: Duration, what: &str) -> Result<Held> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match flock(&f, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Held(f)),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                if tokio::time::Instant::now() >= deadline {
                    bail!(
                        "{what} still holds {} after {}s",
                        path.display(),
                        wait.as_secs()
                    );
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => bail!("lock {}: {e}", path.display()),
        }
    }
}

/// (A) this worktree's lock, then (B) the repository's, both in the WORKTREE's git common dir —
/// the same files whichever checkout or clone the caller stands in (the checkout's for a
/// directory that is no worktree). Never the other order, and
/// nothing takes A while holding B, so no two retires can wait on each other.
pub async fn locks(checkout: &Path, wt: &Path, wait: Duration) -> Result<(Held, Held)> {
    // The checkout's only for a directory that is no git worktree at all (it holds no git work).
    let common = git(
        wt,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .or_else(|| {
        git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
    })
    .with_context(|| {
        format!(
            "git could not name {}'s common dir, where the retire locks live",
            wt.display()
        )
    })?;
    let common = PathBuf::from(common.trim());
    let tree = wt
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let a = lock_file(
        &common.join(format!("agent-fleet-retire-{tree}.lock")),
        wait,
        "another fleet retire of this worktree",
    )
    .await?;
    let b = lock_file(
        &common.join(REPO_LOCK),
        wait,
        "another retire in this repository",
    )
    .await?;
    Ok((a, b))
}

/// The `retire` hook with the worktree as $1 and B's open file inherited by THIS child only:
/// close-on-exec is cleared between fork and exec, so no other process the caller starts holds
/// the lock. Err carries the script's own words.
pub async fn run_hook(
    checkout: &Path,
    wt: &str,
    script: &str,
    extra: &[&str],
    held: &Held,
) -> Result<()> {
    let path = checkout.join(script);
    if !path.is_file() {
        bail!("retire script {script:?} is not in {}", checkout.display());
    }
    let fd = held.0.as_raw_fd();
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg(wt)
        .args(extra)
        .current_dir(checkout)
        .env(LOCK_FD_ENV, fd.to_string())
        .kill_on_drop(true);
    // SAFETY: only fcntl runs between fork and exec, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let b = rustix::fd::BorrowedFd::borrow_raw(fd);
            rustix::io::fcntl_setfd(b, rustix::io::FdFlags::empty())?;
            Ok(())
        });
    }
    let out = tokio::time::timeout(Duration::from_secs(5 * 60), cmd.output())
        .await
        .context("retire script timed out (5m) and was killed")?
        .with_context(|| format!("retire script {script} did not start"))?;
    if !out.status.success() {
        let said = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
        let (err, o) = (said(&out.stderr), said(&out.stdout));
        bail!(
            "retire script {script} refused {wt} ({}): {}",
            out.status,
            crate::spawn::tail(if err.is_empty() { &o } else { &err })
        );
    }
    Ok(())
}

/// The checkout whose `retire` hook and locks a retire of `repo` uses: the connected one
/// (`agent-fleet repo add`), else the checkout `here` only if its `origin` IS `repo` — another
/// repository's hook run on this agent's tree, and locks taken where the host's retire does not
/// look, would guard nothing.
pub fn checkout_for(
    repo: &str,
    registered: Option<PathBuf>,
    here: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(c) = registered {
        return Ok(c);
    }
    match here {
        Some(h) if crate::spawn::slug_of(&h).as_deref() == Some(repo) => Ok(h),
        Some(h) => bail!(
            "{} is not a checkout of {repo}, and {repo} is not connected (`agent-fleet repo add`)",
            h.display()
        ),
        None => bail!(
            "not in a checkout of {repo}, and {repo} is not connected (`agent-fleet repo add`)"
        ),
    }
}

/// What happens to the agent's pane before its tree is removed: nothing if the agent is not up;
/// closed if it is and herdr and the pane are both at hand; otherwise the retire must not go on —
/// removing a tree under a live agent is what this refuses. Err is the reason.
pub fn pane_step<'a>(
    up: bool,
    pane: Option<&'a str>,
    sock: Option<&'a Path>,
) -> std::result::Result<Option<(&'a str, &'a Path)>, String> {
    if !up {
        return Ok(None);
    }
    match (pane, sock) {
        (Some(p), Some(s)) => Ok(Some((p, s))),
        (None, _) => Err("the agent is up and its spawn records no pane".into()),
        (_, None) => {
            Err("the agent is up and there is no herdr here (HERDR_SOCKET_PATH unset)".into())
        }
    }
}

/// Where `stash` writes: fleet.toml's `retire_patches` (absolute, or `~/…` under `home`), else
/// `retired-patches` in the instance's data dir.
pub fn patches_dir(configured: Option<&str>, home: Option<&Path>, data: &Path) -> Result<PathBuf> {
    match configured {
        None => Ok(data.join("retired-patches")),
        Some(p) => match p.strip_prefix("~/") {
            Some(rest) => Ok(home
                .context("retire_patches starts with ~/ and there is no home dir")?
                .join(rest)),
            None if Path::new(p).is_absolute() => Ok(PathBuf::from(p)),
            None => bail!("retire_patches must be absolute or start with ~/: {p:?}"),
        },
    }
}

/// What a retire needs besides the tree: the checkout whose `retire` hook runs, the hook (a repo
/// with none is not retired by the fleet at all, as before), the
/// base ref, where stash writes, and the command a refusal tells its reader to run.
pub struct Plan<'a> {
    pub checkout: &'a Path,
    pub script: &'a str,
    pub base: &'a str,
    pub patches: &'a Path,
    pub decide_cmd: &'a str,
}

/// Under the locks, before anything irreversible: may this retire go on?
#[derive(Debug, PartialEq, Eq)]
pub enum Ready {
    Go,
    Gone,
    Kept,
    Refused(String),
}

fn branch_of(tree: &Path) -> String {
    git(tree, &["symbolic-ref", "-q", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The tree's real changes. A tree with its own `.git` is a worktree and is read, and one git
/// cannot read is an Err; a directory without one holds no git work, and the hook alone decides it.
fn read(plan: &Plan<'_>, tree: &Path) -> Result<Vec<Change>> {
    if tree.join(".git").exists() {
        changes(tree, plan.base, &branch_of(tree))
    } else {
        Ok(vec![])
    }
}

/// Whether a retire of `wt` with `decide` goes on: real changes and no decision stop it here,
/// before a pane closes or a file moves.
pub fn assess(plan: &Plan<'_>, wt: &str, decide: Option<Decide>) -> Ready {
    let tree = Path::new(wt);
    if !tree.exists() {
        return Ready::Gone;
    }
    if decide == Some(Decide::Keep) {
        return Ready::Kept;
    }
    match read(plan, tree) {
        Err(e) => Ready::Refused(format!("{e:#}; nothing removed")),
        Ok(found) if !found.is_empty() && decide.is_none() => {
            Ready::Refused(report(tree, &found, plan.decide_cmd))
        }
        Ok(_) => Ready::Go,
    }
}

/// After `assess` said Go (and the caller did what it does first, e.g. close the pane): reads
/// the tree again — the agent may have written since — stashes if told to, and runs the hook,
/// with `--decide=delete` when changes were accepted (the dirt, nothing else: the hook's own
/// guards still stand). `b` is the held repository lock the hook inherits.
pub async fn finish(
    plan: &Plan<'_>,
    wt: &str,
    decide: Option<Decide>,
    b: &Held,
) -> Result<Outcome> {
    let tree = Path::new(wt);
    if !tree.exists() {
        return Ok(Outcome::Gone);
    }
    let found = match read(plan, tree) {
        Ok(f) => f,
        Err(e) => return Ok(Outcome::Refused(format!("{e:#}; nothing removed"))),
    };
    let mut extra: Vec<&str> = vec![];
    let mut stashed = None;
    if !found.is_empty() {
        match decide {
            None => return Ok(Outcome::Refused(report(tree, &found, plan.decide_cmd))),
            Some(Decide::Keep) => return Ok(Outcome::Kept),
            Some(Decide::Stash) => {
                let b = branch_of(tree);
                let name = if b.is_empty() {
                    tree.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                } else {
                    b
                };
                match save_patch(tree, plan.patches, &name, &found) {
                    Ok(f) => stashed = Some(f),
                    Err(e) => {
                        return Ok(Outcome::Refused(format!(
                            "could not stash {wt}: {e:#}; nothing removed"
                        )));
                    }
                }
            }
            Some(Decide::Delete) => {}
        }
        extra.push("--decide=delete");
    }
    run_hook(plan.checkout, wt, plan.script, &extra, b).await?;
    Ok(stashed.map_or(Outcome::Retired, Outcome::Stashed))
}

/// The guarded retire in one call, for a caller with nothing to do between the read and the
/// removal (the host's close-retire, a failed spawn): locks up to `wait`, `assess`, `finish`.
pub async fn guarded(
    plan: &Plan<'_>,
    wt: &str,
    decide: Option<Decide>,
    wait: Duration,
) -> Result<Outcome> {
    let tree = Path::new(wt);
    if !tree.exists() {
        return Ok(Outcome::Gone);
    }
    let (_a, b) = locks(plan.checkout, tree, wait).await?;
    match assess(plan, wt, decide) {
        Ready::Gone => Ok(Outcome::Gone),
        Ready::Kept => Ok(Outcome::Kept),
        Ready::Refused(why) => Ok(Outcome::Refused(why)),
        Ready::Go => finish(plan, wt, decide, &b).await,
    }
}

// ---------- `agent-fleet retire`, on the agent's host ----------

/// What a retire on this host needs: the CLI fills it from its own process, the host from its
/// config (its herdr socket, not an environment variable a request cannot carry).
pub struct Here<'a> {
    pub state: &'a crate::state::State,
    pub sp: &'a crate::state::Spawn,
    pub host: &'a str,
    pub decide: Option<Decide>,
    pub machine: Option<PathBuf>,
    /// The checkout to use when the repo is not registered: the CLI's own, if it is in one.
    pub cwd_checkout: Option<PathBuf>,
    pub data_dir: &'a Path,
    pub sock: Option<PathBuf>,
}

/// What `run_here` did: the text to print (stdout when `ok`, stderr when not), and the BLOCKED
/// message for the agent's lead when the retire was refused over uncommitted changes.
pub struct Done {
    pub ok: bool,
    pub text: String,
    pub blocked: Option<crate::event::MsgSent>,
}

fn done(ok: bool, text: impl Into<String>) -> Done {
    Done {
        ok,
        text: text.into(),
        blocked: None,
    }
}

/// The guarded retire of `h.sp`'s agent, here: only a tree the fleet made on THIS host, locked,
/// read, the pane closed, the repository's hook run. Never an error: whatever went wrong is the
/// text of a not-ok `Done`.
pub async fn run_here(h: &Here<'_>) -> Done {
    match try_here(h).await {
        Ok(d) => d,
        Err(e) => done(false, format!("{e:#}")),
    }
}

async fn try_here(h: &Here<'_>) -> Result<Done> {
    let sp = h.sp;
    let agent = crate::spawn::short(&sp.request.agent);
    // The host's own rule: only a tree the fleet made, on THIS host, that no newer spawn handed
    // to another agent — never a main checkout, never another agent's tree.
    let Some((repo, wt, issue)) = h.state.retire_target(&sp.request.agent, h.host) else {
        bail!(
            "{agent}: no worktree to retire on {} (a main checkout, a tree handed to another agent, or another host's agent)",
            h.host
        );
    };
    // keep = leave it: the pane too, so the host's own close-retire never starts.
    if h.decide == Some(Decide::Keep) {
        return Ok(done(
            true,
            format!("kept {wt}: decided keep; pane and tree untouched"),
        ));
    }
    if !Path::new(&wt).exists() {
        return Ok(done(true, format!("{wt} was already retired")));
    }
    let registered = h
        .machine
        .as_deref()
        .map(crate::setup::registered)
        .transpose()?
        .and_then(|mut r| r.remove(&repo));
    let checkout = checkout_for(&repo, registered, h.cwd_checkout.clone())?;
    let (cfg, _) = crate::spawn::load_in(&checkout, h.machine.as_deref())?;
    let Some(script) = cfg.retire.clone() else {
        bail!(
            "{}'s fleet.toml has no `retire` hook; nothing would retire {wt}",
            checkout.display()
        );
    };
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    let patches = patches_dir(cfg.retire_patches.as_deref(), home.as_deref(), h.data_dir)?;
    let cmd = format!("agent-fleet retire {agent}");
    let plan = Plan {
        checkout: &checkout,
        script: &script,
        base: &cfg.base,
        patches: &patches,
        decide_cmd: &cmd,
    };
    // Locked, then read, all BEFORE the pane closes: a refusal leaves the agent up and starts no
    // host retire; the host's retire of the close that does follow waits on these locks and finds
    // the tree gone.
    let (_a, b) = locks(&checkout, Path::new(&wt), WAIT).await?;
    let outcome = match assess(&plan, &wt, h.decide) {
        Ready::Gone => Outcome::Gone,
        Ready::Kept => Outcome::Kept,
        Ready::Refused(why) => Outcome::Refused(why),
        Ready::Go => {
            let pane = sp.result.as_ref().and_then(|r| r.pane.clone());
            let up = h.state.agents.contains_key(&sp.request.agent);
            let step = pane_step(up, pane.as_deref(), h.sock.as_deref())
                .map_err(|e| anyhow::anyhow!("refusing to retire {wt}: {e}; nothing removed"))?;
            if let Some((p, s)) = step
                && let Err(e) =
                    crate::host::herdr_call(s, "pane.close", serde_json::json!({ "pane_id": p }))
                        .await
            {
                bail!(
                    "refusing to retire {wt}: {agent} is up and its pane {p} did not close ({e:#}); nothing removed"
                );
            }
            finish(&plan, &wt, h.decide, &b).await?
        }
    };
    Ok(match outcome {
        Outcome::Retired => done(true, format!("retired {wt}")),
        Outcome::Stashed(f) => done(
            true,
            format!(
                "stashed {wt}'s uncommitted changes in {} (on this host)\nretired {wt}",
                f.display()
            ),
        ),
        Outcome::Kept => done(true, format!("kept {wt}: decided keep, nothing removed")),
        Outcome::Gone => done(true, format!("{wt} was already retired")),
        Outcome::Refused(why) => Done {
            ok: false,
            blocked: Some(crate::event::MsgSent {
                to: crate::spawn::lead_of(sp),
                kind: "BLOCKED".into(),
                reference: issue
                    .map(|n| format!("{repo}#{n}"))
                    .unwrap_or_else(|| repo.clone()),
                text: why.clone(),
            }),
            text: why,
        },
    })
}

/// Follows the hub until the request's `retire.result` arrives. A retire waits on locks (90 s)
/// and the repository's hook, so the wait is longer than a spawn's poll interval suggests.
pub async fn wait_result(
    client: &crate::client::Client,
    id: &str,
) -> Result<crate::event::RetireResult> {
    let deadline = tokio::time::Instant::now() + crate::spawn::REQUEST_MAX_AGE.to_std().unwrap_or(Duration::from_secs(900));
    loop {
        match client.state().await {
            Ok(s) => {
                if let Some(r) = s.retires.get(id).and_then(|r| r.result.clone()) {
                    return Ok(r);
                }
            }
            Err(e) if e.downcast_ref::<crate::client::Refused>().is_some() => {
                bail!("waiting for the retire result: {e:#}");
            }
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("no retire.result for {id} within 15 min — the agent's host may be down (or too old an agent-fleet to answer retire requests)");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The `retire.requested` ids this host should run: its own, unanswered, not seen before, and
/// younger than a spawn request may be.
pub fn due(
    state: &crate::state::State,
    host: &str,
    session: &str,
    seen: &mut std::collections::HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    seen.retain(|id| state.retires.get(id).is_some_and(|r| r.result.is_none()));
    state
        .retires
        .iter()
        .filter(|(id, r)| {
            r.request.host == host
                // One host agent per herdr session: pane ids repeat across sessions, so only the
                // session the agent runs in may close its pane.
                && crate::spawn::find_spawn(state, &r.request.agent)
                    .is_some_and(|sp| sp.request.session == session)
                && r.result.is_none()
                && now - r.since <= crate::spawn::REQUEST_MAX_AGE
                && seen.insert((*id).clone())
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// The host's side: run each retire asked of this host and answer it. Runs on the spawn loop's
/// tick, one at a time, so a retire never overlaps another on this host.
pub async fn serve(host: &crate::host::Host, seen: &mut std::collections::HashSet<String>) {
    let Ok(view) = host.view() else { return };
    let state = &view.state;
    for id in due(state, &host.cfg.host, &host.cfg.session, seen, chrono::Utc::now()) {
        let Some(req) = state.retires.get(&id) else {
            continue;
        };
        // A self-update drain waits for this: a restart mid-hook would kill it half done.
        let Some(_busy) = host.busy_guard(format!("retire of {}", req.request.agent)) else {
            continue;
        };
        let (done, project) = match crate::spawn::find_spawn(state, &req.request.agent) {
            None => (
                done(false, format!("{} has no spawn record", req.request.agent)),
                req.project.clone(),
            ),
            Some(sp) => {
                let decide = req
                    .request
                    .decide
                    .as_deref()
                    .and_then(|d| d.parse::<Decide>().ok());
                let here = Here {
                    state,
                    sp,
                    host: &host.cfg.host,
                    decide,
                    machine: host.cfg.config_dir.clone(),
                    // The host's `--repo` flags too, as a spawn resolves its checkout.
                    cwd_checkout: host
                        .cfg
                        .checkouts_now()
                        .ok()
                        .and_then(|mut c| c.remove(&sp.request.repo)),
                    data_dir: &host.cfg.data_dir,
                    sock: Some(host.cfg.herdr_socket.clone()),
                };
                (run_here(&here).await, req.project.clone())
            }
        };
        if let Some(body) = done.blocked {
            host.publish(Envelope::new(
                host.actor(),
                &project,
                crate::event::Known::MsgSent(body),
            ))
            .await;
        }
        host.publish(
            host.presence(crate::event::Known::RetireResult(
                crate::event::RetireResult {
                    agent: req.request.agent.clone(),
                    ok: done.ok,
                    text: done.text,
                },
            ))
            .re(&id),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_host_runs_only_its_own_unanswered_retires_once() {
        use crate::event::RetireRequested;
        use crate::state::{Retire, State};
        let now = chrono::Utc::now();
        let mut state = State::default();
        let mut add = |id: &str, host: &str, age_min: i64, answered: bool| {
            state.retires.insert(
                id.into(),
                Retire {
                    request: RetireRequested {
                        agent: "agent:kir/s/a".into(),
                        host: host.into(),
                        decide: None,
                    },
                    by: "agent:mac/s/me".into(),
                    project: "p".into(),
                    result: answered.then(|| crate::event::RetireResult {
                        agent: "agent:kir/s/a".into(),
                        ok: true,
                        text: String::new(),
                    }),
                    since: now - chrono::Duration::minutes(age_min),
                },
            );
        };
        add("mine", "kir", 1, false);
        add("theirs", "mac", 1, false);
        add("done", "kir", 1, true);
        add("old", "kir", 60, false);
        state.spawns.insert("sp".into(), crate::state::Spawn {
            request: crate::event::SpawnRequested {
                name: "a".into(), agent: "agent:kir/s/a".into(), host: "kir".into(),
                session: "s".into(), repo: "o/r".into(), issue: None, branch: "b".into(),
                profile: "p".into(), brief: String::new(), level: None, lead: None,
                worktree: None, replaces: None, why: String::new(),
            },
            by: "x".into(), project: "p".into(), result: None, since: now, answered: None,
        });
        let mut seen = std::collections::HashSet::new();
        assert_eq!(due(&state, "kir", "s", &mut seen, now), vec!["mine".to_string()]);
        assert!(due(&state, "kir", "s", &mut seen, now).is_empty(), "not twice");
        assert!(
            due(&state, "kir", "other", &mut std::collections::HashSet::new(), now).is_empty(),
            "another session's host leaves it alone"
        );
    }
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A checkout `co` on `main` with `a.txt`, a worktree `wt` on `feat`, and a hook `x.sh` that
    /// records its arguments and whether it inherited the lock fd, then removes the tree.
    struct Fx {
        _d: tempfile::TempDir,
        co: PathBuf,
        wt: PathBuf,
        patches: PathBuf,
    }
    fn fx() -> Fx {
        fx_hook("")
    }
    /// `fx` with `extra` shell run by the hook before it removes the tree.
    fn fx_hook(extra: &str) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let (co, wt) = (root.join("co"), root.join("wt"));
        std::fs::create_dir(&co).unwrap();
        sh(&co, &["init", "-q", "-b", "main"]);
        std::fs::write(co.join("a.txt"), "a\n").unwrap();
        sh(&co, &["add", "a.txt"]);
        sh(&co, &["commit", "-qm", "init"]);
        sh(
            &co,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
        );
        let hook = co.join("x.sh");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}/hook-args\n[ -n \"${LOCK_FD_ENV}\" ] && [ -e \"/dev/fd/${LOCK_FD_ENV}\" ] && echo inherited >> {log}/hook-fd\n{extra}\ngit worktree remove --force \"$1\"\n",
                log = root.display(),
                extra = extra
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let patches = root.join("patches");
        Fx {
            _d: d,
            co,
            wt,
            patches,
        }
    }
    impl Fx {
        fn plan(&self) -> Plan<'_> {
            Plan {
                checkout: &self.co,
                script: "x.sh",
                base: "main",
                patches: &self.patches,
                decide_cmd: "agent-fleet retire dv-1",
            }
        }
        fn wt(&self) -> &str {
            self.wt.to_str().unwrap()
        }
        fn read(&self, f: &str) -> String {
            std::fs::read_to_string(self.co.parent().unwrap().join(f)).unwrap_or_default()
        }
        fn dirty(&self) {
            std::fs::write(self.wt.join("a.txt"), "edited, nowhere else\n").unwrap();
            std::fs::write(self.wt.join("new.txt"), "untracked\n").unwrap();
        }
    }

    #[test]
    fn changes_label_each_path_by_where_its_content_already_lives() {
        let f = fx();
        // main moves on with b.txt; the tree holds the same bytes uncommitted
        std::fs::write(f.co.join("b.txt"), "landed\n").unwrap();
        sh(&f.co, &["add", "b.txt"]);
        sh(&f.co, &["commit", "-qm", "b"]);
        std::fs::write(f.wt.join("b.txt"), "landed\n").unwrap();
        f.dirty();
        std::fs::create_dir(f.wt.join("dir")).unwrap();
        std::fs::write(f.wt.join("dir/x"), "x\n").unwrap();
        let mut got: Vec<(String, String)> = changes(&f.wt, "main", "feat")
            .unwrap()
            .into_iter()
            .map(|c| (c.path, c.kind))
            .collect();
        got.sort();
        let w = |p: &str, k: &str| (p.to_string(), k.to_string());
        assert_eq!(
            got,
            vec![
                w("a.txt", "work"),
                w("b.txt", "in main"),
                w("dir/", "work"),
                w("new.txt", "work")
            ]
        );
    }

    #[tokio::test]
    async fn a_clean_tree_is_retired_by_the_hook_with_no_decision_and_the_lock_fd() {
        let f = fx();
        assert_eq!(
            guarded(&f.plan(), f.wt(), None, WAIT).await.unwrap(),
            Outcome::Retired
        );
        assert_eq!(f.read("hook-args").trim(), f.wt());
        assert_eq!(f.read("hook-fd").trim(), "inherited");
        assert!(!f.wt.exists());
    }

    #[tokio::test]
    async fn real_changes_stop_the_retire_before_the_hook_and_say_what_they_are() {
        let f = fx();
        f.dirty();
        let Outcome::Refused(why) = guarded(&f.plan(), f.wt(), None, WAIT).await.unwrap() else {
            panic!("not refused")
        };
        for want in [
            "a.txt — work",
            "new.txt — work",
            "1 file changed",
            "decide: agent-fleet retire dv-1 --decide=delete|keep|stash",
        ] {
            assert!(why.contains(want), "missing {want:?} in:\n{why}");
        }
        assert_eq!(f.read("hook-args"), "", "the hook ran");
        assert!(f.wt.join("new.txt").exists());
    }

    #[tokio::test]
    async fn keep_removes_nothing_and_runs_no_hook() {
        let f = fx();
        f.dirty();
        assert_eq!(
            guarded(&f.plan(), f.wt(), Some(Decide::Keep), WAIT)
                .await
                .unwrap(),
            Outcome::Kept
        );
        assert_eq!(f.read("hook-args"), "");
        assert!(f.wt.join("new.txt").exists());
    }

    #[tokio::test]
    async fn delete_tells_the_hook_the_dirt_was_accepted() {
        let f = fx();
        f.dirty();
        assert_eq!(
            guarded(&f.plan(), f.wt(), Some(Decide::Delete), WAIT)
                .await
                .unwrap(),
            Outcome::Retired
        );
        assert_eq!(
            f.read("hook-args").trim(),
            format!("{} --decide=delete", f.wt())
        );
    }

    #[tokio::test]
    async fn stash_writes_a_patch_that_applies_back_before_the_hook_runs() {
        let f = fx();
        f.dirty();
        std::fs::write(f.wt.join("bin.dat"), [0u8, 1, 0, 255]).unwrap();
        assert_eq!(
            guarded(&f.plan(), f.wt(), Some(Decide::Stash), WAIT)
                .await
                .unwrap(),
            Outcome::Retired
        );
        assert_eq!(
            f.read("hook-args").trim(),
            format!("{} --decide=delete", f.wt())
        );
        let p: Vec<PathBuf> = std::fs::read_dir(&f.patches)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(p.len(), 1);
        assert!(
            p[0].file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("feat-")
        );
        sh(&f.co, &["switch", "-q", "feat"]);
        sh(&f.co, &["apply", p[0].to_str().unwrap()]);
        assert_eq!(
            std::fs::read_to_string(f.co.join("a.txt")).unwrap(),
            "edited, nowhere else\n"
        );
        assert_eq!(
            std::fs::read(f.co.join("bin.dat")).unwrap(),
            vec![0u8, 1, 0, 255]
        );
    }

    #[tokio::test]
    async fn stash_refuses_what_a_patch_cannot_carry_and_removes_nothing() {
        let f = fx();
        sh(&f.wt, &["init", "-q", "nested"]);
        std::fs::write(f.wt.join("nested/w.txt"), "w\n").unwrap();
        let Outcome::Refused(why) = guarded(&f.plan(), f.wt(), Some(Decide::Stash), WAIT)
            .await
            .unwrap()
        else {
            panic!("not refused")
        };
        assert!(why.contains("nested/"), "{why}");
        assert!(!f.patches.exists());
        assert_eq!(f.read("hook-args"), "");
    }

    #[tokio::test]
    async fn a_status_git_cannot_read_is_refused_never_clean() {
        let f = fx();
        let gd = git(&f.wt, &["rev-parse", "--absolute-git-dir"]).unwrap();
        std::fs::write(Path::new(gd.trim()).join("index"), "junk").unwrap();
        let Outcome::Refused(why) = guarded(&f.plan(), f.wt(), Some(Decide::Delete), WAIT)
            .await
            .unwrap()
        else {
            panic!("not refused")
        };
        assert!(why.contains("could not be read"), "{why}");
        assert_eq!(f.read("hook-args"), "");
    }

    #[tokio::test]
    async fn two_retires_of_one_tree_one_removes_it_and_the_other_finds_it_gone() {
        let f = fx_hook("sleep 1");
        let (p1, p2) = (f.plan(), f.plan());
        let both = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(
                guarded(&p1, f.wt(), None, WAIT),
                guarded(&p2, f.wt(), None, WAIT)
            )
        })
        .await
        .expect("a retire hung");
        let mut got = vec![both.0.unwrap(), both.1.unwrap()];
        got.sort_by_key(|o| format!("{o:?}"));
        assert_eq!(got, vec![Outcome::Gone, Outcome::Retired]);
        assert_eq!(f.read("hook-args").lines().count(), 1, "the hook ran twice");
    }

    #[tokio::test]
    async fn a_hook_child_left_running_with_the_inherited_fd_does_not_keep_the_lock() {
        // The fd is the held open file; unlocking it (Held's drop) releases the flock for every
        // process sharing it, so a background child the hook leaves behind holds nothing.
        let f = fx_hook("sleep 5 &");
        assert_eq!(
            guarded(&f.plan(), f.wt(), None, WAIT).await.unwrap(),
            Outcome::Retired
        );
        let common = f.co.join(".git");
        lock_file(&common.join(REPO_LOCK), Duration::ZERO, "test")
            .await
            .expect("the repository lock is still held");
    }

    #[tokio::test]
    async fn stash_keeps_bytes_that_are_not_utf8_and_ignores_colour_and_prefix_config() {
        let f = fx();
        sh(&f.co, &["config", "color.ui", "always"]);
        sh(&f.co, &["config", "diff.noprefix", "true"]);
        let latin1 = b"caf\xe9, na\xefve\n".to_vec();
        std::fs::write(f.wt.join("a.txt"), &latin1).unwrap();
        std::fs::write(f.wt.join("l1.txt"), &latin1).unwrap();
        assert_eq!(
            guarded(&f.plan(), f.wt(), Some(Decide::Stash), WAIT)
                .await
                .unwrap(),
            Outcome::Retired
        );
        let p = std::fs::read_dir(&f.patches)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        sh(&f.co, &["switch", "-q", "feat"]);
        sh(&f.co, &["apply", p.to_str().unwrap()]);
        assert_eq!(std::fs::read(f.co.join("a.txt")).unwrap(), latin1);
        assert_eq!(std::fs::read(f.co.join("l1.txt")).unwrap(), latin1);
    }

    #[tokio::test]
    async fn a_tree_already_gone_runs_no_hook() {
        let f = fx();
        sh(&f.co, &["worktree", "remove", f.wt()]);
        assert_eq!(
            guarded(&f.plan(), f.wt(), None, WAIT).await.unwrap(),
            Outcome::Gone
        );
        assert_eq!(f.read("hook-args"), "");
    }

    #[test]
    fn checkout_for_takes_the_connected_one_else_only_a_checkout_of_the_same_repo() {
        let f = fx();
        sh(
            &f.co,
            &["remote", "add", "origin", "git@github.com:o/r.git"],
        );
        let other = PathBuf::from("/elsewhere");
        assert_eq!(
            checkout_for("o/r", Some(other.clone()), Some(f.co.clone())).unwrap(),
            other
        );
        assert_eq!(checkout_for("o/r", None, Some(f.co.clone())).unwrap(), f.co);
        let e = checkout_for("o/other", None, Some(f.co.clone()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a checkout of o/other"), "{e}");
        assert!(checkout_for("o/r", None, None).is_err());
    }

    #[test]
    fn patches_dir_is_absolute_home_relative_or_the_data_dir() {
        let (home, data) = (Path::new("/h"), Path::new("/d"));
        assert_eq!(
            patches_dir(None, Some(home), data).unwrap(),
            PathBuf::from("/d/retired-patches")
        );
        assert_eq!(
            patches_dir(Some("~/p"), Some(home), data).unwrap(),
            PathBuf::from("/h/p")
        );
        assert_eq!(
            patches_dir(Some("/abs"), Some(home), data).unwrap(),
            PathBuf::from("/abs")
        );
        assert!(patches_dir(Some("rel"), Some(home), data).is_err());
        assert!(patches_dir(Some("~/p"), None, data).is_err());
    }

    #[test]
    fn pane_step_closes_an_up_agents_pane_and_refuses_when_it_cannot() {
        let s = Path::new("/sock");
        assert_eq!(
            pane_step(false, None, None),
            Ok(None),
            "an agent that is down needs nothing"
        );
        assert_eq!(pane_step(true, Some("p1"), Some(s)), Ok(Some(("p1", s))));
        assert!(
            pane_step(true, Some("p1"), None)
                .unwrap_err()
                .contains("no herdr here")
        );
        assert!(
            pane_step(true, None, Some(s))
                .unwrap_err()
                .contains("no pane")
        );
    }
}
