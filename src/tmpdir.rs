//! Per-agent scratch dir: `<worktree>/.tmp`, exported as TMPDIR/TMP/TEMP into the agent's pane at
//! spawn so test mktemps and tool scratch land inside the worktree and go with it on retire.

use std::path::{Path, PathBuf};

/// The agent's scratch dir inside `worktree`.
pub fn dir(worktree: &Path) -> PathBuf {
    worktree.join(".tmp")
}

/// Shell line for the agent's pane: make the dir, ignore everything in it for git (its own
/// `.gitignore`, so `retire` does not count scratch as work), and point TMPDIR/TMP/TEMP at it.
pub fn export_line(worktree: &Path) -> String {
    let d = dir(worktree).display().to_string();
    let q = format!("'{}'", d.replace('\'', r"'\''"));
    let g = format!(
        "'{}'",
        dir(worktree)
            .join(".gitignore")
            .display()
            .to_string()
            .replace('\'', r"'\''")
    );
    format!("mkdir -p {q} && echo '*' > {g} && export TMPDIR={q} TMP={q} TEMP={q}")
}

/// Scratch in use under `<worktree>/.tmp`: `size` less the `.gitignore` the export line writes.
pub fn used(worktree: &Path) -> u64 {
    let d = dir(worktree);
    let own = std::fs::metadata(d.join(".gitignore")).map_or(0, |m| m.len());
    size(&d).saturating_sub(own)
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
    fn used_ignores_the_gitignore_the_export_line_writes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir(d.path())).unwrap();
        std::fs::write(dir(d.path()).join(".gitignore"), "*\n").unwrap();
        assert_eq!(used(d.path()), 0);
        std::fs::write(dir(d.path()).join("x"), [0u8; 7]).unwrap();
        assert_eq!(used(d.path()), 7);
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
        // scratch is not work to retire
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
        // removing the worktree removes the scratch with it
        assert!(git(&main, &["worktree", "remove", wt.to_str().unwrap()]));
        assert!(!dir(&wt).exists());
    }
}
