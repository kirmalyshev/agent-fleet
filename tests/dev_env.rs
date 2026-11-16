//! `agent-fleet dev` against throwaway repositories: a dummy service, real process groups, ports
//! above 20000, a temporary data dir and claims directory.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Repo {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    main: PathBuf,
    base: u16,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A free port with room above it for the test's slots.
fn free_port() -> u16 {
    loop {
        let p = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if (20000..65000).contains(&p) {
            return p;
        }
    }
}

fn config(base: u16, health: &str, timeout: u32) -> String {
    format!(
        r#"[slots]
max = 3
step = 1
ports = {{ app = {base} }}

[env]
WHO = "w_{{id}}_{{slot}}"

[services.app]
run = "echo port $PORT; echo started > {{root}}/.started; exec sleep 60"
health = "{health}"
timeout = {timeout}
env = {{ PORT = "{{port.app}}" }}
url = "http://localhost:{{port.app}}"
"#
    )
}

fn repo(health: &str, timeout: u32) -> Repo {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let main = tmp.path().join("main");
    fs::create_dir_all(main.join(".agent-fleet")).unwrap();
    let base = free_port();
    fs::write(
        main.join(".agent-fleet/dev.toml"),
        config(base, health, timeout),
    )
    .unwrap();
    fs::write(main.join(".gitignore"), ".started\n").unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "init"]);
    Repo {
        _tmp: tmp,
        home,
        main,
        base,
    }
}

impl Repo {
    fn dev(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-fleet"))
            .arg("dev")
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("AGENT_FLEET_HOME", self.home.join("data"))
            .env("AGENT_FLEET_DEV_SHARED", self.home.join("shared"))
            .env_remove("AGENT_FLEET_INSTANCE")
            .output()
            .unwrap()
    }

    fn ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.dev(dir, args);
        assert!(
            out.status.success(),
            "dev {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn info(&self, dir: &Path) -> serde_json::Value {
        serde_json::from_str(&self.ok(dir, &["info"])).unwrap()
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.main.parent().unwrap().join(name);
        git(
            &self.main,
            &["worktree", "add", "-q", path.to_str().unwrap()],
        );
        path
    }
}

fn alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn up_status_restart_down_of_a_dummy_service() {
    let r = repo("test -f {root}/.started", 20);
    let out = r.ok(&r.main, &["up", "--app"]);
    assert!(out.contains("app: healthy"), "{out}");
    assert!(
        out.contains(&format!("http://localhost:{}", r.base)),
        "{out}"
    );

    let info = r.info(&r.main);
    assert_eq!(info["slot"], 0);
    assert_eq!(info["env"]["WHO"], "w_main_0");
    let pid = info["services"]["app"]["pid"].as_i64().unwrap();
    assert!(alive(pid));
    let log = fs::read_to_string(info["services"]["app"]["log"].as_str().unwrap()).unwrap();
    assert!(log.contains(&format!("port {}", r.base)), "{log}");

    let status = r.ok(&r.main, &["status"]);
    assert!(
        status.contains("running") && status.contains("healthy"),
        "{status}"
    );
    assert!(!status.contains("env changed"), "{status}");

    // A changed [env] shows until the service restarts with it.
    let toml = r.main.join(".agent-fleet/dev.toml");
    let text = fs::read_to_string(&toml)
        .unwrap()
        .replace("w_{id}", "x_{id}");
    fs::write(&toml, text).unwrap();
    assert!(
        r.ok(&r.main, &["status"])
            .contains("env changed since start")
    );
    r.ok(&r.main, &["restart"]);
    let status = r.ok(&r.main, &["status"]);
    assert!(
        status.contains("running") && !status.contains("env changed"),
        "{status}"
    );
    let pid2 = r.info(&r.main)["services"]["app"]["pid"].as_i64().unwrap();
    assert_ne!(pid, pid2);
    assert!(!alive(pid));

    r.ok(&r.main, &["down"]);
    assert!(!alive(pid2));
    assert!(r.ok(&r.main, &["status"]).contains("stopped"));
    // The recorded set survives `down`: a bare restart brings the service back.
    r.ok(&r.main, &["restart"]);
    assert!(r.ok(&r.main, &["status"]).contains("running"));
    r.ok(&r.main, &["down"]);
}

#[test]
fn a_port_something_else_listens_on_is_refused() {
    let r = repo("true", 20);
    let _held = TcpListener::bind(("127.0.0.1", r.base)).unwrap();
    let out = r.dev(&r.main, &["up", "--app"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("port {} is in use", r.base)), "{err}");
    assert_eq!(r.info(&r.main)["services"]["app"]["running"], false);
}

#[test]
fn a_failed_health_check_prints_the_log_and_stops_the_service() {
    let r = repo("false", 1);
    let out = r.dev(&r.main, &["up", "--app"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not healthy after 1s"), "{err}");
    assert!(err.contains(&format!("port {}", r.base)), "{err}");
    assert_eq!(r.info(&r.main)["services"]["app"]["running"], false);
}

#[test]
fn worktrees_get_their_own_slots_and_ports() {
    let r = repo("true", 20);
    let a = r.worktree("wt-a");
    let b = r.worktree("wt-b");
    let (ia, ib) = (r.info(&a), r.info(&b));
    assert_eq!(
        (ia["slot"].as_u64(), ib["slot"].as_u64()),
        (Some(1), Some(2))
    );
    assert_eq!(ia["ports"]["app"].as_u64(), Some(u64::from(r.base) + 1));
    assert_eq!(ib["ports"]["app"].as_u64(), Some(u64::from(r.base) + 2));
    assert_eq!(ia["env"]["WHO"], "w_wt_a_1");
    // A slot is kept, and freed by --release for the next worktree.
    assert_eq!(r.info(&a)["slot"], 1);
    r.ok(&a, &["down", "--release"]);
    let c = r.worktree("wt-c");
    assert_eq!(r.info(&c)["slot"], 1);
    assert_eq!(r.info(&r.main)["slot"], 0);
    // Both run at once on their own ports.
    r.ok(&b, &["up", "--all"]);
    r.ok(&c, &["up", "app"]);
    assert_eq!(r.info(&b)["services"]["app"]["running"], true);
    assert_eq!(r.info(&c)["services"]["app"]["running"], true);
    r.ok(&b, &["down"]);
    assert_eq!(r.info(&c)["services"]["app"]["running"], true);
    r.ok(&c, &["down"]);
}
