//! Self-update: keep the installed binary at `origin/main` of the host's checkout.
//!
//! The decision is pure (`decide`). The rest shells out to git and the build: pull only a clean,
//! `main`, fast-forwardable checkout; build; check the new binary reports the commit; then
//! rename it over the installed one. Overwriting in place kills the running process on macOS
//! (`OS_REASON_CODESIGNING`), and a rename keeps the old binary until the new one is whole.
//! The old binary stays as `<name>.prev`: to roll back, rename it over the installed one and
//! restart.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::event::{HostUpdate, Known, UpdateStatus};

/// How often a host checks.
const EVERY: Duration = Duration::from_secs(300);
/// A fetch may wait on the network; every other git call is local.
const GIT_NET: Duration = Duration::from_secs(120);
const GIT_LOCAL: Duration = Duration::from_secs(60);
const BUILD: Duration = Duration::from_secs(30 * 60);
const VERSION: Duration = Duration::from_secs(10);
const RESTART: Duration = Duration::from_secs(60);
/// A `.agent-fleet.new.<pid>` older than this is a leftover of a killed swap.
const LEFTOVER: Duration = Duration::from_secs(600);

/// What the checkout and the running build look like.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub installed: String,
    pub target: String,
    pub head: String,
    pub branch: String,
    pub dirty: bool,
    /// `HEAD` is an ancestor of `origin/main`.
    pub ff: bool,
    /// What the installed file says to `--version`: it may already be the target while the
    /// running process is not.
    pub on_disk: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Current,
    /// The target is already installed on disk; the running process just has to restart.
    Restart,
    Update,
    Blocked(String),
}

/// The first 7 characters of a commit, when it has them.
fn short(c: &str) -> Option<&str> {
    c.get(..7)
}

/// Same commit, whichever way each side abbreviates it. A `-dirty` or `unknown` build is no
/// commit: it is never the target, so a host running one always gets replaced.
fn same(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n >= 7 && !a.contains('-') && !b.contains('-') && a.get(..n) == b.get(..n)
}

pub fn decide(f: &Facts) -> Decision {
    if same(&f.installed, &f.target) {
        Decision::Current
    } else if short(&f.target).is_some_and(|t| f.on_disk.contains(&format!("({t})"))) {
        Decision::Restart
    } else if f.dirty {
        Decision::Blocked("checkout has uncommitted changes".into())
    } else if f.branch != "main" {
        Decision::Blocked(format!("checkout is on {:?}, not main", f.branch))
    } else if !f.ff {
        Decision::Blocked("checkout has commits origin/main lacks (diverged)".into())
    } else {
        Decision::Update
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Current,
    /// The new binary is in place; the running process is still the old one.
    Updated,
    Blocked(String),
    /// Nothing was swapped: the old binary stays.
    Failed(String),
    /// Nothing new to say: another host holds the lock, or this target already failed to build.
    Unchanged,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `origin/main` after the fetch, when there was one.
    pub target: Option<String>,
    pub outcome: Outcome,
}

#[derive(Clone)]
pub struct Cfg {
    pub checkout: PathBuf,
    /// The installed binary.
    pub dest: PathBuf,
    /// Builds the binary, run in the checkout.
    pub build: Vec<String>,
    /// Where the build leaves it, relative to the checkout.
    pub built: PathBuf,
    /// Service-manager commands that restart the long-running processes, in order.
    pub restart: Vec<Vec<String>>,
}

impl Cfg {
    pub fn cargo(checkout: PathBuf, dest: PathBuf, restart: Vec<Vec<String>>) -> Self {
        Cfg {
            checkout,
            dest,
            build: ["cargo", "build", "--release", "--locked"]
                .map(String::from)
                .to_vec(),
            built: "target/release/agent-fleet".into(),
            restart,
        }
    }
}

/// Runs `c` to the end or kills its whole process group after `limit`. No stdin and no git
/// prompt: under a service manager nobody can answer one.
fn exec(mut c: Command, limit: Duration) -> Result<Output, String> {
    use std::os::unix::process::CommandExt;
    let mut child = c
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = r.read_to_end(&mut v);
            v
        })
    };
    let out = drain(Box::new(child.stdout.take().ok_or("no stdout")?));
    let err = drain(Box::new(child.stderr.take().ok_or("no stderr")?));
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                // The group, not just the child: cargo and git spawn their own children.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(format!("timed out after {}s", limit.as_secs()));
            }
        }
    };
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

fn run(dir: &Path, cmd: &[String], limit: Duration) -> Result<String, String> {
    let (prog, args) = cmd.split_first().ok_or("empty command")?;
    let mut c = Command::new(prog);
    c.args(args).current_dir(dir);
    let what = args.first().map_or("", String::as_str);
    let out = exec(c, limit).map_err(|e| format!("{prog} {what}: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let mut tail: Vec<&str> = err
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect();
    tail.reverse();
    let mut why = tail.join(" | ");
    if why.len() > 300 {
        why = why.chars().take(300).collect();
    }
    Err(format!("{prog} {what} ({}): {why}", out.status))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let cmd: Vec<String> = std::iter::once("git")
        .chain(args.iter().copied())
        .map(String::from)
        .collect();
    let limit = if args.first() == Some(&"fetch") {
        GIT_NET
    } else {
        GIT_LOCAL
    };
    run(dir, &cmd, limit)
}

/// `HEAD` is an ancestor of `origin/main`: git exits 0 for yes and 1 for no; anything else is
/// an error, not a divergence.
fn is_ancestor(dir: &Path) -> Result<bool, String> {
    let mut c = Command::new("git");
    c.args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
        .current_dir(dir);
    let out = exec(c, GIT_LOCAL).map_err(|e| format!("git merge-base: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git merge-base ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn facts(cfg: &Cfg, installed: &str) -> Result<Facts, String> {
    let d = &cfg.checkout;
    Ok(Facts {
        installed: installed.into(),
        target: git(d, &["rev-parse", "origin/main"])?,
        head: git(d, &["rev-parse", "HEAD"])?,
        branch: git(d, &["branch", "--show-current"])?,
        // Untracked files do not stop a fast-forward; `build.rs` counts the same way.
        dirty: !git(d, &["status", "--porcelain", "--untracked-files=no"])?.is_empty(),
        ff: is_ancestor(d)?,
        on_disk: version_of(&cfg.dest).unwrap_or_default(),
    })
}

/// What `bin --version` printed, when it ran and exited 0.
fn version_of(bin: &Path) -> Result<String, String> {
    let mut c = Command::new(bin);
    c.arg("--version");
    let out = exec(c, VERSION).map_err(|e| format!("{} does not run: {e}", bin.display()))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(said)
    } else {
        Err(format!(
            "{} --version ({}): {said}",
            bin.display(),
            out.status
        ))
    }
}

/// One host per checkout at a time: the lock lives in the checkout's git dir, so two hosts of
/// one machine neither race on `git merge` nor both restart.
pub struct Lock(#[allow(dead_code)] std::fs::File);

/// `None` when another process holds it.
pub fn lock(cfg: &Cfg) -> Result<Option<Lock>, String> {
    use rustix::fs::{FlockOperation, flock};
    let dir = git(&cfg.checkout, &["rev-parse", "--absolute-git-dir"])?;
    let path = PathBuf::from(dir).join("agent-fleet-update.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    match flock(&f, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(Lock(f))),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(None),
        Err(e) => Err(format!("lock {}: {e}", path.display())),
    }
}

/// Removes the temp files a killed swap left next to the installed binary.
pub fn sweep(dest: &Path, older_than: Duration) {
    let Some(dir) = dest.parent() else { return };
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
        if old
            && e.file_name()
                .to_string_lossy()
                .starts_with(".agent-fleet.new.")
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// One check. `installed` is the commit the running binary was built from; `failed` is a
/// target whose build already failed: it is not built again until `origin/main` moves.
pub fn check(cfg: &Cfg, installed: &str, failed: Option<&str>) -> Report {
    let fail = |target: Option<String>, why: String| Report {
        target,
        outcome: Outcome::Failed(why),
    };
    if let Err(e) = git(&cfg.checkout, &["fetch", "origin", "main"]) {
        return fail(None, e);
    }
    let f = match facts(cfg, installed) {
        Ok(f) => f,
        Err(e) => return fail(None, e),
    };
    let target = Some(f.target.clone());
    let outcome = match decide(&f) {
        Decision::Current => Outcome::Current,
        Decision::Restart => Outcome::Updated,
        Decision::Blocked(why) => Outcome::Blocked(why),
        Decision::Update if failed == Some(f.target.as_str()) => Outcome::Unchanged,
        Decision::Update => match install(cfg, &f) {
            Ok(()) => Outcome::Updated,
            Err(e) => Outcome::Failed(e),
        },
    };
    Report { target, outcome }
}

fn install(cfg: &Cfg, f: &Facts) -> Result<(), String> {
    if !same(&f.head, &f.target) {
        git(&cfg.checkout, &["merge", "--ff-only", "origin/main"])?;
    }
    run(&cfg.checkout, &cfg.build, BUILD)?;
    // Next to the installed file: a rename across volumes fails (EXDEV). Per process, so two
    // hosts never share a temp file.
    let tmp = cfg
        .dest
        .with_file_name(format!(".agent-fleet.new.{}", std::process::id()));
    let want = short(&f.target).ok_or("origin/main is not a commit id")?;
    let swap = || -> Result<(), String> {
        let built = cfg.checkout.join(&cfg.built);
        std::fs::copy(&built, &tmp)
            .map_err(|e| format!("copy {} to {}: {e}", built.display(), tmp.display()))?;
        let said = version_of(&tmp).map_err(|e| format!("new binary: {e}"))?;
        if !said.contains(&format!("({want})")) {
            return Err(format!("new binary reports {said:?}, not {want}"));
        }
        // The old binary stays reachable: the new one has only proved it prints its version.
        let prev = cfg.dest.with_file_name(format!(
            "{}.prev",
            cfg.dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        let _ = std::fs::remove_file(&prev);
        if let Err(e) = std::fs::hard_link(&cfg.dest, &prev) {
            eprintln!(
                "agent-fleet host: self-update: keeping {}: {e}",
                prev.display()
            );
        }
        std::fs::rename(&tmp, &cfg.dest)
            .map_err(|e| format!("rename {} to {}: {e}", tmp.display(), cfg.dest.display()))
    };
    let r = swap();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// The `host.update` to publish for a check, or none when it says what the log already says.
pub fn record(
    host: &str,
    installed: &str,
    r: &Report,
    prev: Option<&HostUpdate>,
) -> Option<HostUpdate> {
    let (status, why) = match &r.outcome {
        Outcome::Unchanged => return None,
        Outcome::Current => (UpdateStatus::Current, None),
        Outcome::Updated => (
            UpdateStatus::Behind,
            Some("updated on disk, restart pending".into()),
        ),
        Outcome::Blocked(w) => (UpdateStatus::Blocked, Some(w.clone())),
        Outcome::Failed(w) => (UpdateStatus::Failed, Some(w.clone())),
    };
    let u = HostUpdate {
        host: host.into(),
        status,
        installed: installed.into(),
        target: r.target.clone(),
        why,
        since: None,
    };
    match prev {
        None if status == UpdateStatus::Current => None,
        // A restart that keeps failing stays one `failed` record, not a flip-flop.
        Some(p)
            if status == UpdateStatus::Behind
                && p.status == UpdateStatus::Failed
                && p.target == u.target
                && p.why.as_deref().is_some_and(|w| w.starts_with("restart")) =>
        {
            None
        }
        Some(p) if (p.status, &p.target, &p.why) == (u.status, &u.target, &u.why) => None,
        _ => Some(u),
    }
}

/// What a tick needs from its host, so the tick is testable with fakes.
trait Io {
    /// This host's `host.update` record in the log.
    fn prev(&self) -> Option<HostUpdate>;
    /// Whether the record was written.
    async fn publish(&self, u: HostUpdate) -> bool;
    /// Work in flight that a restart would cut short.
    fn busy(&self) -> bool;
    /// Runs the service-manager commands; returns what failed.
    async fn restart(&self, cmds: Vec<Vec<String>>) -> Vec<String>;
}

struct HostIo(std::sync::Arc<crate::host::Host>);

impl Io for HostIo {
    fn prev(&self) -> Option<HostUpdate> {
        match self.0.view() {
            Ok(v) => v.state.updates.get(&self.0.cfg.host).cloned(),
            Err(e) => {
                // `record` tolerates no previous record: still say what the check found.
                eprintln!("agent-fleet host: self-update: replica unreadable: {e:#}");
                None
            }
        }
    }

    async fn publish(&self, u: HostUpdate) -> bool {
        self.0.publish(self.0.presence(Known::HostUpdate(u))).await
    }

    fn busy(&self) -> bool {
        self.0.is_busy()
    }

    async fn restart(&self, cmds: Vec<Vec<String>>) -> Vec<String> {
        tokio::task::spawn_blocking(move || restart(&cmds))
            .await
            .unwrap_or_else(|e| vec![format!("restart task died: {e}")])
    }
}

#[derive(Default)]
struct Tick {
    /// The restart commands run once per target: a restart that did not take must not bounce
    /// the hub and the other services every tick.
    restarted_for: Option<String>,
}

/// One tick: lock, check, publish what is wrong, restart after an update. Publishes before
/// the restart, which may kill this process; a host that restarts itself comes back already
/// current and clears its own record.
async fn once<I: Io>(io: &I, cfg: &Cfg, me: &str, installed: &str, st: &mut Tick) {
    let _lock = match lock(cfg) {
        Ok(Some(l)) => l,
        Ok(None) => return,
        Err(e) => {
            eprintln!("agent-fleet host: self-update: {e}");
            return;
        }
    };
    let prev = io.prev();
    // A build that failed for this target is not repeated until `origin/main` moves; a restart
    // that failed is retried by the restart rule below, not by building.
    let failed = prev
        .as_ref()
        .filter(|p| p.status == UpdateStatus::Failed)
        .filter(|p| !p.why.as_deref().is_some_and(|w| w.starts_with("restart")))
        .and_then(|p| p.target.clone());
    let (c, inst) = (cfg.clone(), installed.to_string());
    let r = match tokio::task::spawn_blocking(move || check(&c, &inst, failed.as_deref())).await {
        Ok(r) => r,
        Err(e) => Report {
            target: None,
            outcome: Outcome::Failed(format!("update check died: {e}")),
        },
    };
    if let Some(u) = record(me, installed, &r, prev.as_ref()) {
        io.publish(u).await;
    }
    if r.outcome != Outcome::Updated || st.restarted_for == r.target {
        return;
    }
    // A spawn or handoff in flight would be cut short: try again next tick.
    if io.busy() {
        return;
    }
    st.restarted_for = r.target.clone();
    let errs = io.restart(cfg.restart.clone()).await;
    if errs.is_empty() {
        return;
    }
    eprintln!("agent-fleet host: self-update: {errs:?}");
    let failed = Report {
        target: r.target.clone(),
        outcome: Outcome::Failed(format!("restart failed: {}", errs.join("; "))),
    };
    if let Some(u) = record(me, installed, &failed, prev.as_ref())
        && !io.publish(u).await
    {
        // Not recorded: try the restart and the report again next tick.
        st.restarted_for = None;
    }
}

/// Every `EVERY`: see `once`.
pub(crate) async fn update_loop(host: std::sync::Arc<crate::host::Host>) {
    let Some(cfg) = host.cfg.update.clone() else {
        eprintln!("agent-fleet host: self-update off (no --self-update)");
        return std::future::pending().await;
    };
    sweep(&cfg.dest, LEFTOVER);
    let me = host.cfg.host.clone();
    let io = HostIo(host);
    let mut st = Tick::default();
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        once(&io, &cfg, &me, env!("AGENT_FLEET_COMMIT"), &mut st).await;
    }
}

/// Restart the long-running processes through their service manager, in order. Returns what
/// failed; a failure does not stop the rest.
pub fn restart(cmds: &[Vec<String>]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| run(Path::new("/"), c, RESTART).err())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn sh(dir: &Path, script: &str) -> String {
        run(
            dir,
            &["sh".into(), "-c".into(), script.into()],
            Duration::from_secs(30),
        )
        .unwrap()
    }

    /// An origin, a checkout of it on main, an installed script that prints `old` (`installed`
    /// is the checkout's first commit), and a "build" that writes a script reporting HEAD.
    struct Fx {
        _tmp: tempfile::TempDir,
        origin: PathBuf,
        cfg: Cfg,
        installed: String,
    }

    const BUILD: &str = r#"printf '#!/bin/sh\necho "agent-fleet 0.1.0 (%s)"\n' "$(git rev-parse --short=7 HEAD)" > built.sh && chmod +x built.sh"#;

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let (origin, work) = (t.join("origin"), t.join("work"));
        std::fs::create_dir_all(&origin).unwrap();
        sh(
            &origin,
            "git init -q -b main . && git config user.email a@b && git config user.name n && git commit -q --allow-empty -m one",
        );
        sh(t, "git clone -q origin work");
        sh(&work, "git config user.email a@b && git config user.name n");
        let installed = sh(&work, "git rev-parse HEAD");
        let bin = t.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let dest = bin.join("agent-fleet");
        std::fs::write(&dest, "#!/bin/sh\necho old\n").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Cfg {
            checkout: work,
            dest,
            build: vec!["sh".into(), "-c".into(), BUILD.into()],
            built: "built.sh".into(),
            restart: vec![],
        };
        Fx {
            _tmp: tmp,
            origin,
            cfg,
            installed,
        }
    }

    fn push_commit(fx: &Fx) -> String {
        sh(&fx.origin, "git commit -q --allow-empty -m two");
        sh(&fx.origin, "git rev-parse HEAD")
    }

    fn no_temp(f: &Fx) -> bool {
        std::fs::read_dir(f.cfg.dest.parent().unwrap())
            .unwrap()
            .all(|e| {
                !e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".agent-fleet.new")
            })
    }

    fn installed_says(fx: &Fx) -> String {
        sh(
            fx.cfg.dest.parent().unwrap(),
            &format!("{}", fx.cfg.dest.display()),
        )
    }

    fn base() -> Facts {
        Facts {
            installed: "aaaaaaa".into(),
            target: "bbbbbbb".into(),
            head: "aaaaaaa".into(),
            branch: "main".into(),
            dirty: false,
            ff: true,
            on_disk: "old".into(),
        }
    }

    #[test]
    fn decide_updates_only_a_clean_main_that_can_fast_forward() {
        assert_eq!(decide(&base()), Decision::Update);
        let at = |f: Facts| decide(&f);
        assert_eq!(
            at(Facts {
                installed: "bbbbbbbcc".into(),
                ..base()
            }),
            Decision::Current
        );
        assert!(
            matches!(at(Facts { dirty: true, ..base() }), Decision::Blocked(w) if w.contains("uncommitted"))
        );
        assert!(
            matches!(at(Facts { branch: "wip".into(), ..base() }), Decision::Blocked(w) if w.contains("not main"))
        );
        assert!(
            matches!(at(Facts { ff: false, ..base() }), Decision::Blocked(w) if w.contains("diverged"))
        );
    }

    #[test]
    fn behind_origin_main_pulls_builds_and_swaps_atomically() {
        let f = fx();
        let target = push_commit(&f);
        let inode = std::fs::metadata(&f.cfg.dest).unwrap().ino();
        let r = check(&f.cfg, &f.installed, None);
        assert_eq!(r.outcome, Outcome::Updated, "{r:?}");
        assert_eq!(r.target.as_deref(), Some(target.as_str()));
        assert_eq!(
            installed_says(&f),
            format!("agent-fleet 0.1.0 ({})", &target[..7])
        );
        assert_ne!(
            std::fs::metadata(&f.cfg.dest).unwrap().ino(),
            inode,
            "renamed, not rewritten"
        );
        assert!(no_temp(&f));
        // The build is installed: the next check is Current.
        assert_eq!(check(&f.cfg, &target, None).outcome, Outcome::Current);
    }

    #[test]
    fn a_current_install_touches_nothing() {
        let f = fx();
        let r = check(&f.cfg, &f.installed, None);
        assert_eq!(r.outcome, Outcome::Current);
        assert_eq!(installed_says(&f), "old");
    }

    #[test]
    fn a_dirty_checkout_is_blocked_and_the_old_binary_stays() {
        let f = fx();
        push_commit(&f);
        std::fs::write(f.cfg.checkout.join("README"), "x").unwrap();
        sh(&f.cfg.checkout, "git add README");
        let head = sh(&f.cfg.checkout, "git rev-parse HEAD");
        let r = check(&f.cfg, &f.installed, None);
        assert!(
            matches!(&r.outcome, Outcome::Blocked(w) if w.contains("uncommitted")),
            "{r:?}"
        );
        assert_eq!(
            sh(&f.cfg.checkout, "git rev-parse HEAD"),
            head,
            "not pulled"
        );
        assert_eq!(installed_says(&f), "old");
    }

    #[test]
    fn a_diverged_checkout_is_blocked_and_left_alone() {
        let f = fx();
        push_commit(&f);
        sh(&f.cfg.checkout, "git commit -q --allow-empty -m local");
        let head = sh(&f.cfg.checkout, "git rev-parse HEAD");
        let r = check(&f.cfg, &f.installed, None);
        assert!(
            matches!(&r.outcome, Outcome::Blocked(w) if w.contains("diverged")),
            "{r:?}"
        );
        assert_eq!(sh(&f.cfg.checkout, "git rev-parse HEAD"), head);
        assert_eq!(installed_says(&f), "old");
    }

    #[test]
    fn a_checkout_off_main_is_blocked() {
        let f = fx();
        push_commit(&f);
        sh(&f.cfg.checkout, "git checkout -q -b wip");
        let r = check(&f.cfg, &f.installed, None);
        assert!(
            matches!(&r.outcome, Outcome::Blocked(w) if w.contains("not main")),
            "{r:?}"
        );
    }

    #[test]
    fn a_failed_build_keeps_the_old_binary() {
        let mut f = fx();
        push_commit(&f);
        f.cfg.build = vec!["sh".into(), "-c".into(), "echo boom >&2; exit 1".into()];
        let r = check(&f.cfg, &f.installed, None);
        assert!(
            matches!(&r.outcome, Outcome::Failed(w) if w.contains("boom")),
            "{r:?}"
        );
        assert_eq!(installed_says(&f), "old");
    }

    #[test]
    fn a_binary_that_reports_another_commit_is_never_installed() {
        let mut f = fx();
        push_commit(&f);
        f.cfg.build = vec![
            "sh".into(),
            "-c".into(),
            r#"printf '#!/bin/sh\necho "agent-fleet 0.1.0 (0000000)"\n' > built.sh && chmod +x built.sh"#.into(),
        ];
        let r = check(&f.cfg, &f.installed, None);
        assert!(
            matches!(&r.outcome, Outcome::Failed(w) if w.contains("0000000")),
            "{r:?}"
        );
        assert_eq!(installed_says(&f), "old");
        assert!(no_temp(&f), "no half-installed file");
    }

    #[test]
    fn record_says_only_what_changed() {
        let blocked = Report {
            target: Some("bbb".into()),
            outcome: Outcome::Blocked("dirty".into()),
        };
        let u = record("h", "aaa", &blocked, None).expect("first report");
        assert_eq!(
            (u.status, u.why.as_deref()),
            (UpdateStatus::Blocked, Some("dirty"))
        );
        assert!(
            record("h", "aaa", &blocked, Some(&u)).is_none(),
            "unchanged"
        );
        let failed = Report {
            outcome: Outcome::Failed("boom".into()),
            ..blocked.clone()
        };
        assert_eq!(
            record("h", "aaa", &failed, Some(&u)).unwrap().status,
            UpdateStatus::Failed
        );
        let cur = Report {
            target: Some("aaa".into()),
            outcome: Outcome::Current,
        };
        assert!(record("h", "aaa", &cur, None).is_none(), "nothing to clear");
        assert_eq!(
            record("h", "aaa", &cur, Some(&u)).unwrap().status,
            UpdateStatus::Current
        );
        let done = Report {
            outcome: Outcome::Updated,
            ..blocked
        };
        assert_eq!(
            record("h", "aaa", &done, None).unwrap().status,
            UpdateStatus::Behind
        );
    }

    #[test]
    fn a_modified_or_unknown_build_is_never_current_and_an_installed_target_only_restarts() {
        let t = "b".repeat(40);
        let dirty = Facts {
            installed: format!("{t}-dirty"),
            target: t.clone(),
            ..base()
        };
        assert_eq!(decide(&dirty), Decision::Update);
        let unknown = Facts {
            installed: "unknown".into(),
            ..base()
        };
        assert_eq!(decide(&unknown), Decision::Update);
        let on_disk = Facts {
            on_disk: "agent-fleet 0.1.0 (bbbbbbb)".into(),
            dirty: true,
            ..base()
        };
        assert_eq!(decide(&on_disk), Decision::Restart);
    }

    #[test]
    fn an_unreachable_origin_is_a_failure_that_keeps_the_old_binary() {
        let f = fx();
        sh(&f.cfg.checkout, "git remote set-url origin /nonexistent");
        let r = check(&f.cfg, &f.installed, None);
        assert!(
            matches!(&r.outcome, Outcome::Failed(w) if w.contains("fetch")),
            "{r:?}"
        );
        assert_eq!(r.target, None);
        assert_eq!(installed_says(&f), "old");
    }

    #[test]
    fn a_target_already_on_disk_is_restarted_not_rebuilt() {
        let mut f = fx();
        let target = push_commit(&f);
        assert_eq!(check(&f.cfg, &f.installed, None).outcome, Outcome::Updated);
        // The process still runs the old build; a broken build must not matter now.
        f.cfg.build = vec!["false".into()];
        let inode = std::fs::metadata(&f.cfg.dest).unwrap().ino();
        let r = check(&f.cfg, &f.installed, None);
        assert_eq!(r.outcome, Outcome::Updated, "{r:?}");
        assert_eq!(r.target.as_deref(), Some(target.as_str()));
        assert_eq!(
            std::fs::metadata(&f.cfg.dest).unwrap().ino(),
            inode,
            "no second swap"
        );
    }

    #[test]
    fn a_failing_restart_stays_one_failed_record() {
        let pending = Report {
            target: Some("bbb".into()),
            outcome: Outcome::Updated,
        };
        let failed = HostUpdate {
            host: "h".into(),
            status: UpdateStatus::Failed,
            installed: "aaa".into(),
            target: Some("bbb".into()),
            why: Some("restart failed: launchctl".into()),
            since: None,
        };
        assert!(
            record("h", "aaa", &pending, Some(&failed)).is_none(),
            "no flip-flop"
        );
        let moved = Report {
            target: Some("ccc".into()),
            outcome: Outcome::Updated,
        };
        assert!(
            record("h", "aaa", &moved, Some(&failed)).is_some(),
            "another target is news"
        );
        let build_failed = HostUpdate {
            why: Some("cargo build".into()),
            ..failed
        };
        assert!(record("h", "aaa", &pending, Some(&build_failed)).is_some());
    }

    #[test]
    fn a_command_past_its_limit_is_killed_with_its_children() {
        let t = tempfile::tempdir().unwrap();
        let mark = t.path().join("late");
        let cmd = vec![
            "sh".into(),
            "-c".into(),
            format!("(sleep 2; touch {}) & sleep 30", mark.display()),
        ];
        let started = Instant::now();
        let e = run(t.path(), &cmd, Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2200));
        assert!(!mark.exists(), "the child died with the group");
    }

    #[test]
    fn the_old_binary_stays_as_prev_and_the_second_holder_of_the_lock_is_refused() {
        let f = fx();
        push_commit(&f);
        assert_eq!(check(&f.cfg, &f.installed, None).outcome, Outcome::Updated);
        let prev = f.cfg.dest.with_file_name("agent-fleet.prev");
        assert_eq!(
            sh(prev.parent().unwrap(), &prev.display().to_string()),
            "old"
        );
        let held = lock(&f.cfg).unwrap();
        assert!(held.is_some());
        assert!(lock(&f.cfg).unwrap().is_none(), "one holder at a time");
        drop(held);
        assert!(lock(&f.cfg).unwrap().is_some());
    }

    #[test]
    fn sweep_removes_leftover_temp_files_only() {
        let f = fx();
        let dir = f.cfg.dest.parent().unwrap();
        std::fs::write(dir.join(".agent-fleet.new.123"), "x").unwrap();
        std::fs::write(dir.join("keep"), "x").unwrap();
        sweep(&f.cfg.dest, Duration::from_secs(3600));
        assert!(
            dir.join(".agent-fleet.new.123").exists(),
            "too young to be a leftover"
        );
        sweep(&f.cfg.dest, Duration::ZERO);
        assert!(!dir.join(".agent-fleet.new.123").exists());
        assert!(dir.join("keep").exists() && f.cfg.dest.exists());
    }

    /// A host with the log and the service manager faked.
    #[derive(Default)]
    struct Fake {
        prev: std::cell::RefCell<Option<HostUpdate>>,
        log: std::cell::RefCell<Vec<String>>,
        busy: std::cell::Cell<bool>,
        drop_failed: std::cell::Cell<bool>,
        restart_errs: std::cell::RefCell<Vec<String>>,
    }

    impl Io for Fake {
        fn prev(&self) -> Option<HostUpdate> {
            self.prev.borrow().clone()
        }
        async fn publish(&self, u: HostUpdate) -> bool {
            self.log
                .borrow_mut()
                .push(format!("publish {:?}", u.status));
            if u.status == UpdateStatus::Failed && self.drop_failed.get() {
                return false;
            }
            *self.prev.borrow_mut() = (u.status != UpdateStatus::Current).then_some(u);
            true
        }
        fn busy(&self) -> bool {
            self.busy.get()
        }
        async fn restart(&self, _: Vec<Vec<String>>) -> Vec<String> {
            self.log.borrow_mut().push("restart".into());
            self.restart_errs.borrow().clone()
        }
    }

    async fn tick(f: &Fx, io: &Fake, st: &mut Tick) {
        once(io, &f.cfg, "h", &f.installed, st).await;
    }

    #[tokio::test]
    async fn the_tick_publishes_before_it_restarts_and_restarts_once_per_target() {
        let f = fx();
        push_commit(&f);
        let (io, mut st) = (Fake::default(), Tick::default());
        tick(&f, &io, &mut st).await;
        tick(&f, &io, &mut st).await;
        assert_eq!(*io.log.borrow(), ["publish Behind", "restart"]);
    }

    #[tokio::test]
    async fn the_tick_does_not_restart_while_a_spawn_or_handoff_is_in_flight() {
        let f = fx();
        push_commit(&f);
        let (io, mut st) = (Fake::default(), Tick::default());
        io.busy.set(true);
        tick(&f, &io, &mut st).await;
        assert_eq!(*io.log.borrow(), ["publish Behind"]);
        assert_eq!(st.restarted_for, None, "left for the next tick");
        io.busy.set(false);
        tick(&f, &io, &mut st).await;
        assert_eq!(*io.log.borrow(), ["publish Behind", "restart"]);
    }

    #[tokio::test]
    async fn a_failed_restart_report_that_was_not_written_is_retried() {
        let f = fx();
        push_commit(&f);
        let (io, mut st) = (Fake::default(), Tick::default());
        *io.restart_errs.borrow_mut() = vec!["boom".into()];
        io.drop_failed.set(true);
        tick(&f, &io, &mut st).await;
        io.drop_failed.set(false);
        tick(&f, &io, &mut st).await;
        assert_eq!(
            *io.log.borrow(),
            [
                "publish Behind",
                "restart",
                "publish Failed",
                "restart",
                "publish Failed"
            ]
        );
        tick(&f, &io, &mut st).await;
        assert_eq!(io.log.borrow().len(), 5, "written: now once per target");
    }

    #[tokio::test]
    async fn a_target_that_failed_to_build_is_not_built_again_until_main_moves() {
        let mut f = fx();
        push_commit(&f);
        f.cfg.build = vec![
            "sh".into(),
            "-c".into(),
            "echo x >> ../builds; exit 1".into(),
        ];
        let builds = f.cfg.checkout.parent().unwrap().join("builds");
        let (io, mut st) = (Fake::default(), Tick::default());
        tick(&f, &io, &mut st).await;
        tick(&f, &io, &mut st).await;
        assert_eq!(std::fs::read_to_string(&builds).unwrap().lines().count(), 1);
        assert_eq!(*io.log.borrow(), ["publish Failed"]);
        push_commit(&f);
        tick(&f, &io, &mut st).await;
        assert_eq!(
            std::fs::read_to_string(&builds).unwrap().lines().count(),
            2,
            "main moved"
        );
    }

    #[tokio::test]
    async fn a_tick_skips_while_another_host_holds_the_checkout() {
        let f = fx();
        push_commit(&f);
        let _held = lock(&f.cfg).unwrap().unwrap();
        let (io, mut st) = (Fake::default(), Tick::default());
        tick(&f, &io, &mut st).await;
        assert!(io.log.borrow().is_empty());
        assert_eq!(installed_says(&f), "old");
    }

    #[test]
    fn restart_runs_each_command_and_reports_the_failures() {
        let t = tempfile::tempdir().unwrap();
        let mark = t.path().join("m");
        let ok = vec!["touch".to_string(), mark.display().to_string()];
        let bad = vec!["false".to_string()];
        let errs = restart(&[bad, ok]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(mark.exists(), "a failure does not stop the rest");
    }
}
