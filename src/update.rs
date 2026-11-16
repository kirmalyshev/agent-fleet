//! Self-update: keep the installed binary at `origin/main` of the host's checkout.
//!
//! The decision is pure (`decide`). The rest shells out to git and the build: pull only a clean,
//! `main`, fast-forwardable checkout; build; check the new binary reports the commit; then
//! rename it over the installed one. Overwriting in place kills the running process on macOS
//! (`OS_REASON_CODESIGNING`), and a rename keeps the old binary until the new one is whole.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::event::{HostUpdate, Known, UpdateStatus};

/// How often a host checks.
const EVERY: Duration = Duration::from_secs(300);

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

fn run(dir: &Path, cmd: &[String]) -> Result<String, String> {
    let (prog, args) = cmd.split_first().ok_or("empty command")?;
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
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
    Err(format!(
        "{prog} {} ({}): {why}",
        args.first().map_or("", String::as_str),
        out.status
    ))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let cmd: Vec<String> = std::iter::once("git")
        .chain(args.iter().copied())
        .map(String::from)
        .collect();
    run(dir, &cmd)
}

/// `HEAD` is an ancestor of `origin/main`: git exits 0 for yes and 1 for no; anything else is
/// an error, not a divergence.
fn is_ancestor(dir: &Path) -> Result<bool, String> {
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
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
        on_disk: Command::new(&cfg.dest)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default(),
    })
}

/// One check. `installed` is the commit the running binary was built from.
pub fn check(cfg: &Cfg, installed: &str) -> Report {
    let failed = |target: Option<String>, why: String| Report {
        target,
        outcome: Outcome::Failed(why),
    };
    if let Err(e) = git(&cfg.checkout, &["fetch", "origin", "main"]) {
        return failed(None, e);
    }
    let f = match facts(cfg, installed) {
        Ok(f) => f,
        Err(e) => return failed(None, e),
    };
    let target = Some(f.target.clone());
    match decide(&f) {
        Decision::Current => Report {
            target,
            outcome: Outcome::Current,
        },
        Decision::Restart => Report {
            target,
            outcome: Outcome::Updated,
        },
        Decision::Blocked(why) => Report {
            target,
            outcome: Outcome::Blocked(why),
        },
        Decision::Update => match install(cfg, &f) {
            Ok(()) => Report {
                target,
                outcome: Outcome::Updated,
            },
            Err(e) => failed(target, e),
        },
    }
}

fn install(cfg: &Cfg, f: &Facts) -> Result<(), String> {
    if !same(&f.head, &f.target) {
        git(&cfg.checkout, &["merge", "--ff-only", "origin/main"])?;
    }
    run(&cfg.checkout, &cfg.build)?;
    // Next to the installed file: a rename across volumes fails (EXDEV).
    let tmp = cfg
        .dest
        .with_file_name(format!(".agent-fleet.new.{}", std::process::id()));
    let want = short(&f.target).ok_or("origin/main is not a commit id")?;
    let swap = || -> Result<(), String> {
        let built = cfg.checkout.join(&cfg.built);
        std::fs::copy(&built, &tmp)
            .map_err(|e| format!("copy {} to {}: {e}", built.display(), tmp.display()))?;
        let out = Command::new(&tmp)
            .arg("--version")
            .output()
            .map_err(|e| format!("new binary does not run: {e}"))?;
        let said = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || !said.contains(&format!("({want}")) {
            return Err(format!("new binary reports {:?}, not {want}", said.trim()));
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
                && p.why.as_deref().is_some_and(|w| w.starts_with("restart")) =>
        {
            None
        }
        Some(p) if (p.status, &p.target, &p.why) == (u.status, &u.target, &u.why) => None,
        _ => Some(u),
    }
}

/// Every `EVERY`: check, update, publish what is wrong, restart after an update. A host that
/// restarts itself comes back already current and clears its own record.
pub(crate) async fn update_loop(host: std::sync::Arc<crate::host::Host>) {
    let Some(cfg) = host.cfg.update.clone() else {
        eprintln!("agent-fleet host: self-update off (no --self-update)");
        return std::future::pending().await;
    };
    let installed = env!("AGENT_FLEET_COMMIT");
    let me = host.cfg.host.clone();
    // The restart commands run once per target: a restart that did not take must not bounce
    // the hub and the other services every tick.
    let mut restarted_for: Option<String> = None;
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let c = cfg.clone();
        let r = match tokio::task::spawn_blocking(move || check(&c, installed)).await {
            Ok(r) => r,
            Err(e) => Report {
                target: None,
                outcome: Outcome::Failed(format!("update check died: {e}")),
            },
        };
        let prev = match host.view() {
            Ok(v) => v.state.updates.get(&me).cloned(),
            Err(e) => {
                // `record` tolerates no previous record: still say what the check found.
                eprintln!("agent-fleet host: self-update: replica unreadable: {e:#}");
                None
            }
        };
        let say = |r: &Report, prev: Option<&HostUpdate>| record(&me, installed, r, prev);
        // Before the restart: it may kill this process.
        if let Some(u) = say(&r, prev.as_ref()) {
            host.publish(host.presence(Known::HostUpdate(u))).await;
        }
        if r.outcome == Outcome::Updated && restarted_for != r.target {
            restarted_for = r.target.clone();
            let errs = restart(&cfg.restart);
            if !errs.is_empty() {
                let failed = Report {
                    target: r.target.clone(),
                    outcome: Outcome::Failed(format!("restart failed: {}", errs.join("; "))),
                };
                eprintln!("agent-fleet host: self-update: {errs:?}");
                if let Some(u) = say(&failed, prev.as_ref())
                    && !host.publish(host.presence(Known::HostUpdate(u))).await
                {
                    // Not recorded: try the restart and the report again next tick.
                    restarted_for = None;
                }
            }
        }
    }
}

/// Restart the long-running processes through their service manager, in order. Returns what
/// failed; a failure does not stop the rest.
pub fn restart(cmds: &[Vec<String>]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| run(Path::new("/"), c).err())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn sh(dir: &Path, script: &str) -> String {
        run(dir, &["sh".into(), "-c".into(), script.into()]).unwrap()
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
        let r = check(&f.cfg, &f.installed);
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
        assert_eq!(check(&f.cfg, &target).outcome, Outcome::Current);
    }

    #[test]
    fn a_current_install_touches_nothing() {
        let f = fx();
        let r = check(&f.cfg, &f.installed);
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
        let r = check(&f.cfg, &f.installed);
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
        let r = check(&f.cfg, &f.installed);
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
        let r = check(&f.cfg, &f.installed);
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
        let r = check(&f.cfg, &f.installed);
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
        let r = check(&f.cfg, &f.installed);
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
        let r = check(&f.cfg, &f.installed);
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
        assert_eq!(check(&f.cfg, &f.installed).outcome, Outcome::Updated);
        // The process still runs the old build; a broken build must not matter now.
        f.cfg.build = vec!["false".into()];
        let inode = std::fs::metadata(&f.cfg.dest).unwrap().ino();
        let r = check(&f.cfg, &f.installed);
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
        let build_failed = HostUpdate {
            why: Some("cargo build".into()),
            ..failed
        };
        assert!(record("h", "aaa", &pending, Some(&build_failed)).is_some());
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
