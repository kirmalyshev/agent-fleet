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

fn git_head() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
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
    let words: Vec<&str> = says.split_whitespace().collect();
    assert_eq!(words[0], "agent-fleet", "{says}");
    assert_eq!(words[1], env!("CARGO_PKG_VERSION"), "{says}");
    match git_head() {
        // Built in a checkout: the last word is the commit the hub's follow check reads.
        Some(_) => {
            assert_eq!(words.len(), 3, "{says}");
            assert_eq!(words[2].len(), 40, "{says}");
            assert!(words[2].chars().all(|c| c.is_ascii_hexdigit()), "{says}");
        }
        None => assert_eq!(words.len(), 2, "no git: no commit: {says}"),
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

const TASK: &str = r#"{"id":"01J0000000000000000000000B","v":1,"type":"task.created","ts":"2026-11-16T12:00:00Z","actor":{"kind":"agent","id":"agent:h/s/a"},"project":"p","task":"task:p/1","data":{"title":"t"}}"#;

#[test]
fn hub_check_reads_a_log_a_live_hub_holds_open_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("hub.db");
    // As the hub keeps it: WAL, a connection open, the event still in the -wal file.
    let live = rusqlite::Connection::open(&db).unwrap();
    live.pragma_update(None, "journal_mode", "WAL").unwrap();
    live.execute_batch(
        "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
         type TEXT NOT NULL, json TEXT NOT NULL, at INTEGER);
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    live.execute(
        "INSERT INTO events (id, type, json) VALUES (?1, 'task.created', ?2)",
        ["01J0000000000000000000000B", TASK],
    )
    .unwrap();
    let out = run(
        dir.path(),
        &["hub", "--check", "--db", db.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok: 1 events");
    drop(live);
}

#[test]
fn hub_check_reads_a_log_from_a_build_before_the_at_column_and_refuses_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.db");
    let conn = rusqlite::Connection::open(&old).unwrap();
    conn.execute_batch(
        "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
         type TEXT NOT NULL, json TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events (id, type, json) VALUES (?1, 'task.created', ?2)",
        ["01J0000000000000000000000B", TASK],
    )
    .unwrap();
    drop(conn);
    let out = run(
        dir.path(),
        &["hub", "--check", "--db", old.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok: 1 events");

    let junk = dir.path().join("junk.db");
    std::fs::write(
        &junk,
        "this is not a database, but it is long enough to be read as one ".repeat(40),
    )
    .unwrap();
    let out = run(
        dir.path(),
        &["hub", "--check", "--db", junk.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).trim().is_empty());
}

#[test]
fn coordinator_claude_execs_claude_with_the_prompt_and_refuses_other_programs() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    let dump = home.path().join("argv");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", dump.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let go = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-fleet"))
            .args(args)
            .env("HOME", home.path())
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("PATH", &path)
            .env_remove("AGENT_FLEET_INSTANCE")
            .output()
            .unwrap()
    };
    assert!(go(&["init"]).status.success());
    assert!(go(&["coordinator", "claude"]).status.success());
    let argv = std::fs::read_to_string(&dump).unwrap();
    assert!(argv.starts_with("--append-system-prompt\n"), "{argv}");
    assert!(argv.contains("name: coordinator") && argv.contains("root-coordinator"));
    let out = go(&["coordinator", "codex"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not supported"));
    assert!(!go(&["coordinator", "--pane", "p1"]).status.success());
}
