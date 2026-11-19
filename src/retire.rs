//! Retiring a worktree: the one guarded path every close takes (`agent-fleet retire`, a closed
//! pane, a failed spawn).
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

/// The repository-wide lock file, in the git common dir. A repo's own retire may take it too.
pub const REPO_LOCK: &str = "agent-fleet-retire.lock";
/// The variable naming the fd of B the hook inherits.
pub const LOCK_FD_ENV: &str = "AGENT_FLEET_RETIRE_LOCK_FD";
/// How long a retire waits for another to finish with the same tree or repository.
pub const WAIT: Duration = Duration::from_secs(90);

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
/// ONE binary patch in `dir`, written before anything is removed. Err, and nothing written, for
/// what a patch cannot carry: work inside a submodule, an untracked nested repository, an
/// untracked file git cannot diff. A stash that leaves work out is a delete.
pub fn save_patch(wt: &Path, dir: &Path, name: &str, changes: &[Change]) -> Result<PathBuf> {
    if let Some(c) = changes.iter().find(|c| is_gitlink(wt, &c.path)) {
        bail!(
            "{} holds work inside the submodule, which a patch cannot carry",
            c.path
        );
    }
    let mut patch = git(
        wt,
        &["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--binary"],
    )
    .context("git diff HEAD failed")?;
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
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--no-index",
                "--",
                "/dev/null",
                f,
            ])
            .output()
            .with_context(|| format!("diff {f}"))?;
        if o.status.code() != Some(1) || o.stdout.is_empty() {
            bail!("could not diff the untracked {f}");
        }
        patch += &String::from_utf8_lossy(&o.stdout);
    }
    if patch.trim().is_empty() {
        bail!("the patch came out empty");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let file = dir.join(format!("{}-{stamp}.patch", name.replace('/', "-")));
    std::fs::write(&file, patch).with_context(|| format!("write {}", file.display()))?;
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

/// (A) this worktree's lock, then (B) the repository's. Never the other order, and nothing takes
/// A while holding B, so no two retires can wait on each other.
pub async fn locks(checkout: &Path, wt: &Path, wait: Duration) -> Result<(Held, Held)> {
    let common = git(
        checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .context("git could not name the common dir, where the retire locks live")?;
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

/// The guarded retire. Locks, then reads the tree, then acts on what it found and on `decide`;
/// the hook runs only for a clean tree or a decision that accepted the changes, and is then told
/// so with `--decide=delete` (the dirt, nothing else: its own guards still stand).
pub async fn guarded(plan: &Plan<'_>, wt: &str, decide: Option<Decide>) -> Result<Outcome> {
    let held = locks(plan.checkout, Path::new(wt), WAIT).await?;
    guarded_with(plan, wt, decide, held).await
}

/// `guarded` for a caller that took the locks itself, to do something under them first —
/// `agent-fleet retire` closes the agent's pane there, so the host's own close-retire waits.
pub async fn guarded_with(
    plan: &Plan<'_>,
    wt: &str,
    decide: Option<Decide>,
    held: (Held, Held),
) -> Result<Outcome> {
    let tree = Path::new(wt);
    let (_a, b) = held;
    if !tree.exists() {
        return Ok(Outcome::Gone);
    }
    let branch = git(tree, &["symbolic-ref", "-q", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // A tree with its own `.git` is a worktree and is read, and one git cannot read is refused;
    // a directory without one holds no git work to lose, and the hook alone decides it.
    let found = if tree.join(".git").exists() {
        match changes(tree, plan.base, &branch) {
            Ok(c) => c,
            Err(e) => return Ok(Outcome::Refused(format!("{e:#}; nothing removed"))),
        }
    } else {
        vec![]
    };
    if decide == Some(Decide::Keep) {
        return Ok(Outcome::Kept);
    }
    let mut extra: Vec<&str> = vec![];
    if !found.is_empty() {
        match decide {
            None => return Ok(Outcome::Refused(report(tree, &found, plan.decide_cmd))),
            Some(Decide::Stash) => {
                let name = if branch.is_empty() {
                    tree.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                } else {
                    branch.clone()
                };
                match save_patch(tree, plan.patches, &name, &found) {
                    Ok(f) => println!("stashed {}'s uncommitted changes in {}", wt, f.display()),
                    Err(e) => {
                        return Ok(Outcome::Refused(format!(
                            "could not stash {wt}: {e:#}; nothing removed"
                        )));
                    }
                }
            }
            Some(Decide::Delete) | Some(Decide::Keep) => {}
        }
        extra.push("--decide=delete");
    }
    run_hook(plan.checkout, wt, plan.script, &extra, &b).await?;
    Ok(Outcome::Retired)
}

#[cfg(test)]
mod tests {
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
                "#!/bin/sh\necho \"$@\" >> {log}/hook-args\n[ -n \"${LOCK_FD_ENV}\" ] && [ -e \"/dev/fd/${LOCK_FD_ENV}\" ] && echo inherited >> {log}/hook-fd\nsleep \"${{HOOK_SLEEP:-0}}\"\ngit worktree remove --force \"$1\"\n",
                log = root.display()
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
            guarded(&f.plan(), f.wt(), None).await.unwrap(),
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
        let Outcome::Refused(why) = guarded(&f.plan(), f.wt(), None).await.unwrap() else {
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
            guarded(&f.plan(), f.wt(), Some(Decide::Keep))
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
            guarded(&f.plan(), f.wt(), Some(Decide::Delete))
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
            guarded(&f.plan(), f.wt(), Some(Decide::Stash))
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
        let Outcome::Refused(why) = guarded(&f.plan(), f.wt(), Some(Decide::Stash))
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
        let Outcome::Refused(why) = guarded(&f.plan(), f.wt(), Some(Decide::Delete))
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
        let f = fx();
        // SAFETY of the env write: tests in this module do not read HOOK_SLEEP concurrently
        // with other env writers; the hook reads it through its own environment.
        unsafe { std::env::set_var("HOOK_SLEEP", "1") };
        let (p1, p2) = (f.plan(), f.plan());
        let both = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(guarded(&p1, f.wt(), None), guarded(&p2, f.wt(), None))
        })
        .await
        .expect("a retire hung");
        unsafe { std::env::remove_var("HOOK_SLEEP") };
        let mut got = vec![both.0.unwrap(), both.1.unwrap()];
        got.sort_by_key(|o| format!("{o:?}"));
        assert_eq!(got, vec![Outcome::Gone, Outcome::Retired]);
        assert_eq!(f.read("hook-args").lines().count(), 1, "the hook ran twice");
    }

    #[tokio::test]
    async fn a_tree_already_gone_runs_no_hook() {
        let f = fx();
        sh(&f.co, &["worktree", "remove", f.wt()]);
        assert_eq!(
            guarded(&f.plan(), f.wt(), None).await.unwrap(),
            Outcome::Gone
        );
        assert_eq!(f.read("hook-args"), "");
    }
}
