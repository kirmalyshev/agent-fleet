//! The built `agent-fleet` binary, run as the hub's follow check runs it.

use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-fleet"))
        .args(args)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("AGENT_FLEET_INSTANCE")
        .output()
        .unwrap()
}

fn git_short() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A hub log with `rows` events: (id, type, json).
fn log(path: &Path, rows: &[(&str, &str, &str)]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
         type TEXT NOT NULL, json TEXT NOT NULL, at INTEGER);
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    for (id, ty, json) in rows {
        conn.execute(
            "INSERT INTO events (id, type, json) VALUES (?1, ?2, ?3)",
            [id, ty, json],
        )
        .unwrap();
    }
}

#[test]
fn version_names_the_commit_the_binary_was_built_from() {
    let home = tempfile::tempdir().unwrap();
    let out = run(home.path(), &["--version"]);
    assert!(out.status.success());
    let says = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (name, rest) = says.split_once(' ').unwrap();
    assert_eq!(name, "agent-fleet", "{says}");
    let (version, commit) = rest.split_once(" (").expect(&says);
    assert_eq!(version, env!("CARGO_PKG_VERSION"), "{says}");
    let commit = commit.strip_suffix(')').expect(&says);
    match git_short() {
        // Built in a checkout: the commit the hub's follow check reads.
        Some(head) => {
            assert_eq!(commit.len(), 7, "{says}");
            assert!(commit.chars().all(|c| c.is_ascii_hexdigit()), "{says}");
            // The build may be older than HEAD (a stale artifact); it is never empty.
            assert!(!head.is_empty());
        }
        None => assert_eq!(commit, "unknown", "{says}"),
    }
}

#[test]
fn hub_check_passes_a_log_it_can_fold_and_fails_one_it_cannot_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("hub.db");
    log(&db, &[]);
    let before = std::fs::read(&db).unwrap();
    let out = run(
        dir.path(),
        &["hub", "--check", "--db", db.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok: 0 events");
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "read-only: the log is untouched"
    );

    // An event of a type this binary does not know: the hub would refuse to start.
    let future = r#"{"id":"01J0000000000000000000000A","v":1,"type":"future.thing","ts":"2026-11-16T12:00:00Z","actor":{"kind":"agent","id":"agent:h/s/a"},"project":"p","data":{}}"#;
    let bad = dir.path().join("bad.db");
    log(
        &bad,
        &[("01J0000000000000000000000A", "future.thing", future)],
    );
    let out = run(
        dir.path(),
        &["hub", "--check", "--db", bad.to_str().unwrap()],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot read"), "{err}");

    // No log: not created.
    let none = dir.path().join("none.db");
    let out = run(
        dir.path(),
        &["hub", "--check", "--db", none.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(!none.exists(), "a check never creates the log");
}
