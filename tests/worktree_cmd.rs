//! `agent-fleet worktree clean` over two connected throwaway repositories and a fake `gh`.

use std::path::Path;
use std::process::{Command, Output};

fn ok(out: Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    ok(out, &format!("git {args:?}")).trim().to_string()
}

/// A clone of a new bare origin at `<t>/<name>`, with one commit on `main`, pushed.
fn repo(t: &Path, name: &str) -> std::path::PathBuf {
    let (origin, repo) = (t.join(format!("{name}.git")), t.join(name));
    git(
        t,
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
        t,
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
    repo
}

/// A branch with one commit of its own, optionally in a worktree beside `repo` and pushed.
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

fn pr(branch: &str, state: &str, tip: &str, n: u32) -> String {
    format!(r#"{{"headRefName":"{branch}","state":"{state}","headRefOid":"{tip}","number":{n}}}"#)
}

#[test]
fn clean_retires_finished_work_in_every_connected_repo_and_keeps_the_rest() {
    let t = tempfile::tempdir().unwrap();
    let t = t.path();
    let a = repo(t, "a");
    let merged = branch(&a, "merged", true, true); // squash-merged PR: worktree retires
    let open = branch(&a, "open", true, true); // open PR: kept
    let dirty = branch(&a, "dirty", true, true); // merged PR, uncommitted file: kept
    std::fs::write(t.join("dirty/scratch"), "x").unwrap();
    let locked = branch(&a, "locked", true, true); // merged PR, its subagent's lock: kept
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    let reason = format!("claude agent a1 (pid {pid} start 1)");
    let wt = t.join("locked");
    git(
        &a,
        &[
            "worktree",
            "lock",
            "--reason",
            &reason,
            wt.to_str().unwrap(),
        ],
    );
    branch(&a, "local", false, false); // local-only commit: kept
    branch(&a, "pushed", false, true); // on origin: deleted
    let b = repo(t, "b");
    let closed = branch(&b, "closed", true, false); // closed PR, never pushed: retires

    let a_prs = [
        pr("merged", "MERGED", &merged, 1),
        pr("open", "OPEN", &open, 2),
        pr("dirty", "MERGED", &dirty, 3),
        pr("locked", "MERGED", &locked, 4),
    ];
    let gh = format!(
        "#!/bin/sh\ncase \"$*\" in\n*owner/a*) echo '[{}]' ;;\n*owner/b*) echo '[{}]' ;;\n\
         *) echo '[]' ;;\nesac\n",
        a_prs.join(","),
        pr("closed", "CLOSED", &closed, 9)
    );
    let gh_path = t.join("gh");
    std::fs::write(&gh_path, gh).unwrap();
    Command::new("chmod")
        .args(["+x", gh_path.to_str().unwrap()])
        .status()
        .unwrap();

    let config = t.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("fleet.toml"),
        format!(
            "[repos]\n\"owner/a\" = \"{}\"\n\"owner/b\" = \"{}\"\n",
            a.display(),
            b.display()
        ),
    )
    .unwrap();
    // No hub answers on port 1: the command reads the (empty) local replica instead.
    let home = t.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("token"), "t").unwrap();
    std::fs::write(home.join("replica.db"), "").unwrap();
    let fleet = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_agent-fleet"))
            .args(["--hub", "http://127.0.0.1:1", "--host-name", "h"])
            .args(args)
            .current_dir(&a)
            .env("AGENT_FLEET_HOME", &home)
            .env("AGENT_FLEET_CONFIG_DIR", &config)
            .env("AGENT_FLEET_GH", &gh_path)
            .env_remove("AGENT_FLEET_INSTANCE")
            .output()
            .unwrap();
        ok(out, &format!("agent-fleet {args:?}"))
    };

    let ls = fleet(&["worktree", "ls"]);
    assert!(ls.contains("owner/a") && ls.contains("owner/b"), "{ls}");
    assert!(ls.contains("#2 open"), "{ls}");
    let dry = fleet(&["worktree", "clean"]);
    assert!(dry.contains("dry run"), "{dry}");
    assert!(t.join("merged").exists(), "a dry run removes nothing");
    let only_b = fleet(&["worktree", "clean", "--repo", "owner/b"]);
    assert!(!only_b.contains("owner/a"), "{only_b}");

    let out = fleet(&["worktree", "clean", "--execute"]);
    assert!(out.contains("[merged: pr]"), "{out}");
    assert!(out.contains("[closed: pr]"), "{out}");
    assert!(out.contains("delete pushed (pushed)"), "{out}");
    assert!(out.contains("uncommitted files"), "{out}");
    assert!(
        out.contains(&format!("locked, stale (pid {pid} gone): git -C ")),
        "{out}"
    );
    let has = |repo: &Path, b: &str| {
        git(repo, &["branch", "--format=%(refname:short)"])
            .lines()
            .any(|l| l == b)
    };
    assert!(
        !has(&a, "merged") && !has(&a, "pushed") && !has(&b, "closed"),
        "{out}"
    );
    for kept in ["open", "dirty", "locked", "local", "main"] {
        assert!(has(&a, kept), "{kept}: {out}");
    }
    assert!(!t.join("merged").exists() && !t.join("closed").exists());
    assert!(t.join("open").exists() && t.join("dirty").exists() && t.join("locked").exists());
}
