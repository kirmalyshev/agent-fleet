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
use crate::spawn::{git, tail};

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

/// Pulls `clone` (which must be on `main` with no uncommitted changes), and unless `running` is
/// already its head, builds it and renames the build over `exe`.
pub async fn update(clone: &Path, exe: &Path, running: Option<&str>) -> Result<Outcome> {
    let branch = git(clone, &["symbolic-ref", "--short", "HEAD"])
        .await
        .with_context(|| format!("{} is not on a branch", clone.display()))?;
    if branch.trim() != "main" {
        bail!("{} is on {}, not main", clone.display(), branch.trim());
    }
    if !git(clone, &["status", "--porcelain", "--untracked-files=no"])
        .await?
        .trim()
        .is_empty()
    {
        bail!("{} has uncommitted changes", clone.display());
    }
    git(clone, &["pull", "--ff-only", "--quiet"]).await?;
    let head = git(clone, &["rev-parse", "HEAD"]).await?.trim().to_string();
    if running == Some(head.as_str()) {
        return Ok(Outcome::Current(head));
    }
    let dir = exe.parent().context("the binary has no directory")?;
    // Built beside the binary, so the rename is atomic. Overwriting it in place kills the running
    // process on macOS (code signing).
    let root = dir.join(format!(".agent-fleet-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // A supervised daemon's PATH may lack cargo; `cargo install` puts binaries next to it.
    let cargo = Some(dir.join("cargo"))
        .filter(|c| c.exists())
        .unwrap_or_else(|| PathBuf::from("cargo"));
    let built = tokio::process::Command::new(&cargo)
        .args(["install", "--locked", "--path"])
        .arg(clone)
        .arg("--root")
        .arg(&root)
        .output()
        .await
        .with_context(|| format!("run {}", cargo.display()));
    let done = built.and_then(|out| {
        if !out.status.success() {
            return Err(anyhow!(
                "cargo install: {}",
                tail(String::from_utf8_lossy(&out.stderr).trim())
            ));
        }
        std::fs::rename(root.join("bin/agent-fleet"), exe)
            .with_context(|| format!("move the build to {}", exe.display()))
    });
    let _ = std::fs::remove_dir_all(&root);
    done?;
    Ok(Outcome::Replaced(head))
}

/// The running binary's path. Linux names a binary replaced under it `<path> (deleted)`.
fn this_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map_or(exe.clone(), PathBuf::from))
}

/// Executes each `update.requested` this host has not answered, answers `update.result`, and
/// exits with `RESTART_STATUS` once its binary was replaced.
pub async fn update_loop(host: Arc<Host>) {
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let view = match host.view() {
            Ok(v) => v,
            Err(err) => {
                eprintln!("agent-fleet host: replica: {err}");
                continue;
            }
        };
        let me = &host.cfg.host;
        let todo: Vec<String> = view
            .state
            .updates
            .iter()
            .filter(|(id, u)| {
                !u.results.contains_key(me)
                    && Utc::now() - u.since < REQUEST_MAX_AGE
                    && seen.insert(id.to_string())
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in todo {
            let done = match this_exe() {
                Ok(exe) => update(Path::new(CLONE), &exe, COMMIT).await,
                Err(err) => Err(err),
            };
            let (commit, error, restart) = match done {
                Ok(Outcome::Current(c)) => (Some(c), None, false),
                Ok(Outcome::Replaced(c)) => (Some(c), None, true),
                Err(err) => (None, Some(tail(&format!("{err:#}"))), false),
            };
            let result = UpdateResult {
                host: me.clone(),
                ok: error.is_none(),
                commit,
                error,
            };
            host.publish(host.presence(Known::UpdateResult(result)).re(&id))
                .await;
            if restart {
                eprintln!(
                    "agent-fleet host: binary replaced; exiting for the supervisor to restart"
                );
                std::process::exit(RESTART_STATUS);
            }
        }
    }
}

/// Follows the hub until each of `hosts` answered `update.result` and reports, in
/// `host.health`, the commit it named. Fails if any refused or did not get there in time.
pub async fn wait(client: &Client, id: &str, hosts: &[String]) -> Result<()> {
    let deadline = tokio::time::Instant::now() + WAIT;
    // host → whether it got there
    let mut done: BTreeMap<&str, bool> = BTreeMap::new();
    let mut last = None;
    loop {
        match client.state().await {
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
            Err(_) => {}
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
                "not every host runs its new build after {} min",
                WAIT.as_secs() / 60
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
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
        std::fs::write(
            &cargo,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = --root ] && root=$2; shift; done\n\
             mkdir -p \"$root/bin\" && echo new > \"$root/bin/agent-fleet\"\n",
        )
        .unwrap();
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
        assert_eq!(left.len(), 2, "the build dir is gone: {left:?}");
    }

    #[test]
    fn short_never_panics_on_odd_input() {
        assert_eq!(short(Some("0123456789abcdef")), "0123456");
        assert_eq!(short(Some("abc")), "abc");
        assert_eq!(short(Some("ééééé")), "ééééé");
        assert_eq!(short(None), "unknown");
    }
}
