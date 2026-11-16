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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Current,
    Update,
    Blocked(String),
}

/// Same commit, whichever way each side abbreviates it.
fn same(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n >= 7 && a[..n] == b[..n]
}

pub fn decide(f: &Facts) -> Decision {
    if same(&f.installed, &f.target) {
        Decision::Current
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
    let last = err.lines().rev().find(|l| !l.trim().is_empty());
    Err(format!(
        "{prog} {}: {}",
        args.first().map_or("", String::as_str),
        last.unwrap_or("failed")
    ))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let cmd: Vec<String> = std::iter::once("git")
        .chain(args.iter().copied())
        .map(String::from)
        .collect();
    run(dir, &cmd)
}

fn facts(cfg: &Cfg, installed: &str) -> Result<Facts, String> {
    let d = &cfg.checkout;
    Ok(Facts {
        installed: installed.into(),
        target: git(d, &["rev-parse", "origin/main"])?,
        head: git(d, &["rev-parse", "HEAD"])?,
        branch: git(d, &["branch", "--show-current"])?,
        dirty: !git(d, &["status", "--porcelain"])?.is_empty(),
        ff: git(d, &["merge-base", "--is-ancestor", "HEAD", "origin/main"]).is_ok(),
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
    // Next to the installed file: a rename across volumes is not atomic.
    let tmp = cfg.dest.with_file_name(".agent-fleet.new");
    let swap = || -> Result<(), String> {
        std::fs::copy(cfg.checkout.join(&cfg.built), &tmp).map_err(|e| format!("copy: {e}"))?;
        let said = Command::new(&tmp)
            .arg("--version")
            .output()
            .map_err(|e| format!("new binary does not run: {e}"))?;
        let said = String::from_utf8_lossy(&said.stdout);
        if !said.contains(&f.target[..7]) {
            return Err(format!(
                "new binary reports {:?}, not {}",
                said.trim(),
                &f.target[..7]
            ));
        }
        std::fs::rename(&tmp, &cfg.dest).map_err(|e| format!("rename: {e}"))
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
        Some(p) if (p.status, &p.target, &p.why) == (u.status, &u.target, &u.why) => None,
        _ => Some(u),
    }
}

/// Every `EVERY`: check, update, publish what is wrong, restart after an update. A host that
/// restarts itself comes back already current and clears its own record.
pub(crate) async fn update_loop(host: std::sync::Arc<crate::host::Host>) {
    let Some(cfg) = host.cfg.update.clone() else {
        return std::future::pending().await;
    };
    let installed = env!("AGENT_FLEET_COMMIT");
    let me = host.cfg.host.clone();
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let c = cfg.clone();
        let Ok(r) = tokio::task::spawn_blocking(move || check(&c, installed)).await else {
            continue;
        };
        let prev = host
            .view()
            .ok()
            .and_then(|v| v.state.updates.get(&me).cloned());
        if let Some(u) = record(&me, installed, &r, prev.as_ref()) {
            host.publish(host.presence(Known::HostUpdate(u))).await;
        }
        if r.outcome == Outcome::Updated {
            for e in restart(&cfg.restart) {
                eprintln!("agent-fleet host: self-update restart: {e}");
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

    /// An origin, a checkout of it on main, an installed script that reports the checkout's
    /// first commit, and a "build" that writes a script reporting the checkout's HEAD.
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
        assert!(!f.cfg.dest.with_file_name(".agent-fleet.new").exists());
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
        std::fs::write(f.cfg.checkout.join("scratch"), "x").unwrap();
        let r = check(&f.cfg, &f.installed);
        assert!(
            matches!(&r.outcome, Outcome::Blocked(w) if w.contains("uncommitted")),
            "{r:?}"
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
        assert!(
            !f.cfg.dest.with_file_name(".agent-fleet.new").exists(),
            "no half-installed file"
        );
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
