//! `agent-fleet self-update`: each host pulls the clone its binary was built from, rebuilds into
//! its own binary's place and exits, so its supervisor starts the new one.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;

use crate::client::{Client, Refused};
use crate::event::{Known, MsgSent, UpdateRequested, UpdateResult};
use crate::host::Host;
use crate::spawn::tail;
use crate::state::State;

/// The commit this binary was built from (`build.rs`); `None` outside a git checkout.
pub const COMMIT: Option<&str> = option_env!("AGENT_FLEET_COMMIT");
/// The checkout this binary was built from: `cargo install --path <clone>` compiles it in place.
pub(crate) const CLONE: &str = env!("CARGO_MANIFEST_DIR");
/// Exit status after a replace: non-zero, so `Restart=on-failure` and launchd's `KeepAlive`
/// (even with `SuccessfulExit=false`) both start the new binary.
pub const RESTART_STATUS: i32 = 75;
/// A request this old when the host first sees it is ignored: a host down at the time does not
/// rebuild by surprise later.
pub(crate) const REQUEST_MAX_AGE: chrono::Duration = chrono::Duration::minutes(15);
/// How long `self-update` waits for every host to run its new commit.
const WAIT: Duration = Duration::from_secs(20 * 60);

/// The first 7 characters of a commit, or `unknown`.
pub fn short(commit: Option<&str>) -> &str {
    commit.map_or("unknown", |c| c.get(..7).unwrap_or(c))
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The clone's `main` is the commit this binary runs: nothing to do.
    Current(String),
    /// The binary at `exe` was replaced by a build of this commit; the process must restart.
    Replaced(String),
}

/// A fetch waits on the network, a build compiles: each gets a wall-clock limit, or a hung
/// credential prompt or compiler silences the host for good.
const PULL: Duration = Duration::from_secs(120);
const GIT: Duration = Duration::from_secs(60);
/// `<exe> --version`: a whole binary answers at once.
const VERSION: Duration = Duration::from_secs(10);
pub(crate) const BUILD: Duration = Duration::from_secs(30 * 60);
/// A `.agent-fleet-update-<pid>` untouched this long (more than `BUILD`) is assumed to be the
/// leftover of a killed update, not another host's build in progress.
const LEFTOVER: Duration = Duration::from_secs(45 * 60);
/// How long a finished update waits for spawns and handoffs in flight before it exits anyway: a
/// hung one must not keep the old binary running for good. Under the age at which a spawn
/// request is answered `expired` (`spawn::REQUEST_MAX_AGE`): one that arrives when the drain
/// starts must still be fresh for the new process.
const DRAIN_MAX: Duration = Duration::from_secs(10 * 60);
/// How long an update waits for another update in its clone (a build may take `BUILD`).
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(35 * 60);
/// How often a host looks whether it is behind `origin/main`.
const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often a finished update looks for the work in flight to end.
const DRAIN_POLL: Duration = Duration::from_secs(1);

/// Runs `c` to the end, or kills its whole process group after `limit` (cargo and git start
/// children of their own). No stdin and no git prompt (the environment applies to cargo too):
/// under a supervisor nobody can answer.
async fn limited(mut c: tokio::process::Command, limit: Duration) -> Result<std::process::Output> {
    let child = c
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id();
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(out) => Ok(out?),
        Err(_) => {
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            bail!("timed out after {}s", limit.as_secs())
        }
    }
}

/// What `exe --version` says, when it runs, exits 0 and names itself: proof that a replaced
/// binary is whole.
pub(crate) async fn binary_says(exe: &Path) -> Result<String> {
    let mut c = tokio::process::Command::new(exe);
    c.arg("--version");
    let out = limited(c, VERSION)
        .await
        .with_context(|| format!("run {} --version", exe.display()))?;
    if !out.status.success() {
        bail!("{} --version exited {}", exe.display(), out.status);
    }
    let says = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // An empty or truncated file can "run" as an empty script and exit 0: it must say who it is.
    if !says.starts_with("agent-fleet ") {
        bail!("{} --version says {says:?}", exe.display());
    }
    Ok(says)
}

/// Whether the build that `says` (a `--version` line) was made from an older commit than the one
/// running (`running`, full): a rollback, which a hub does not follow. Judged by git in `clone`;
/// when that cannot tell (no clone, an unknown commit), it is not a rollback.
pub(crate) async fn is_rollback(clone: &Path, running: Option<&str>, says: &str) -> bool {
    let (Some(running), Some(open)) = (running, says.rfind('(')) else {
        return false;
    };
    let new: String = says[open + 1..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    if new.len() < 7 || running.starts_with(new.as_str()) {
        return false;
    }
    let mut c = tokio::process::Command::new("git");
    c.arg("-C")
        .arg(clone)
        .args(["merge-base", "--is-ancestor", &new, running]);
    // 0: the new commit is an ancestor of the running one (older); 1: not; anything else: unknown.
    matches!(limited(c, GIT).await.map(|o| o.status.code()), Ok(Some(0)))
}

/// Whether git should be told `ssh -o BatchMode=yes`: not when the operator chose an ssh command
/// (`GIT_SSH_COMMAND` or `GIT_SSH`, or `core.sshCommand` in the clone), which BatchMode would override. Only
/// git's "not set" (exit 1) allows it: any other failure leaves the operator's setting alone.
async fn wants_batch_ssh(clone: &Path, env_set: bool) -> bool {
    if env_set {
        return false;
    }
    let mut c = tokio::process::Command::new("git");
    c.arg("-C")
        .arg(clone)
        .args(["config", "--get", "core.sshCommand"]);
    match limited(c, GIT).await.map(|o| o.status.code()) {
        Ok(Some(1)) => true,
        Ok(Some(0)) => false,
        other => {
            eprintln!(
                "agent-fleet host: update: cannot read core.sshCommand ({other:?}); leaving ssh as configured"
            );
            false
        }
    }
}

/// `git -C <clone> <args>`, told not to prompt for ssh when no ssh command is configured.
async fn git_command(clone: &Path, args: &[&str]) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("git");
    c.arg("-C").arg(clone).args(args);
    // git takes its ssh from GIT_SSH_COMMAND, then core.sshCommand, then GIT_SSH.
    let chosen =
        std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some();
    if wants_batch_ssh(clone, chosen).await {
        c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    c
}

async fn git_in(clone: &Path, args: &[&str], limit: Duration) -> Result<String> {
    let c = git_command(clone, args).await;
    let out = limited(c, limit)
        .await
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} ({}): {}",
            args.join(" "),
            out.status,
            tail(String::from_utf8_lossy(&out.stderr).trim())
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// One update at a time per clone: the lock lives in the clone's git dir, so two hosts of one
/// machine never race on `git pull` or both build. An update waits for it (`wait`), the timer's
/// fetch does not. The file is only held, never read: dropping it releases the lock.
struct Lock(#[allow(dead_code)] std::fs::File);

/// Takes the lock at once or fails.
async fn lock(clone: &Path) -> Result<Lock> {
    lock_for(clone, Duration::ZERO).await
}

/// Takes the lock, waiting up to `wait` for another update in the clone to end.
async fn lock_for(clone: &Path, wait: Duration) -> Result<Lock> {
    use rustix::fs::{FlockOperation, flock};
    let dir = git_in(clone, &["rev-parse", "--absolute-git-dir"], GIT).await?;
    let path = PathBuf::from(dir).join("agent-fleet-update.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match flock(&f, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Lock(f)),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                if tokio::time::Instant::now() >= deadline {
                    bail!("another update is running in {}", clone.display());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => bail!("lock {}: {e}", path.display()),
        }
    }
}

/// Removes the build directories a killed update left next to the binary.
pub fn sweep(dir: &Path, older_than: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= older_than);
        let name = e.file_name().to_string_lossy().into_owned();
        // Another host of this machine may be building in it right now.
        let building = name
            .strip_prefix(".agent-fleet-update-")
            .and_then(|p| p.parse::<i32>().ok())
            .is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0);
        if old
            && !building
            && name.starts_with(".agent-fleet-update-")
            && let Err(err) = std::fs::remove_dir_all(e.path())
        {
            eprintln!("agent-fleet host: sweep {}: {err}", e.path().display());
        }
    }
}

/// Pulls `clone` (which must be on `main` with no uncommitted changes), and unless `running` is
/// already its head, builds it and renames the build over `exe`. The old binary stays as
/// `<exe>.prev`: to roll back, rename it over `exe` and restart.
pub async fn update(clone: &Path, exe: &Path, running: Option<&str>) -> Result<Outcome> {
    update_waiting(clone, exe, running, LOCK_WAIT).await
}

async fn update_waiting(
    clone: &Path,
    exe: &Path,
    running: Option<&str>,
    wait: Duration,
) -> Result<Outcome> {
    // First: the checks below must hold while this update pulls. Another host of this machine
    // may be building in the same clone: wait for it rather than fail, or this host would
    // answer `failed` and, with nothing left to ask for, stay on the old binary.
    let _lock = lock_for(clone, wait).await?;
    let branch = git_in(clone, &["symbolic-ref", "--short", "HEAD"], GIT)
        .await
        .map_err(|e| {
            if format!("{e:#}").contains("not a symbolic ref") {
                anyhow!("{} is not on a branch", clone.display())
            } else {
                e
            }
        })?;
    if branch != "main" {
        bail!("{} is on {branch}, not main", clone.display());
    }
    if !git_in(
        clone,
        &["status", "--porcelain", "--untracked-files=no"],
        GIT,
    )
    .await?
    .is_empty()
    {
        bail!("{} has uncommitted changes", clone.display());
    }
    git_in(clone, &["pull", "--ff-only", "--quiet"], PULL).await?;
    let head = git_in(clone, &["rev-parse", "HEAD"], GIT).await?;
    if running == Some(head.as_str()) {
        return Ok(Outcome::Current(head));
    }
    let dir = exe.parent().context("the binary has no directory")?;
    // Built beside the binary, so the rename is atomic. Overwriting it in place kills the running
    // process on macOS (code signing).
    let root = dir.join(format!(".agent-fleet-update-{}", std::process::id()));
    clean(&root);
    // A supervised daemon's PATH may lack cargo; `cargo install` puts binaries next to it.
    let cargo = Some(dir.join("cargo"))
        .filter(|c| c.exists())
        .unwrap_or_else(|| PathBuf::from("cargo"));
    let mut build = tokio::process::Command::new(&cargo);
    build
        .args(["install", "--locked", "--path"])
        .arg(clone)
        .arg("--root")
        .arg(&root);
    let built = limited(build, BUILD)
        .await
        .with_context(|| format!("run {}", cargo.display()))
        .and_then(|out| {
            if !out.status.success() {
                bail!(
                    "cargo install: {}",
                    tail(String::from_utf8_lossy(&out.stderr).trim())
                );
            }
            Ok(())
        });
    let new = root.join("bin/agent-fleet");
    let built = match built {
        Ok(()) => reports(&new, &head).await,
        err => err,
    };
    let done = built.and_then(|()| {
        // The new binary has proved nothing past `--version` yet: keep the old one reachable.
        let prev = exe.with_file_name(format!(
            "{}.prev",
            exe.file_name().unwrap_or_default().to_string_lossy()
        ));
        // No rollback copy, no update.
        keep_prev(exe, &prev, true)?;
        std::fs::rename(&new, exe).with_context(|| format!("move the build to {}", exe.display()))
    });
    clean(&root);
    done?;
    Ok(Outcome::Replaced(head))
}

/// Fails unless the build at `new` runs and its `--version` names `head`: a build of anything
/// else, or one that does not start, never replaces the binary.
async fn reports(new: &Path, head: &str) -> Result<()> {
    let mut c = tokio::process::Command::new(new);
    c.arg("--version");
    let out = limited(c, GIT)
        .await
        .with_context(|| format!("run {} --version", new.display()))?;
    let said = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !said.split_whitespace().any(|w| w == head) {
        bail!(
            "the build reports {:?}, not {head}; the old binary is kept",
            said.trim()
        );
    }
    Ok(())
}

/// Removes a build directory; a leftover is logged, `sweep` retries it later.
fn clean(root: &Path) {
    if let Err(e) = std::fs::remove_dir_all(root)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("agent-fleet host: update: remove {}: {e}", root.display());
    }
}

/// Makes `prev` the old binary: a hard link (else a copy) under a temporary name, renamed over
/// the old `prev` only once whole, so a failed copy leaves the previous rollback copy intact.
fn keep_prev(exe: &Path, prev: &Path, link: bool) -> Result<()> {
    let tmp = prev.with_file_name(format!(
        "{}.tmp-{}",
        prev.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let made = if link {
        std::fs::hard_link(exe, &tmp).or_else(|_| std::fs::copy(exe, &tmp).map(|_| ()))
    } else {
        std::fs::copy(exe, &tmp).map(|_| ())
    };
    let done = made.and_then(|()| std::fs::rename(&tmp, prev));
    // Renaming a link onto a link of the same file does nothing and leaves `tmp` behind.
    let _ = std::fs::remove_file(&tmp);
    done.map_err(|e| anyhow!("keep the old binary as {}: {e}", prev.display()))
}

/// The running binary's path. Linux names a binary replaced under it `<path> (deleted)`.
pub(crate) fn this_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map_or(exe.clone(), PathBuf::from))
}

/// The requests that ask `me` (or every host), it has not answered, still fresh enough to run.
fn pending_for(s: &State, me: &str, now: chrono::DateTime<Utc>) -> Vec<String> {
    s.updates
        .iter()
        .filter(|(_, u)| {
            u.host.as_deref().is_none_or(|h| h == me)
                && !u.results.contains_key(me)
                && now - u.since < REQUEST_MAX_AGE
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// The messages that tell the root coordinator a restart cut work off.
fn drain_warnings(s: &State, text: &str) -> Vec<MsgSent> {
    crate::watch::recipients(s, None)
        .into_iter()
        .map(|to| MsgSent {
            to,
            kind: "UPDATE".into(),
            reference: "update-drain".into(),
            text: text.to_string(),
        })
        .collect()
}

/// What a tick needs from its host, so the tick is testable with fakes.
trait Io {
    /// Requests not yet answered by this host, still fresh.
    fn pending(&self) -> Vec<String>;
    /// Whether the answer was written.
    async fn publish(&self, id: &str, r: UpdateResult) -> bool;
    /// Writes an `update.requested` that asks this host only; whether it was written.
    async fn ask(&self) -> bool;
    /// Spawn or handoff work in flight that an exit would cut short.
    fn busy(&self) -> bool;
    /// What that work is.
    fn busy_work(&self) -> Vec<String>;
    /// Tells the root coordinator.
    async fn warn(&self, text: String);
    /// One line for the host's log.
    fn note(&self, line: String);
    /// One way: no new such work starts, until the process exits.
    fn drain(&self);
    fn exit(&self, code: i32);
}

struct HostIo(Arc<Host>);

impl Io for HostIo {
    fn pending(&self) -> Vec<String> {
        match self.0.view() {
            Ok(v) => pending_for(&v.state, &self.0.cfg.host, Utc::now()),
            Err(err) => {
                eprintln!("agent-fleet host: replica: {err}");
                Vec::new()
            }
        }
    }

    async fn publish(&self, id: &str, r: UpdateResult) -> bool {
        self.0
            .publish(self.0.presence(Known::UpdateResult(r)).re(id))
            .await
    }

    async fn ask(&self) -> bool {
        let body = Known::UpdateRequested(UpdateRequested {
            host: Some(self.0.cfg.host.clone()),
        });
        self.0.publish(self.0.presence(body)).await
    }

    fn busy(&self) -> bool {
        self.0.is_busy()
    }

    fn busy_work(&self) -> Vec<String> {
        self.0.busy_work()
    }

    fn note(&self, line: String) {
        eprintln!("agent-fleet host: {line}");
    }

    async fn warn(&self, text: String) {
        let view = match self.0.view() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("agent-fleet host: update: cannot tell the coordinator: {e:#}");
                return;
            }
        };
        let msgs = drain_warnings(&view.state, &text);
        if msgs.is_empty() {
            eprintln!("agent-fleet host: update: no root coordinator to tell");
        }
        for m in msgs {
            self.0.publish(self.0.presence(Known::MsgSent(m))).await;
        }
    }

    fn drain(&self) {
        self.0.drain();
    }

    fn exit(&self, code: i32) {
        std::process::exit(code)
    }
}

/// What a host has already done: requests it ran, and answers it could not write yet.
#[derive(Default)]
struct Seen {
    done: HashSet<String>,
    unsent: BTreeMap<String, UpdateResult>,
}

/// The timer's memory.
#[derive(Default)]
struct Timer {
    last: Option<tokio::time::Instant>,
    /// The `origin/main` commit this host last asked itself to update to. A target that failed
    /// to build is not asked for again until `origin/main` moves.
    asked_for: Option<String>,
    /// Since when the fetch has found `origin/main` ahead of this binary's commit; `host.health`
    /// reports it, so `doctor` sees a fleet that agrees on an old commit.
    behind_since: Option<chrono::DateTime<Utc>>,
}

/// What a tick runs against.
struct Ctx<'a> {
    me: &'a str,
    clone: &'a Path,
    /// This binary's path; when it cannot be told, every request is answered with the error.
    exe: &'a Result<PathBuf>,
    running: Option<&'a str>,
    poll: Duration,
    drain_max: Duration,
    /// How often the timer looks at `origin/main`.
    check_every: Duration,
}

/// Executes each request this host has not answered and answers `update.result`. Once the binary
/// was replaced it stops new spawns and handoffs, waits (at most `drain_max`) for those in
/// flight, then exits with `RESTART_STATUS`. A restart whose answer was not written is not
/// retried by building again: the new process finds the request unanswered and answers it.
async fn tick<I: Io>(io: &I, c: &Ctx<'_>, seen: &mut Seen) {
    let pending = io.pending();
    // Answers that were not written are sent again as they are: the update is not run twice.
    seen.unsent.retain(|id, _| pending.contains(id));
    for (id, r) in seen.unsent.clone() {
        if io.publish(&id, r).await {
            seen.unsent.remove(&id);
        }
    }
    for id in pending {
        if !seen.done.insert(id.clone()) {
            continue;
        }
        let done = match c.exe {
            Ok(exe) => update(c.clone, exe, c.running).await,
            Err(err) => Err(anyhow!("cannot tell this binary's path: {err:#}")),
        };
        let (commit, error, restart) = match done {
            Ok(Outcome::Current(head)) => {
                io.note(format!("update: already at {}", short(Some(&head))));
                (Some(head), None, false)
            }
            Ok(Outcome::Replaced(head)) => {
                io.note(format!(
                    "update: built and installed {} (was {}); restarting once spawns and handoffs in flight end",
                    short(Some(&head)),
                    short(c.running)
                ));
                (Some(head), None, true)
            }
            // The commit it still runs: a failure leaves it as it was.
            Err(err) => {
                let why = tail(&format!("{err:#}"));
                io.note(format!("update: failed: {why}"));
                (c.running.map(String::from), Some(why), false)
            }
        };
        let result = UpdateResult {
            host: c.me.to_string(),
            ok: error.is_none(),
            commit,
            error,
        };
        let written = io.publish(&id, result.clone()).await;
        if !restart {
            if !written {
                eprintln!("agent-fleet host: update.result for {id} not written; retrying");
                seen.unsent.insert(id, result);
            }
            continue;
        }
        if !written {
            eprintln!(
                "agent-fleet host: update.result for {id} not written; the restarted host answers it"
            );
        }
        // A spawn or handoff cut off by the exit would be run again from scratch.
        io.drain();
        let deadline = tokio::time::Instant::now() + c.drain_max;
        while io.busy() {
            if tokio::time::Instant::now() >= deadline {
                let text = format!(
                    "{}: self-update restarts this host after {} min with work still running: {}. It runs again from scratch.",
                    c.me,
                    c.drain_max.as_secs() / 60,
                    io.busy_work().join(", ")
                );
                eprintln!("agent-fleet host: {text}");
                io.warn(text).await;
                break;
            }
            tokio::time::sleep(c.poll).await;
        }
        eprintln!("agent-fleet host: binary replaced; exiting for the supervisor to restart");
        io.exit(RESTART_STATUS);
        return;
    }
}

/// `origin/main` when this binary's commit is an ancestor of it and not it: the host is behind.
/// The caller holds the clone's lock for the fetch.
async fn behind(clone: &Path, running: Option<&str>) -> Result<Option<String>> {
    let Some(running) = running else {
        return Ok(None);
    };
    git_in(clone, &["fetch", "--quiet", "origin", "main"], PULL).await?;
    let target = git_in(clone, &["rev-parse", "origin/main"], GIT).await?;
    if target == running {
        return Ok(None);
    }
    // 0: ancestor (behind); 1: not (a build from somewhere else: not ours to replace).
    let mut c = tokio::process::Command::new("git");
    c.arg("-C")
        .arg(clone)
        .args(["merge-base", "--is-ancestor", running, "origin/main"]);
    let out = limited(c, GIT).await?;
    match out.status.code() {
        Some(0) => Ok(Some(target)),
        Some(1) => Ok(None),
        _ => bail!(
            "git merge-base ({}): {}",
            out.status,
            tail(String::from_utf8_lossy(&out.stderr).trim())
        ),
    }
}

/// Every `check_every`: when this host's commit is behind `origin/main`, ask this host (and no
/// other) to update, once per target. The request is run by `tick` like any other.
async fn timer<I: Io>(io: &I, c: &Ctx<'_>, t: &mut Timer) {
    let now = tokio::time::Instant::now();
    if t.last.is_some_and(|l| now - l < c.check_every) {
        return;
    }
    t.last = Some(now);
    // A request for this host is already waiting: let it run first.
    if !io.pending().is_empty() {
        return;
    }
    // Another update running means nothing to say now; what is known about being behind stays.
    let Ok(_lock) = lock(c.clone).await else {
        return;
    };
    match behind(c.clone, c.running).await {
        Ok(Some(target)) => {
            t.behind_since.get_or_insert_with(Utc::now);
            if t.asked_for.as_deref() != Some(target.as_str()) && io.ask().await {
                io.note(format!(
                    "update: running {}, origin/main is {}; asked this host to update",
                    short(c.running),
                    short(Some(&target))
                ));
                t.asked_for = Some(target);
            }
        }
        Ok(None) => t.behind_since = None,
        // A failed fetch tells nothing: the clock keeps running.
        Err(e) => eprintln!("agent-fleet host: update check: {e:#}"),
    }
}

pub async fn update_loop(host: Arc<Host>) {
    let exe = this_exe();
    match &exe {
        Ok(exe) => {
            if let Some(dir) = exe.parent() {
                sweep(dir, LEFTOVER);
            }
        }
        Err(err) => eprintln!("agent-fleet host: update: {err:#}"),
    }
    let me = host.cfg.host.clone();
    let io = HostIo(host.clone());
    let mut seen = Seen::default();
    let mut timer_state = Timer::default();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let c = Ctx {
            me: &me,
            clone: Path::new(CLONE),
            exe: &exe,
            running: COMMIT,
            poll: DRAIN_POLL,
            drain_max: DRAIN_MAX,
            check_every: CHECK_EVERY,
        };
        tick(&io, &c, &mut seen).await;
        timer(&io, &c, &mut timer_state).await;
        *host.behind_since.lock().unwrap() = timer_state.behind_since;
    }
}

/// Follows the hub until each of `hosts` answered `update.result` and reports, in
/// `host.health`, the commit it named. Fails if any refused or did not get there in time.
pub async fn wait(client: &Client, id: &str, hosts: &[String]) -> Result<()> {
    wait_with(|| client.state(), id, hosts, Duration::from_secs(2), WAIT).await
}

async fn wait_with<F, Fut>(
    fetch: F,
    id: &str,
    hosts: &[String],
    poll: Duration,
    limit: Duration,
) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<State>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    // host → whether it got there
    let mut done: BTreeMap<&str, bool> = BTreeMap::new();
    let mut last = None;
    let mut last_err: Option<String> = None;
    loop {
        match fetch().await {
            Ok(s) => {
                for h in hosts {
                    if done.contains_key(h.as_str()) {
                        continue;
                    }
                    let Some(r) = s.updates.get(id).and_then(|u| u.results.get(h)) else {
                        continue;
                    };
                    let runs = s.hosts.get(h).and_then(|x| x.commit.as_deref());
                    if !r.ok {
                        println!("{h}  refused: {}", r.error.as_deref().unwrap_or("?"));
                        done.insert(h, false);
                    } else if runs == r.commit.as_deref() {
                        println!("{h}  runs {}", short(runs));
                        done.insert(h, true);
                    }
                }
                last = Some(s);
            }
            Err(e) if e.downcast_ref::<Refused>().is_some() => {
                bail!("waiting for the update results: {e:#}");
            }
            Err(e) => {
                let e = format!("{e:#}");
                if last_err.as_deref() != Some(e.as_str()) {
                    eprintln!("waiting: the hub did not answer: {e}");
                    last_err = Some(e);
                }
            }
        }
        if done.len() == hosts.len() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            for h in hosts.iter().filter(|h| !done.contains_key(h.as_str())) {
                let s = last.as_ref();
                let runs = s
                    .and_then(|s| s.hosts.get(h))
                    .and_then(|x| x.commit.as_deref());
                match s.and_then(|s| s.updates.get(id)?.results.get(h)) {
                    None => println!("{h}  no answer (still runs {})", short(runs)),
                    Some(r) => println!(
                        "{h}  built {} but still runs {} — did its supervisor restart it?",
                        short(r.commit.as_deref()),
                        short(runs)
                    ),
                }
            }
            bail!(
                "not every host runs its new build after {} min{}",
                limit.as_secs() / 60,
                last_err
                    .map(|e| format!(" (the hub's last error: {e})"))
                    .unwrap_or_default()
            );
        }
        tokio::time::sleep(poll).await;
    }
    if done.values().any(|ok| !ok) {
        bail!("some hosts refused the update");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An origin with one commit on `main`, a clone of it, and a binary dir with a fake `cargo`
    /// that "builds" a script whose `--version` names the clone's head.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        fixture_with(&builds(r#""$(git -C "$src" rev-parse HEAD)""#))
    }

    /// A fake `cargo install` body whose build answers `--version` with `commit` (shell words).
    fn builds(commit: &str) -> String {
        format!(
            r#"while [ $# -gt 0 ]; do case $1 in --root) root=$2;; --path) src=$2;; esac; shift; done
mkdir -p "$root/bin"
printf '#!/bin/sh\necho agent-fleet 0.1.0 %s\n' {commit} > "$root/bin/agent-fleet"
chmod +x "$root/bin/agent-fleet"
"#
        )
    }

    fn fixture_with(cargo_body: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let origin = t.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        sh(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("f"), "1").unwrap();
        sh(&origin, &["add", "f"]);
        sh(&origin, &["commit", "-qm", "one"]);
        let clone = t.path().join("clone");
        sh(
            t.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        let bin = t.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let cargo = bin.join("cargo");
        std::fs::write(&cargo, format!("#!/bin/sh\n{cargo_body}")).unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exe = bin.join("agent-fleet");
        std::fs::write(&exe, "old").unwrap();
        (t, origin, clone, exe)
    }

    #[tokio::test]
    async fn a_dirty_clone_or_one_off_main_is_refused_and_the_binary_untouched() {
        let (_t, _origin, clone, exe) = fixture();
        std::fs::write(clone.join("f"), "edited").unwrap();
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("uncommitted changes"),
            "{err:#}"
        );
        sh(&clone, &["checkout", "-q", "f"]);
        sh(&clone, &["checkout", "-qb", "feature"]);
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("is on feature, not main"),
            "{err:#}"
        );
        sh(&clone, &["checkout", "-q", "--detach"]);
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(format!("{err:#}").contains("not on a branch"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[tokio::test]
    async fn it_pulls_main_builds_and_renames_the_build_over_the_binary() {
        let (_t, origin, clone, exe) = fixture();
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        // An untracked file is not a dirty clone.
        std::fs::write(clone.join("scratch"), "x").unwrap();
        assert_eq!(
            update(&clone, &exe, Some(&first)).await.unwrap(),
            Outcome::Current(first.clone()),
            "nothing new: no build"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        std::fs::write(origin.join("f"), "2").unwrap();
        sh(&origin, &["commit", "-qam", "two"]);
        let second = sh(&origin, &["rev-parse", "HEAD"]);
        assert_eq!(
            update(&clone, &exe, Some(&first)).await.unwrap(),
            Outcome::Replaced(second.clone())
        );
        assert!(std::fs::read_to_string(&exe).unwrap().contains(&second));
        let left: Vec<_> = std::fs::read_dir(exe.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 3, "cargo, the binary, its .prev: {left:?}");
        let prev = exe.with_file_name("agent-fleet.prev");
        assert_eq!(
            std::fs::read_to_string(prev).unwrap(),
            "old",
            "rollback copy"
        );
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn a_failed_cargo_keeps_the_old_binary_and_leaves_nothing() {
        let (_t, origin, clone, exe) = fixture_with("echo 'error: boom' >&2; exit 101\n");
        std::fs::write(origin.join("f"), "2").unwrap();
        sh(&origin, &["commit", "-qam", "two"]);
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("cargo install: error: boom"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        let left: Vec<_> = std::fs::read_dir(exe.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "no build dir, no .prev: {left:?}");
    }

    #[tokio::test]
    async fn a_build_reporting_another_commit_is_never_installed() {
        let (_t, origin, clone, exe) = fixture_with(&builds(&"0".repeat(40)));
        newer(&origin);
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("the old binary is kept"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        let left: Vec<_> = std::fs::read_dir(exe.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left.len(),
            2,
            "cargo and the binary, no build, no .prev: {left:?}"
        );
    }

    #[tokio::test]
    async fn a_diverged_clone_is_refused_and_the_binary_untouched() {
        let (_t, origin, clone, exe) = fixture();
        std::fs::write(clone.join("f"), "local").unwrap();
        sh(&clone, &["commit", "-qam", "local"]);
        std::fs::write(origin.join("f"), "2").unwrap();
        sh(&origin, &["commit", "-qam", "two"]);
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(format!("{err:#}").contains("git pull"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[tokio::test]
    async fn a_second_update_in_one_clone_is_refused_while_the_first_holds_the_lock() {
        let (_t, origin, clone, exe) = fixture();
        std::fs::write(origin.join("f"), "2").unwrap();
        sh(&origin, &["commit", "-qam", "two"]);
        let held = lock(&clone).await.unwrap();
        let err = update_waiting(&clone, &exe, None, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("another update is running"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        drop(held);
        assert!(update(&clone, &exe, None).await.is_ok());
    }

    #[tokio::test]
    async fn an_update_waits_for_a_sibling_in_the_clone_instead_of_failing() {
        let (_t, origin, clone, exe) = fixture();
        std::fs::write(origin.join("f"), "2").unwrap();
        sh(&origin, &["commit", "-qam", "two"]);
        let held = lock(&clone).await.unwrap();
        let (done, _) = tokio::join!(
            update_waiting(&clone, &exe, None, Duration::from_secs(10)),
            async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                drop(held);
            }
        );
        assert!(matches!(done.unwrap(), Outcome::Replaced(_)));
    }

    #[tokio::test]
    async fn a_command_past_its_limit_is_killed_with_its_children_and_never_prompts() {
        let t = tempfile::tempdir().unwrap();
        let mark = t.path().join("late");
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c")
            .arg(format!("(sleep 1; touch {}) & sleep 30", mark.display()));
        let started = std::time::Instant::now();
        let e = limited(c, Duration::from_millis(300)).await.unwrap_err();
        assert!(format!("{e}").contains("timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!mark.exists(), "the child died with the group");
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg("echo $GIT_TERMINAL_PROMPT");
        let out = limited(c, Duration::from_secs(5)).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
    }

    #[test]
    fn sweep_removes_old_build_dirs_of_dead_processes_only() {
        let t = tempfile::tempdir().unwrap();
        let dead = t.path().join(".agent-fleet-update-123456789");
        let live = t
            .path()
            .join(format!(".agent-fleet-update-{}", std::process::id()));
        std::fs::create_dir(&dead).unwrap();
        std::fs::create_dir(&live).unwrap();
        std::fs::create_dir(t.path().join("keep")).unwrap();
        sweep(t.path(), Duration::from_secs(3600));
        assert!(dead.exists(), "too young");
        sweep(t.path(), Duration::ZERO);
        assert!(!dead.exists());
        assert!(live.exists(), "its process is alive: a build in progress");
        assert!(t.path().join("keep").exists());
    }

    /// A host with the log and the exit faked.
    #[derive(Default)]
    struct Fake {
        pending: std::cell::RefCell<Vec<String>>,
        log: std::cell::RefCell<Vec<String>>,
        busy: std::cell::Cell<bool>,
        drop_publish: std::cell::Cell<bool>,
        asked: std::cell::Cell<usize>,
        ask_fails: std::cell::Cell<bool>,
        notes: std::cell::RefCell<Vec<String>>,
        results: std::cell::RefCell<Vec<UpdateResult>>,
    }

    impl Io for Fake {
        fn pending(&self) -> Vec<String> {
            self.pending.borrow().clone()
        }
        async fn publish(&self, id: &str, r: UpdateResult) -> bool {
            self.log
                .borrow_mut()
                .push(format!("publish {id} ok={}", r.ok));
            self.results.borrow_mut().push(r);
            if !self.drop_publish.get() {
                self.pending.borrow_mut().retain(|p| p != id);
            }
            !self.drop_publish.get()
        }
        async fn ask(&self) -> bool {
            self.log.borrow_mut().push("ask".into());
            if self.ask_fails.get() {
                return false;
            }
            let n = self.asked.get() + 1;
            self.asked.set(n);
            self.pending.borrow_mut().push(format!("own{n}"));
            true
        }
        fn busy(&self) -> bool {
            self.busy.get()
        }
        fn busy_work(&self) -> Vec<String> {
            vec!["spawn of agent:h/s/x".into()]
        }
        fn note(&self, line: String) {
            self.notes.borrow_mut().push(line);
        }
        async fn warn(&self, text: String) {
            self.log.borrow_mut().push(format!("warn {text}"));
        }
        fn drain(&self) {
            self.log.borrow_mut().push("drain".into());
        }
        fn exit(&self, code: i32) {
            self.log.borrow_mut().push(format!("exit {code}"));
        }
    }

    const POLL: Duration = Duration::from_millis(10);

    fn ctx<'a>(clone: &'a Path, exe: &'a Result<PathBuf>, running: Option<&'a str>) -> Ctx<'a> {
        Ctx {
            me: "h",
            clone,
            exe,
            running,
            poll: POLL,
            drain_max: Duration::from_secs(30),
            check_every: Duration::ZERO,
        }
    }

    fn newer(origin: &Path) {
        std::fs::write(origin.join("f"), "2").unwrap();
        sh(origin, &["commit", "-qam", "two"]);
    }

    #[tokio::test]
    async fn the_exit_waits_for_the_work_in_flight_and_nothing_new_starts() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        io.busy.set(true);
        let mut seen = Seen::default();
        let c = ctx(&clone, &exe, None);
        tokio::join!(tick(&io, &c, &mut seen), async {
            while !io.log.borrow().contains(&"drain".to_string()) {
                tokio::time::sleep(POLL).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                *io.log.borrow(),
                ["publish r1 ok=true", "drain"],
                "answered and draining, not gone"
            );
            io.busy.set(false);
        });
        assert_eq!(*io.log.borrow(), ["publish r1 ok=true", "drain", "exit 75"]);
    }

    #[tokio::test]
    async fn work_that_never_ends_cannot_keep_the_old_binary_running() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        io.busy.set(true);
        let c = Ctx {
            drain_max: Duration::from_millis(100),
            ..ctx(&clone, &exe, None)
        };
        tick(&io, &c, &mut Seen::default()).await;
        let log = io.log.borrow();
        assert_eq!(log.last().unwrap(), "exit 75");
        let warn = log.iter().find(|l| l.starts_with("warn ")).expect("told");
        assert!(
            warn.contains("spawn of agent:h/s/x") && warn.contains("from scratch"),
            "{warn}"
        );
    }

    #[test]
    fn the_drain_ends_before_a_spawn_request_that_arrived_with_it_expires() {
        let max = chrono::Duration::from_std(DRAIN_MAX).unwrap();
        assert!(max < crate::spawn::REQUEST_MAX_AGE);
    }

    #[test]
    fn keep_prev_is_repeatable_and_works_with_no_previous_copy() {
        let t = tempfile::tempdir().unwrap();
        let (exe, prev) = (
            t.path().join("agent-fleet"),
            t.path().join("agent-fleet.prev"),
        );
        std::fs::write(&exe, "v1").unwrap();
        keep_prev(&exe, &prev, true).unwrap();
        // A retried update links the same file again: nothing is left behind.
        keep_prev(&exe, &prev, true).unwrap();
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "v1");
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn an_update_whose_rollback_copy_cannot_be_kept_fails_before_the_rename() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        // A directory where the rollback copy goes: the rename onto it fails.
        let prev = exe.with_file_name("agent-fleet.prev");
        std::fs::create_dir(&prev).unwrap();
        std::fs::write(prev.join("x"), "x").unwrap();
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("keep the old binary"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[test]
    fn a_rollback_copy_that_cannot_be_made_leaves_the_previous_one_whole_and_no_temp() {
        let t = tempfile::tempdir().unwrap();
        let (exe, prev) = (
            t.path().join("agent-fleet"),
            t.path().join("agent-fleet.prev"),
        );
        std::fs::write(&exe, "v2").unwrap();
        std::fs::write(&prev, "v1").unwrap();
        // Link, then copy, both replace it.
        keep_prev(&exe, &prev, true).unwrap();
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "v2");
        std::fs::write(&exe, "v3").unwrap();
        std::fs::remove_file(&prev).unwrap();
        std::fs::write(&prev, "v2").unwrap();
        keep_prev(&exe, &prev, false).unwrap();
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "v3");
        // The source is gone: the copy fails and `prev` is untouched.
        std::fs::remove_file(&exe).unwrap();
        assert!(keep_prev(&exe, &prev, false).is_err());
        assert!(keep_prev(&exe, &prev, true).is_err());
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "v3");
        let left: Vec<_> = std::fs::read_dir(t.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temp left: {left:?}");
    }

    #[tokio::test]
    async fn batch_ssh_is_set_only_when_the_operator_chose_no_ssh_command() {
        let (_t, _origin, clone, _exe) = fixture();
        assert!(wants_batch_ssh(&clone, false).await);
        assert!(
            !wants_batch_ssh(&clone, true).await,
            "GIT_SSH_COMMAND is set"
        );
        let envs = |c: &tokio::process::Command| {
            c.as_std()
                .get_envs()
                .any(|(k, v)| k == "GIT_SSH_COMMAND" && v.is_some())
        };
        assert!(
            envs(&git_command(&clone, &["status"]).await),
            "wired into the command"
        );
        sh(&clone, &["config", "core.sshCommand", "ssh -i /k"]);
        assert!(
            !wants_batch_ssh(&clone, false).await,
            "core.sshCommand is set"
        );
        assert!(!envs(&git_command(&clone, &["status"]).await));
        // An unreadable clone is not "unset": the operator's setting is not overridden.
        assert!(!wants_batch_ssh(Path::new("/nonexistent/clone"), false).await);
    }

    #[test]
    fn the_root_coordinator_is_told_and_nobody_when_there_is_none() {
        let now = Utc::now();
        let agent = || crate::state::Agent {
            host: "h".into(),
            kind: None,
            pane: None,
            state: crate::event::AgentState::Working,
            since: now,
            worked: None,
            finished: None,
            usage: None,
        };
        let mut s = State::default();
        assert!(drain_warnings(&s, "x").is_empty());
        s.agents.insert("agent:h/s/root".into(), agent());
        s.roles.insert(
            "agent:h/s/root".into(),
            crate::state::Role {
                level: crate::event::Level::RootCoordinator,
                lead: None,
                domain: None,
                by: "agent:h/s/root".into(),
                since: now,
            },
        );
        let m = drain_warnings(&s, "cut off");
        assert_eq!(m.len(), 1);
        assert_eq!(
            (
                m[0].to.as_str(),
                m[0].kind.as_str(),
                m[0].reference.as_str(),
                m[0].text.as_str()
            ),
            ("agent:h/s/root", "UPDATE", "update-drain", "cut off")
        );
    }

    #[tokio::test]
    async fn a_replaced_binary_restarts_even_if_the_answer_was_not_written() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        io.drop_publish.set(true);
        *io.pending.borrow_mut() = ids(&["r1"]);
        tick(&io, &ctx(&clone, &exe, None), &mut Seen::default()).await;
        // The new process finds the request unanswered and answers it: no second build here.
        assert_eq!(*io.log.borrow(), ["publish r1 ok=true", "drain", "exit 75"]);
    }

    #[tokio::test]
    async fn an_answer_that_was_not_written_is_retried_and_a_request_runs_once() {
        let (_t, _origin, clone, exe) = fixture();
        let head = sh(&clone, &["rev-parse", "HEAD"]);
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        io.drop_publish.set(true);
        let mut seen = Seen::default();
        let c = ctx(&clone, &exe, Some(&head));
        tick(&io, &c, &mut seen).await;
        tick(&io, &c, &mut seen).await;
        io.drop_publish.set(false);
        tick(&io, &c, &mut seen).await;
        tick(&io, &c, &mut seen).await;
        assert_eq!(
            *io.log.borrow(),
            ["publish r1 ok=true"; 3],
            "retried until written, then once"
        );
    }

    #[tokio::test]
    async fn an_unwritten_answer_is_sent_again_without_running_the_update_again() {
        let t = tempfile::tempdir().unwrap();
        let builds = t.path().join("builds");
        let (_f, origin, clone, exe) = fixture_with(&format!(
            "echo x >> {}; echo 'error: boom' >&2; exit 1\n",
            builds.display()
        ));
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        io.drop_publish.set(true);
        let (mut seen, c) = (Seen::default(), ctx(&clone, &exe, None));
        tick(&io, &c, &mut seen).await;
        tick(&io, &c, &mut seen).await;
        io.drop_publish.set(false);
        tick(&io, &c, &mut seen).await;
        tick(&io, &c, &mut seen).await;
        assert_eq!(std::fs::read_to_string(&builds).unwrap().lines().count(), 1);
        assert_eq!(
            io.results.borrow().len(),
            3,
            "sent until written, then no more"
        );
        assert!(
            io.results.borrow()[2]
                .error
                .as_deref()
                .unwrap()
                .contains("boom")
        );
    }

    #[tokio::test]
    async fn the_timer_asks_once_per_target_only_while_behind_origin_main() {
        let (_t, origin, clone, exe) = fixture();
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        let exe = Ok(exe);
        let io = Fake::default();
        let (mut t, running) = (Timer::default(), Some(first.as_str()));
        let c = ctx(&clone, &exe, running);
        timer(&io, &c, &mut t).await;
        assert!(io.log.borrow().is_empty(), "up to date: no request");
        assert!(t.behind_since.is_none());
        newer(&origin);
        timer(&io, &c, &mut t).await;
        assert_eq!(*io.log.borrow(), ["ask"], "behind: one request");
        let since = t.behind_since.expect("behind since the fetch saw it");
        // The request is answered; still behind (it built nothing, say): not asked again.
        io.pending.borrow_mut().clear();
        timer(&io, &c, &mut t).await;
        assert_eq!(io.log.borrow().len(), 1, "same target: once");
        sh(&origin, &["commit", "-q", "--allow-empty", "-m", "three"]);
        timer(&io, &c, &mut t).await;
        assert_eq!(io.log.borrow().len(), 2, "origin/main moved: asked again");
        assert_eq!(t.behind_since, Some(since), "behind since the first time");
        // origin/main went back (a revert, say) to the commit this binary runs: not behind.
        sh(&origin, &["reset", "-q", "--hard", &first]);
        io.pending.borrow_mut().clear();
        timer(&io, &c, &mut t).await;
        assert!(t.behind_since.is_none(), "on origin/main again");
    }

    #[tokio::test]
    async fn the_timer_waits_for_its_interval_and_for_a_request_already_pending() {
        let (_t, origin, clone, exe) = fixture();
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        let mut t = Timer::default();
        let slow = Ctx {
            check_every: Duration::from_secs(3600),
            ..ctx(&clone, &exe, Some(&first))
        };
        *io.pending.borrow_mut() = ids(&["other"]);
        timer(&io, &slow, &mut t).await;
        assert!(io.log.borrow().is_empty(), "a request is waiting");
        io.pending.borrow_mut().clear();
        timer(&io, &slow, &mut t).await;
        assert!(io.log.borrow().is_empty(), "the interval has not passed");
    }

    #[tokio::test]
    async fn the_timer_leaves_a_build_from_elsewhere_and_an_unknown_commit_alone() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        for running in [Some("0123456789abcdef0123456789abcdef01234567"), None] {
            let mut t = Timer::default();
            timer(&io, &ctx(&clone, &exe, running), &mut t).await;
        }
        assert!(io.log.borrow().is_empty());
    }

    #[tokio::test]
    async fn the_timer_does_not_replace_a_diverged_build_or_one_ahead_of_origin() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        sh(&clone, &["commit", "-q", "--allow-empty", "-m", "local"]);
        let local = sh(&clone, &["rev-parse", "HEAD"]);
        let exe = Ok(exe);
        let io = Fake::default();
        // A real commit that is not an ancestor of origin/main: not ours to replace.
        let mut t = Timer::default();
        assert!(behind(&clone, Some(&local)).await.unwrap().is_none());
        timer(&io, &ctx(&clone, &exe, Some(&local)), &mut t).await;
        assert!(io.log.borrow().is_empty());
        // An unknown commit is an error, not a request.
        assert!(behind(&clone, Some(&"0".repeat(40))).await.is_err());
    }

    #[tokio::test]
    async fn a_request_that_was_not_written_is_asked_again_next_interval() {
        let (_t, origin, clone, exe) = fixture();
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        io.ask_fails.set(true);
        let (mut t, c) = (Timer::default(), ctx(&clone, &exe, Some(&first)));
        timer(&io, &c, &mut t).await;
        io.ask_fails.set(false);
        timer(&io, &c, &mut t).await;
        assert_eq!(*io.log.borrow(), ["ask", "ask"]);
        assert_eq!(t.asked_for.as_deref().map(str::len), Some(40));
    }

    #[tokio::test]
    async fn the_timer_asks_nothing_while_an_update_holds_the_lock_or_origin_is_unreachable() {
        let (_t, origin, clone, exe) = fixture();
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        let c = ctx(&clone, &exe, Some(&first));
        let since = Utc::now() - chrono::Duration::hours(1);
        let mut t = Timer {
            behind_since: Some(since),
            ..Timer::default()
        };
        let held = lock(&clone).await.unwrap();
        timer(&io, &c, &mut t).await;
        assert!(io.log.borrow().is_empty(), "an update is running");
        drop(held);
        sh(&clone, &["remote", "set-url", "origin", "/nonexistent"]);
        t.last = None;
        timer(&io, &c, &mut t).await;
        assert!(
            io.log.borrow().is_empty(),
            "origin unreachable: logged, no request"
        );
        assert_eq!(t.behind_since, Some(since), "neither resets the clock");
    }

    #[tokio::test]
    async fn a_target_that_failed_to_build_is_not_asked_for_again_until_main_moves() {
        let t = tempfile::tempdir().unwrap();
        let builds = t.path().join("builds");
        let (_f, origin, clone, exe) = fixture_with(&format!(
            "echo x >> {}; echo 'error: boom' >&2; exit 1\n",
            builds.display()
        ));
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        newer(&origin);
        let exe = Ok(exe);
        let io = Fake::default();
        let (mut seen, mut tm) = (Seen::default(), Timer::default());
        let c = ctx(&clone, &exe, Some(&first));
        for _ in 0..3 {
            timer(&io, &c, &mut tm).await;
            tick(&io, &c, &mut seen).await;
        }
        let built = |n| std::fs::read_to_string(&builds).unwrap().lines().count() == n;
        assert!(built(1), "built once");
        assert_eq!(io.log.borrow().iter().filter(|l| *l == "ask").count(), 1);
        sh(&origin, &["commit", "-q", "--allow-empty", "-m", "three"]);
        timer(&io, &c, &mut tm).await;
        tick(&io, &c, &mut seen).await;
        assert!(built(2), "origin/main moved: tried again");
    }

    #[test]
    fn a_request_for_one_host_is_pending_for_that_host_only() {
        let now = Utc::now();
        let mut s = state_with(&[], &[]);
        s.updates.clear();
        let ask = |host: Option<&str>| crate::state::Update {
            by: "host:x".into(),
            since: now,
            host: host.map(String::from),
            results: BTreeMap::new(),
        };
        s.updates.insert("all".into(), ask(None));
        s.updates.insert("mine".into(), ask(Some("h")));
        s.updates.insert("yours".into(), ask(Some("other")));
        assert_eq!(pending_for(&s, "h", now), ["all", "mine"]);
    }

    #[tokio::test]
    async fn a_failed_update_answers_with_the_commit_the_host_still_runs() {
        let (_t, _origin, clone, exe) = fixture();
        std::fs::write(clone.join("f"), "dirty").unwrap();
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        tick(
            &io,
            &ctx(&clone, &exe, Some("abc1234")),
            &mut Seen::default(),
        )
        .await;
        let r = &io.results.borrow()[0];
        assert!(!r.ok);
        assert_eq!(r.commit.as_deref(), Some("abc1234"));
    }

    #[tokio::test]
    async fn the_host_logs_when_the_timer_asks_and_when_an_update_ends() {
        let (_t, origin, clone, exe) = fixture();
        let first = sh(&clone, &["rev-parse", "HEAD"]);
        let exe = Ok(exe);
        let io = Fake::default();
        let c = ctx(&clone, &exe, Some(&first));
        let mut t = Timer::default();
        // Up to date: the check says nothing, the update says it is current.
        timer(&io, &c, &mut t).await;
        *io.pending.borrow_mut() = ids(&["r0"]);
        tick(&io, &c, &mut Seen::default()).await;
        assert_eq!(io.notes.borrow().len(), 1);
        assert!(
            io.notes.borrow()[0].starts_with("update: already at "),
            "{:?}",
            io.notes
        );
        // Behind: the timer logs that it asked, with both commits.
        io.notes.borrow_mut().clear();
        newer(&origin);
        timer(&io, &c, &mut t).await;
        let notes = io.notes.borrow().clone();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(&first[..7]) && notes[0].contains("asked this host to update"),
            "{notes:?}"
        );
        // Its request runs: built and installed, then restarting.
        io.notes.borrow_mut().clear();
        tick(&io, &c, &mut Seen::default()).await;
        let notes = io.notes.borrow().clone();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("built and installed") && notes[0].contains(&first[..7]),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_update_is_logged_with_its_cause() {
        let (_t, _origin, clone, exe) = fixture();
        std::fs::write(clone.join("f"), "dirty").unwrap();
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        tick(&io, &ctx(&clone, &exe, None), &mut Seen::default()).await;
        let notes = io.notes.borrow();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("update: failed: ") && notes[0].contains("uncommitted"),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_update_never_exits() {
        let (_t, _origin, clone, exe) = fixture();
        std::fs::write(clone.join("f"), "dirty").unwrap();
        let exe = Ok(exe);
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r2"]);
        tick(&io, &ctx(&clone, &exe, None), &mut Seen::default()).await;
        assert_eq!(*io.log.borrow(), ["publish r2 ok=false"]);
        assert!(
            io.results.borrow()[0]
                .error
                .as_deref()
                .unwrap()
                .contains("uncommitted")
        );
    }

    #[tokio::test]
    async fn a_binary_whose_path_is_unknown_still_answers_the_request() {
        let (_t, _origin, clone, _exe) = fixture();
        let exe: Result<PathBuf> = Err(anyhow!("no /proc"));
        let io = Fake::default();
        *io.pending.borrow_mut() = ids(&["r1"]);
        tick(&io, &ctx(&clone, &exe, None), &mut Seen::default()).await;
        assert_eq!(*io.log.borrow(), ["publish r1 ok=false"]);
        assert!(
            io.results.borrow()[0]
                .error
                .as_deref()
                .unwrap()
                .contains("no /proc")
        );
    }

    #[test]
    fn only_fresh_requests_this_host_has_not_answered_are_pending() {
        let now = Utc::now();
        let mut s = state_with(&[], &[]);
        let fresh = |results: &[&str]| crate::state::Update {
            by: "host:x".into(),
            since: now,
            host: None,
            results: results
                .iter()
                .map(|h| {
                    (
                        h.to_string(),
                        UpdateResult {
                            host: h.to_string(),
                            ok: true,
                            commit: None,
                            error: None,
                        },
                    )
                })
                .collect(),
        };
        s.updates.clear();
        s.updates.insert("fresh".into(), fresh(&[]));
        s.updates.insert("mine".into(), fresh(&["h"]));
        s.updates.insert("theirs".into(), fresh(&["other"]));
        let mut stale = fresh(&[]);
        stale.since = now - REQUEST_MAX_AGE - chrono::Duration::seconds(1);
        s.updates.insert("stale".into(), stale);
        assert_eq!(pending_for(&s, "h", now), ["fresh", "theirs"]);
    }

    #[tokio::test]
    async fn the_second_update_keeps_the_build_before_it_as_prev() {
        let (_t, origin, clone, exe) = fixture();
        newer(&origin);
        assert!(update(&clone, &exe, None).await.is_ok());
        std::fs::write(&exe, "first").unwrap();
        sh(&origin, &["commit", "-q", "--allow-empty", "-m", "three"]);
        assert!(update(&clone, &exe, None).await.is_ok());
        let prev = exe.with_file_name("agent-fleet.prev");
        assert_eq!(std::fs::read_to_string(prev).unwrap(), "first");
    }

    fn state_with(hosts: &[(&str, Option<&str>)], results: &[(&str, bool, Option<&str>)]) -> State {
        let mut s = State::default();
        let ts = Utc::now();
        for (h, commit) in hosts {
            s.hosts.insert(
                h.to_string(),
                crate::state::Host {
                    disk_free: 0,
                    disk_total: 0,
                    agents: 0,
                    commit: commit.map(String::from),
                    behind_since: None,
                    ts,
                    started: None,
                },
            );
        }
        let mut u = crate::state::Update {
            by: "host:x".into(),
            since: ts,
            host: None,
            results: BTreeMap::new(),
        };
        for (h, ok, commit) in results {
            u.results.insert(
                h.to_string(),
                UpdateResult {
                    host: h.to_string(),
                    ok: *ok,
                    commit: commit.map(String::from),
                    error: (!ok).then(|| "no".to_string()),
                },
            );
        }
        s.updates.insert("r1".into(), u);
        s
    }

    async fn waited(s: State, hosts: &[&str], limit: Duration) -> Result<()> {
        let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
        wait_with(|| async { Ok(s.clone()) }, "r1", &hosts, POLL, limit).await
    }

    #[tokio::test]
    async fn wait_survives_a_hub_that_fails_and_names_it_when_it_gives_up() {
        let ok = state_with(&[("a", Some("abc"))], &[("a", true, Some("abc"))]);
        let calls = std::cell::Cell::new(0);
        let hosts = ids(&["a"]);
        wait_with(
            || {
                calls.set(calls.get() + 1);
                let r = if calls.get() < 3 {
                    Err(anyhow!("hub down"))
                } else {
                    Ok(ok.clone())
                };
                async move { r }
            },
            "r1",
            &hosts,
            POLL,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let e = wait_with(
            || async { Err(anyhow!("hub down")) },
            "r1",
            &hosts,
            POLL,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(format!("{e}").contains("hub down"), "{e}");
    }

    #[tokio::test]
    async fn wait_ends_when_every_host_runs_the_commit_it_built() {
        let s = state_with(
            &[("a", Some("abc")), ("b", Some("abc"))],
            &[("a", true, Some("abc")), ("b", true, Some("abc"))],
        );
        waited(s, &["a", "b"], Duration::from_secs(5))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wait_reports_a_failure_that_names_the_commit_the_host_runs_as_refused() {
        let s = state_with(&[("a", Some("abc"))], &[("a", false, Some("abc"))]);
        let e = waited(s, &["a"], Duration::from_secs(5)).await.unwrap_err();
        assert!(format!("{e}").contains("refused"), "{e}");
    }

    #[tokio::test]
    async fn wait_fails_on_a_refusal_and_on_a_host_that_never_restarts() {
        let refused = state_with(&[("a", Some("old"))], &[("a", false, None)]);
        let e = waited(refused, &["a"], Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{e}").contains("refused"), "{e}");
        // Built `new`, still reports `old`: no supervisor restart.
        let stuck = state_with(&[("a", Some("old"))], &[("a", true, Some("new"))]);
        let e = waited(stuck, &["a"], Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{e}").contains("not every host"), "{e}");
        let silent = state_with(&[("a", Some("old"))], &[]);
        assert!(
            waited(silent, &["a"], Duration::from_millis(100))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_replaced_binary_must_run_and_exit_zero_to_be_followed() {
        let (_t, _origin, _clone, exe) = fixture();
        let bin = exe.with_file_name("probe");
        let put = |body: &str| {
            std::fs::write(&bin, body).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        put("#!/bin/sh\necho 'agent-fleet 0.1.0 (abc1234)'\n");
        assert_eq!(
            binary_says(&bin).await.unwrap(),
            "agent-fleet 0.1.0 (abc1234)"
        );
        put("#!/bin/sh\necho boom; exit 3\n");
        assert!(binary_says(&bin).await.is_err(), "exits non-zero");
        put("");
        assert!(
            binary_says(&bin).await.is_err(),
            "empty: exits 0 but says nothing"
        );
        put("#!/bin/sh\necho hello\n");
        assert!(binary_says(&bin).await.is_err(), "not this program");
        assert!(binary_says(&exe.with_file_name("missing")).await.is_err());
    }

    #[tokio::test]
    async fn a_build_of_an_older_commit_is_a_rollback_and_nothing_else_is() {
        let (_t, _origin, clone, _exe) = fixture();
        let old = sh(&clone, &["rev-parse", "HEAD"]);
        sh(&clone, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let new = sh(&clone, &["rev-parse", "HEAD"]);
        let says = |c: &str| format!("agent-fleet 0.1.0 ({})", &c[..7]);
        let rb = |running: Option<&str>, says: &str| {
            let (clone, running, says) =
                (clone.clone(), running.map(String::from), says.to_string());
            async move { is_rollback(&clone, running.as_deref(), &says).await }
        };
        assert!(
            rb(Some(&new), &says(&old)).await,
            "older than the running one"
        );
        assert!(!rb(Some(&old), &says(&new)).await, "newer: an update");
        assert!(!rb(Some(&new), &says(&new)).await, "the same commit");
        assert!(
            !rb(Some(&new), "agent-fleet 0.1.0 (0123456)").await,
            "unknown to git: not a rollback"
        );
        assert!(!rb(None, &says(&old)).await, "running commit unknown");
        assert!(
            !rb(Some(&new), "agent-fleet 0.1.0").await,
            "no commit in the line"
        );
        assert!(!rb(Some(&new), "agent-fleet 0.1.0 (unknown)").await);
        assert!(
            !is_rollback(Path::new("/nonexistent"), Some(&new), &says(&old)).await,
            "no clone"
        );
    }

    #[test]
    fn short_never_panics_on_odd_input() {
        assert_eq!(short(Some("0123456789abcdef")), "0123456");
        assert_eq!(short(Some("abc")), "abc");
        assert_eq!(short(Some("ééééé")), "ééééé");
        assert_eq!(short(None), "unknown");
    }
}
