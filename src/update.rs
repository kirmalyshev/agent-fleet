//! `agent-fleet self-update`: each host pulls the clone its binary was built from, rebuilds into
//! its own binary's place and exits, so its supervisor starts the new one.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;

use crate::client::{Client, Refused};
use crate::event::{Known, UpdateResult};
use crate::host::Host;
use crate::spawn::tail;
use crate::state::State;

/// The commit this binary was built from (`build.rs`); `None` outside a git checkout.
pub const COMMIT: Option<&str> = option_env!("AGENT_FLEET_COMMIT");
/// The checkout this binary was built from: `cargo install --path <clone>` compiles it in place.
const CLONE: &str = env!("CARGO_MANIFEST_DIR");
/// Exit status after a replace: non-zero, so `Restart=on-failure` and launchd's `KeepAlive`
/// (even with `SuccessfulExit=false`) both start the new binary.
pub const RESTART_STATUS: i32 = 75;
/// A request this old when the host first sees it is ignored: a host down at the time does not
/// rebuild by surprise later.
const REQUEST_MAX_AGE: chrono::Duration = chrono::Duration::minutes(15);
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
const BUILD: Duration = Duration::from_secs(30 * 60);
/// A `.agent-fleet-update-<pid>` untouched this long (more than `BUILD`) is assumed to be the
/// leftover of a killed update, not another host's build in progress.
const LEFTOVER: Duration = Duration::from_secs(45 * 60);
/// How long a finished update waits for spawns and handoffs in flight (a ready script may run
/// 15 min) before it exits anyway: a hung one must not keep the old binary running for good.
const DRAIN_MAX: Duration = Duration::from_secs(20 * 60);
/// How often a finished update looks for the work in flight to end.
const DRAIN_POLL: Duration = Duration::from_secs(1);

/// Runs `c` to the end, or kills its whole process group after `limit` (cargo and git start
/// children of their own). No stdin and no git prompt (the environment applies to cargo too):
/// under a supervisor nobody can answer.
async fn limited(mut c: tokio::process::Command, limit: Duration) -> Result<std::process::Output> {
    let child = c
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
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

async fn git_in(clone: &Path, args: &[&str], limit: Duration) -> Result<String> {
    let mut c = tokio::process::Command::new("git");
    c.arg("-C").arg(clone).args(args);
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
/// machine never race on `git pull` or both build. The second is refused, not queued. The file
/// is only held, never read: dropping it releases the lock.
struct Lock(#[allow(dead_code)] std::fs::File);

async fn lock(clone: &Path) -> Result<Lock> {
    use rustix::fs::{FlockOperation, flock};
    let dir = git_in(clone, &["rev-parse", "--absolute-git-dir"], GIT).await?;
    let path = PathBuf::from(dir).join("agent-fleet-update.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match flock(&f, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Lock(f)),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
            bail!("another update is running in {}", clone.display())
        }
        Err(e) => bail!("lock {}: {e}", path.display()),
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
    // First: the checks below must hold while this update pulls.
    let _lock = lock(clone).await?;
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
        .with_context(|| format!("run {}", cargo.display()));
    let done = built.and_then(|out| {
        if !out.status.success() {
            return Err(anyhow!(
                "cargo install: {}",
                tail(String::from_utf8_lossy(&out.stderr).trim())
            ));
        }
        // The new binary has proved nothing yet: keep the old one reachable.
        let prev = exe.with_file_name(format!(
            "{}.prev",
            exe.file_name().unwrap_or_default().to_string_lossy()
        ));
        match std::fs::remove_file(&prev) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("remove the old {}: {e}", prev.display())),
        }
        // No rollback copy, no update: a link, else a copy.
        if std::fs::hard_link(exe, &prev).is_err() {
            std::fs::copy(exe, &prev)
                .with_context(|| format!("keep the old binary as {}", prev.display()))?;
        }
        std::fs::rename(root.join("bin/agent-fleet"), exe)
            .with_context(|| format!("move the build to {}", exe.display()))
    });
    clean(&root);
    done?;
    Ok(Outcome::Replaced(head))
}

/// Removes a build directory; a leftover is logged, `sweep` retries it later.
fn clean(root: &Path) {
    if let Err(e) = std::fs::remove_dir_all(root)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("agent-fleet host: update: remove {}: {e}", root.display());
    }
}

/// The running binary's path. Linux names a binary replaced under it `<path> (deleted)`.
fn this_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map_or(exe.clone(), PathBuf::from))
}

/// The requests `me` has not answered, still fresh enough to run.
fn pending_for(s: &State, me: &str, now: chrono::DateTime<Utc>) -> Vec<String> {
    s.updates
        .iter()
        .filter(|(_, u)| !u.results.contains_key(me) && now - u.since < REQUEST_MAX_AGE)
        .map(|(id, _)| id.clone())
        .collect()
}

/// What a tick needs from its host, so the tick is testable with fakes.
trait Io {
    /// Requests not yet answered by this host, still fresh.
    fn pending(&self) -> Vec<String>;
    /// Whether the answer was written.
    async fn publish(&self, id: &str, r: UpdateResult) -> bool;
    /// Spawn or handoff work in flight that an exit would cut short.
    fn busy(&self) -> bool;
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

    fn busy(&self) -> bool {
        self.0.is_busy()
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

/// What a tick runs against.
struct Ctx<'a> {
    me: &'a str,
    clone: &'a Path,
    /// This binary's path; when it cannot be told, every request is answered with the error.
    exe: &'a Result<PathBuf>,
    running: Option<&'a str>,
    poll: Duration,
    drain_max: Duration,
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
            Ok(Outcome::Current(c)) => (Some(c), None, false),
            Ok(Outcome::Replaced(c)) => (Some(c), None, true),
            Err(err) => (None, Some(tail(&format!("{err:#}"))), false),
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
                eprintln!(
                    "agent-fleet host: spawn or handoff still running after {} min; restarting anyway",
                    c.drain_max.as_secs() / 60
                );
                break;
            }
            tokio::time::sleep(c.poll).await;
        }
        eprintln!("agent-fleet host: binary replaced; exiting for the supervisor to restart");
        io.exit(RESTART_STATUS);
        return;
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
    let io = HostIo(host);
    let mut seen = Seen::default();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let c = Ctx {
            me: &me,
            clone: Path::new(CLONE),
            exe: &exe,
            running: COMMIT,
            poll: DRAIN_POLL,
            drain_max: DRAIN_MAX,
        };
        tick(&io, &c, &mut seen).await;
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
    /// that "builds" a script printing `new`.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        fixture_with(
            "while [ $# -gt 0 ]; do [ \"$1\" = --root ] && root=$2; shift; done\n\
             mkdir -p \"$root/bin\" && echo new > \"$root/bin/agent-fleet\"\n",
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
            Outcome::Replaced(second)
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap().trim(), "new");
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
        let err = update(&clone, &exe, None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("another update is running"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        drop(held);
        assert!(update(&clone, &exe, None).await.is_ok());
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
        c.arg("-c")
            .arg("echo $GIT_TERMINAL_PROMPT $GIT_SSH_COMMAND");
        let out = limited(c, Duration::from_secs(5)).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "0 ssh -o BatchMode=yes"
        );
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
            !self.drop_publish.get()
        }
        fn busy(&self) -> bool {
            self.busy.get()
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
        assert_eq!(io.log.borrow().last().unwrap(), "exit 75");
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
                    ts,
                },
            );
        }
        let mut u = crate::state::Update {
            by: "host:x".into(),
            since: ts,
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

    #[test]
    fn short_never_panics_on_odd_input() {
        assert_eq!(short(Some("0123456789abcdef")), "0123456");
        assert_eq!(short(Some("abc")), "abc");
        assert_eq!(short(Some("ééééé")), "ééééé");
        assert_eq!(short(None), "unknown");
    }
}
