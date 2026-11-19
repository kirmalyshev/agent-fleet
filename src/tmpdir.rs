//! Per-agent scratch dir: `<worktree>/.tmp`, exported as TMPDIR/TMP/TEMP into the agent's pane at
//! spawn so test mktemps and tool scratch land inside the worktree and go with it on retire.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The agent's scratch dir inside `worktree`.
pub fn dir(worktree: &Path) -> PathBuf {
    worktree.join(".tmp")
}

/// Shell line for the agent's pane: make the dir and point TMPDIR/TMP/TEMP at it.
pub fn export_line(worktree: &Path) -> String {
    let d = dir(worktree).display().to_string();
    let q = format!("'{}'", d.replace('\'', r"'\''"));
    format!("mkdir -p {q} && export TMPDIR={q} TMP={q} TEMP={q}")
}

/// Keep `.tmp/` out of `git status` (so retire does not count scratch as work) via the repo's
/// `info/exclude`. Idempotent; the file is shared by all of the repo's worktrees.
pub fn exclude(worktree: &Path) -> std::io::Result<()> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(worktree)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("git rev-parse --git-path failed"));
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let cur = std::fs::read_to_string(&path).unwrap_or_default();
    if cur.lines().any(|l| l == "/.tmp/") {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let sep = if cur.is_empty() || cur.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    std::fs::write(&path, format!("{cur}{sep}/.tmp/\n"))
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

    fn git(d: &Path, a: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(d)
            .args(a)
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn export_line_points_all_three_vars_into_the_worktree_and_quotes() {
        let l = export_line(Path::new("/w/it's"));
        for v in ["TMPDIR", "TMP", "TEMP"] {
            assert!(l.contains(&format!("{v}='/w/it'\\''s/.tmp'")), "{l}");
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
    fn the_export_line_makes_a_shell_mktemp_land_in_tmp_and_retire_removes_it() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("main");
        let wt = d.path().join("wt");
        std::fs::create_dir(&main).unwrap();
        assert!(git(&main, &["init", "-q"]));
        assert!(git(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i"
            ]
        ));
        assert!(git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]));
        exclude(&wt).unwrap();
        exclude(&wt).unwrap(); // idempotent
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{} && mktemp \"$TMPDIR/x.XXXXXX\"",
                export_line(&wt)
            ))
            .env_remove("TMPDIR")
            .output()
            .unwrap();
        let made = String::from_utf8(out.stdout).unwrap();
        assert!(Path::new(made.trim()).starts_with(dir(&wt)), "{made}");
        // scratch is not work to retire, and the exclude line is written once
        let st = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            st.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&st.stdout)
        );
        let ex = std::fs::read_to_string(main.join(".git/info/exclude")).unwrap();
        assert_eq!(ex.matches("/.tmp/").count(), 1);
        // removing the worktree removes the scratch with it
        assert!(git(&main, &["worktree", "remove", wt.to_str().unwrap()]));
        assert!(!dir(&wt).exists());
    }
}
