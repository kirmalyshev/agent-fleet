//! `./dev clean` against a throwaway repository and a fake `gh`.

use std::path::Path;
use std::process::{Child, Command, Output};

fn run(dir: &Path, prog: &str, args: &[&str]) -> Output {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GH", dir.parent().unwrap().join("gh"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{prog} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(run(dir, "git", args).stdout)
        .unwrap()
        .trim()
        .to_string()
}

/// A branch with one commit of its own, optionally in a worktree and pushed.
fn branch(repo: &Path, name: &str, worktree: bool, push: bool) -> String {
    let dir = if worktree {
        let wt = repo.parent().unwrap().join(name);
        git(
            repo,
            &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()],
        );
        wt
    } else {
        git(repo, &["switch", "-q", "-c", name]);
        repo.to_path_buf()
    };
    std::fs::write(dir.join(name), name).unwrap();
    git(&dir, &["add", name]);
    git(&dir, &["commit", "-q", "-m", name]);
    if push {
        git(&dir, &["push", "-q", "origin", name]);
    }
    let tip = git(&dir, &["rev-parse", "HEAD"]);
    if !worktree {
        git(repo, &["switch", "-q", "main"]);
    }
    tip
}

#[test]
fn clean_retires_finished_work_and_keeps_the_rest() {
    let t = tempfile::tempdir().unwrap();
    let (origin, repo) = (t.path().join("origin.git"), t.path().join("repo"));
    git(
        t.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(
        t.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["switch", "-q", "-c", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "root"]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let merged = branch(&repo, "merged", true, true); // squash-merged PR: worktree retires
    let open = branch(&repo, "open", true, true); // open PR: kept
    let dirty = branch(&repo, "dirty", true, true); // merged PR, uncommitted file: kept
    std::fs::write(t.path().join("dirty/scratch"), "x").unwrap();
    let busy = branch(&repo, "busy", true, true); // merged PR, a process in it: kept
    branch(&repo, "local", false, false); // local-only commit: kept
    branch(&repo, "pushed", false, true); // on origin: deleted
    let gh = format!(
        "#!/bin/sh\nprintf '%s\\n' 'merged MERGED {merged} 1' 'open OPEN {open} 2' \
         'dirty MERGED {dirty} 3' 'busy MERGED {busy} 4'\n"
    );
    let gh_path = t.path().join("gh");
    std::fs::write(&gh_path, gh).unwrap();
    run(t.path(), "chmod", &["+x", gh_path.to_str().unwrap()]);
    let mut sleeper: Child = Command::new("sleep")
        .arg("60")
        .current_dir(t.path().join("busy"))
        .spawn()
        .unwrap();
    let lsof = Command::new("lsof").arg("-v").output().is_ok();

    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("dev");
    let dry = String::from_utf8(run(&repo, dev.to_str().unwrap(), &["clean"]).stdout).unwrap();
    let out = String::from_utf8(run(&repo, dev.to_str().unwrap(), &["clean", "--execute"]).stdout)
        .unwrap();
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();

    assert!(dry.contains("dry run"), "{dry}");
    assert!(out.contains("[merged: pr]"), "{out}");
    assert!(out.contains("delete pushed (pushed)"), "{out}");
    assert!(out.contains("uncommitted changes"), "{out}");
    let branches = git(&repo, &["branch", "--format=%(refname:short)"]);
    let has = |b: &str| branches.lines().any(|l| l == b);
    assert!(!has("merged") && !has("pushed"), "{branches}");
    assert!(
        has("open") && has("dirty") && has("local") && has("main"),
        "{branches}"
    );
    assert!(!t.path().join("merged").exists());
    assert!(t.path().join("open").exists() && t.path().join("dirty").exists());
    if lsof {
        assert!(out.contains("a process runs in it"), "{out}");
        assert!(has("busy") && t.path().join("busy").exists(), "{branches}");
    }
}
