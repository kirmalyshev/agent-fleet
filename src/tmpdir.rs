//! Per-agent scratch dir, outside any checkout: `<cache dir>/agent-fleet/tmp/<session>/<name>`,
//! exported as TMPDIR/TMP/TEMP into the agent's pane at spawn. The host removes it when the pane
//! closes or the spawn fails; doctor sizes it.
//! Outside the worktree so a tool that walks the repository never meets the agent's mktemps.

use std::path::{Path, PathBuf};

/// Where every agent's scratch dir on this machine lives: the platform cache dir's
/// `agent-fleet/tmp` (`~/.cache/agent-fleet/tmp` on Linux). `None` without a home directory.
pub fn base() -> Option<PathBuf> {
    if cfg!(test) {
        // A test's spawn or retire never makes or removes a real agent's scratch.
        // Per test thread: parallel tests use the same agent ids.
        let t = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        return Some(
            std::env::temp_dir().join(format!("agent-fleet-test-{}-{t}", std::process::id())),
        );
    }
    cache_base()
}

fn cache_base() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.cache_dir().join("agent-fleet").join("tmp"))
}

/// The scratch dir of `agent` (`agent:<host>/<session>/<name>`) under `base`: `base/<session>/<name>`.
/// `None` for an id without all three parts, or a session or name that would leave `base`.
pub fn dir_in(base: &Path, agent: &str) -> Option<PathBuf> {
    let mut it = agent.rsplit('/');
    let (name, session) = (it.next()?, it.next()?);
    it.next()?;
    let ok = |s: &str| !s.is_empty() && s != "." && s != "..";
    (ok(name) && ok(session)).then(|| base.join(session).join(name))
}

/// `dir_in` under `base()`.
pub fn dir(agent: &str) -> Option<PathBuf> {
    dir_in(&base()?, agent)
}

/// Shell line for the agent's pane: make `dir` and point TMPDIR/TMP/TEMP at it.
pub fn export_line(dir: &Path) -> String {
    let q = format!("'{}'", dir.display().to_string().replace('\'', r"'\''"));
    format!("mkdir -p {q} && export TMPDIR={q} TMP={q} TEMP={q}")
}

/// Removes `dir` and everything in it; a missing one is already removed.
pub fn remove(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// `remove`, a failure logged: the caller's step (a retire, a closed pane) is done regardless.
pub fn remove_logged(dir: &Path) {
    if let Err(e) = remove(dir) {
        eprintln!("agent-fleet: scratch dir {}: {e}", dir.display());
    }
}

/// Bytes under `d` (files only, symlinks not followed); 0 when it is missing.
pub fn size(d: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(d) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(d: &Path, a: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(d)
            .args(a)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn the_dir_is_session_then_name_under_the_base_and_never_leaves_it() {
        let b = Path::new("/c/agent-fleet/tmp");
        assert_eq!(
            dir_in(b, "agent:h/eait/o-139"),
            Some(b.join("eait").join("o-139"))
        );
        for bad in [
            "o-139",
            "agent:h/o-139",
            "agent:h/s/",
            "agent:h/../x",
            "agent:h/s/..",
        ] {
            assert_eq!(dir_in(b, bad), None, "{bad}");
        }
    }

    #[test]
    fn the_base_is_the_cache_dir_not_a_checkout() {
        let b = cache_base().unwrap();
        assert!(b.ends_with("agent-fleet/tmp"), "{}", b.display());
        let home = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();
        assert!(b.starts_with(home), "{}", b.display());
    }

    #[test]
    fn export_line_points_all_three_vars_at_the_dir_and_quotes() {
        let l = export_line(Path::new("/c/it's/o-1"));
        for v in ["TMPDIR", "TMP", "TEMP"] {
            assert!(l.contains(&format!("{v}='/c/it'\\''s/o-1'")), "{l}");
        }
    }

    #[test]
    fn size_sums_nested_files_and_is_zero_when_missing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/x"), [0u8; 10]).unwrap();
        std::fs::write(d.path().join("a/b/y"), [0u8; 5]).unwrap();
        assert_eq!(size(d.path()), 15);
        assert_eq!(size(&d.path().join("nope")), 0);
    }

    #[test]
    fn remove_takes_the_dir_and_its_files_and_a_missing_one_is_fine() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("s/o-1");
        std::fs::create_dir_all(s.join("deep")).unwrap();
        std::fs::write(s.join("deep/x"), "x").unwrap();
        remove(&s).unwrap();
        assert!(!s.exists());
        remove(&s).unwrap();
    }

    /// The agent's case end to end: a shell mktemp after the export line lands in the scratch
    /// dir, and the worktree it ran in shows nothing for git or a tree walk to see.
    #[test]
    fn the_export_line_makes_a_shell_mktemp_land_outside_the_worktree() {
        let d = tempfile::tempdir().unwrap();
        let wt = d.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        assert!(git(&wt, &["init", "-q"]));
        let scratch = dir_in(&d.path().join("cache"), "agent:h/s/o-1").unwrap();
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{} && mktemp \"$TMPDIR/x.XXXXXX\"",
                export_line(&scratch)
            ))
            .current_dir(&wt)
            .env_remove("TMPDIR")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let made = String::from_utf8(out.stdout).unwrap();
        for p in made.lines() {
            assert!(Path::new(p).starts_with(&scratch), "{made}");
        }
        assert_eq!(
            std::fs::read_dir(&wt)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect::<Vec<_>>(),
            [".git"],
            "the worktree holds only its .git"
        );
    }
}
