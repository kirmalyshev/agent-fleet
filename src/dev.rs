//! `agent-fleet dev`: a per-worktree dev environment from the repository's
//! `.agent-fleet/dev.toml`. Each worktree gets a slot (its own ports and values); its services run
//! as process groups whose pids, logs and env live in the data dir; infra runs once for all.
//! `docs/DEV.md` is the reference.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rustix::fs::{FlockOperation, flock};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::{self, Held};
use crate::scan::Procs;

pub const CONFIG: &str = ".agent-fleet/dev.toml";
/// Where each Unix user publishes the slots they hold, so users on one machine share ports.
const SHARED: &str = "/var/tmp/agent-fleet-dev";
const INFRA_TIMEOUT: Duration = Duration::from_secs(120);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_GRACE: Duration = Duration::from_secs(10);
const LOG_LINES: usize = 20;

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Start the infra, then the named services (`--api` or `api`, `--all` for every one); bare:
    /// the infra only. Refuses a port something else listens on, waits for health (on failure
    /// prints the log's last lines and exits non-zero) and prints the URLs.
    Up {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        services: Vec<String>,
    },
    /// Stop this worktree's services (the process group: TERM, then KILL), never the infra.
    Down {
        /// Also free the slot and drop this worktree's state and logs.
        #[arg(long)]
        release: bool,
    },
    /// Stop and start the named services; bare: every service `up` ever started here (the set
    /// survives a crash and `down`).
    Restart {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        services: Vec<String>,
    },
    /// Each service: running, healthy, and whether its env changed since it started.
    Status,
    /// Follow one service's log, or all of them.
    Logs { service: Option<String> },
    /// The [env] values for this worktree, one `KEY=value` per line.
    Env,
    /// Each service's URL.
    Url,
    /// Slot, id, ports, env and services as JSON.
    Info,
    /// Run the repository's `[db]` command with this worktree's env. `reset` and `drop` refuse a
    /// database name another worktree, or another user, also uses; `drop` then stops this
    /// worktree's services and frees its slot, as `down --release`.
    Db {
        #[arg(value_enum)]
        action: DbAction,
    },
    /// Run `[test.<name>]` with this worktree's env; the args go after its `run`.
    Test {
        name: String,
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// The fleet's agents in this repository: worktree, slot, branch and the PR's health
    /// (checks, mergeable).
    Agents,
    /// Your open PRs across the connected repositories, with their health and the agent on each.
    Prs,
    /// Drop the slots of deleted worktrees whose services no longer run, with their state and
    /// logs. A deleted worktree whose services still run keeps its slot.
    Prune,
    /// Each worktree: branch, PR, commits not in the default branch, uncommitted files, its slot,
    /// ports and running services, and what `clean` would do with it.
    Ls {
        /// Only this `owner/repo`.
        #[arg(long)]
        repo: Option<String>,
    },
    /// What clean would do (a dry run). A branch is finished when its tip is the head of one of
    /// your merged or closed PRs, is in the default branch, or is in `origin/<branch>`; one with
    /// an open PR or local-only commits never is. Only a finished PR retires a worktree, and
    /// never the main checkout, a locked one, the one you are in, one an agent works in, or one
    /// with uncommitted files. The processes left in a worktree it retires are stopped first;
    /// a herdr pane, an agent's processes and your own keep the worktree.
    Clean {
        /// Only this `owner/repo`.
        #[arg(long)]
        repo: Option<String>,
        /// Stop the leftover processes (TERM, then KILL after 5 s), retire the worktrees
        /// (`.agent-fleet/hook retire`, else `git worktree remove`) and delete the branches.
        #[arg(long)]
        execute: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum DbAction {
    Setup,
    Reset,
    Drop,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    slots: Slots,
    #[serde(default)]
    env: BTreeMap<String, String>,
    infra: Option<Infra>,
    #[serde(default)]
    services: BTreeMap<String, Service>,
    db: Option<Db>,
    #[serde(default)]
    test: BTreeMap<String, Test>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Db {
    /// The database this worktree uses: what `reset` and `drop` check against other worktrees.
    name: String,
    setup: Option<String>,
    reset: Option<String>,
    drop: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Test {
    dir: Option<String>,
    run: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Slots {
    max: u32,
    step: u32,
    ports: BTreeMap<String, u16>,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            max: 31,
            step: 10,
            ports: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Infra {
    up: Option<String>,
    #[serde(default)]
    health: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    dir: Option<String>,
    before: Option<String>,
    run: String,
    health: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    url: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    60
}

impl Service {
    /// Every template of the service, with the key it sits under.
    fn templates(&self) -> Vec<(&'static str, &str)> {
        let mut v = vec![("run", self.run.as_str())];
        for (k, t) in [
            ("dir", &self.dir),
            ("before", &self.before),
            ("health", &self.health),
            ("url", &self.url),
        ] {
            if let Some(t) = t {
                v.push((k, t));
            }
        }
        v.extend(self.env.values().map(|t| ("env", t.as_str())));
        v
    }
}

/// What a template may name: `{slot}`, `{id}`, `{root}`, `{state}`, `{port.<name>}`.
struct Vars {
    slot: u32,
    id: String,
    root: String,
    state: String,
    ports: BTreeMap<String, u16>,
}

impl Vars {
    fn new(cfg: &Config, slot: u32, id: &str, root: &Path, state: &Path) -> Result<Self> {
        let ports = cfg
            .slots
            .ports
            .iter()
            .map(|(name, base)| {
                let p = u64::from(*base) + u64::from(cfg.slots.step) * u64::from(slot);
                let p = u16::try_from(p)
                    .map_err(|_| anyhow!("port {name} is {p} at slot {slot}, past 65535"))?;
                Ok((name.clone(), p))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            slot,
            id: id.to_string(),
            root: root.display().to_string(),
            state: state.display().to_string(),
            ports,
        })
    }
}

/// Fills the placeholders in `s`. Any other `{…}` (a shell's `${VAR}`) stays as it is.
fn render(s: &str, v: &Vars) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find('}') else { break };
        let val = match &rest[1..end] {
            "slot" => Some(v.slot.to_string()),
            "id" => Some(v.id.clone()),
            "root" => Some(v.root.clone()),
            "state" => Some(v.state.clone()),
            key => match key.strip_prefix("port.") {
                Some(name) => Some(
                    v.ports
                        .get(name)
                        .ok_or_else(|| anyhow!("unknown port {name:?}; [slots] ports names it"))?
                        .to_string(),
                ),
                None => None,
            },
        };
        match val {
            Some(val) => {
                out.push_str(&val);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The port names a template refers to.
fn port_names(s: &str) -> impl Iterator<Item = &str> {
    s.split("{port.")
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
}

pub fn parse(text: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(text).map_err(|e| anyhow!("{CONFIG}: {e}"))?;
    for name in cfg.services.keys() {
        if name == "all"
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            bail!("{CONFIG}: service name {name:?}: letters, digits, - and _, and not \"all\"");
        }
    }
    // Every template must render at every slot: catches a misspelt port and an overflow now.
    let vars = Vars::new(
        &cfg,
        cfg.slots.max,
        "id",
        Path::new("/root"),
        Path::new("/state"),
    )
    .map_err(|e| anyhow!("{CONFIG}: [slots] {e}"))?;
    for (k, t) in &cfg.env {
        render(t, &vars).map_err(|e| anyhow!("{CONFIG}: env.{k}: {e}"))?;
    }
    for (k, t) in cfg.infra.iter().flat_map(|i| {
        i.up.iter()
            .map(|t| ("up", t))
            .chain(i.health.iter().map(|t| ("health", t)))
    }) {
        render(t, &vars).map_err(|e| anyhow!("{CONFIG}: infra.{k}: {e}"))?;
    }
    for (name, s) in &cfg.services {
        for (k, t) in s.templates() {
            render(t, &vars).map_err(|e| anyhow!("{CONFIG}: services.{name}.{k}: {e}"))?;
        }
    }
    if let Some(d) = &cfg.db {
        let all = [
            ("name", Some(&d.name)),
            ("setup", d.setup.as_ref()),
            ("reset", d.reset.as_ref()),
            ("drop", d.drop.as_ref()),
        ];
        for (k, t) in all.into_iter().filter_map(|(k, t)| Some((k, t?))) {
            render(t, &vars).map_err(|e| anyhow!("{CONFIG}: db.{k}: {e}"))?;
        }
    }
    for (name, t) in &cfg.test {
        let all = [("run", Some(&t.run)), ("dir", t.dir.as_ref())]
            .into_iter()
            .filter_map(|(k, t)| Some((k, t?)))
            .chain(t.env.values().map(|t| ("env", t)));
        for (k, t) in all {
            render(t, &vars).map_err(|e| anyhow!("{CONFIG}: test.{name}.{k}: {e}"))?;
        }
    }
    Ok(cfg)
}

/// The services named on the command line (`--api`, `api`, `--all`).
fn pick(args: &[String], cfg: &Config) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for a in args {
        let name = a.trim_start_matches('-');
        if name == "all" {
            out.extend(cfg.services.keys().cloned());
        } else if cfg.services.contains_key(name) {
            out.push(name.to_string());
        } else {
            let known: Vec<&str> = cfg.services.keys().map(String::as_str).collect();
            bail!("no service {a:?} in {CONFIG}; it has: {}", known.join(", "));
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|n| seen.insert(n.clone()));
    Ok(out)
}

/// What `restart` restarts: the named services, or the recorded set.
fn restart_set(asked: Vec<String>, recorded: &BTreeSet<String>) -> Vec<String> {
    if asked.is_empty() {
        recorded.iter().cloned().collect()
    } else {
        asked
    }
}

/// The lowest slot ≥ 1 that nobody holds.
fn choose(taken: &BTreeSet<u32>, max: u32) -> Option<u32> {
    (1..=max).find(|s| !taken.contains(s))
}

fn lock(path: &Path) -> Result<Held> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    flock(&f, FlockOperation::LockExclusive)?;
    Ok(Held(f))
}

fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("read {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Temp file + rename. In a sticky shared directory the temp file is created anew (no symlink
/// followed), and the rename cannot replace another user's file.
fn write_json(path: &Path, v: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(v)?.as_bytes())?;
    fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// The machine-wide claims directory: created sticky and world-writable when missing; refused
/// when another user (not root) owns it, or anyone may write it without the sticky bit.
fn shared_dir(dir: &Path) -> Result<()> {
    if fs::symlink_metadata(dir).is_err() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o1777))?;
    }
    let m = fs::symlink_metadata(dir)?;
    let open = m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0;
    if !m.is_dir() || (m.uid() != 0 && m.uid() != uid()) || open {
        bail!(
            "refusing {}: it must be a directory owned by root or you, sticky if others may \
             write it (sudo install -d -m 1777 {0})",
            dir.display()
        );
    }
    Ok(())
}

/// The slots other users published.
fn others(shared: &Path) -> BTreeSet<u32> {
    let mine = format!("{}.json", uid());
    fs::read_dir(shared)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.ends_with(".json") && !n.ends_with(DBS) && n != mine
        })
        .filter_map(|e| serde_json::from_slice::<Vec<u32>>(&fs::read(e.path()).ok()?).ok())
        .flatten()
        .collect()
}

/// The suffix of the files each user publishes their database names to, next to their slots.
const DBS: &str = ".dbs.json";

/// The users other than you who published `name` as a database they use, as `uid <n>`.
fn others_using(shared: &Path, name: &str) -> Vec<String> {
    let mine = format!("{}{DBS}", uid());
    fs::read_dir(shared)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            let user = n.strip_suffix(DBS).filter(|_| n != mine)?.to_string();
            let names: Vec<String> = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
            names
                .iter()
                .any(|x| x == name)
                .then(|| format!("uid {user}"))
        })
        .collect()
}

/// Where a user's slots live: `slots.json` (worktree path → slot) in their data dir, and the
/// shared directory every user's claims are published to.
struct Registry<'a> {
    dir: &'a Path,
    shared: &'a Path,
}

impl Registry<'_> {
    /// This worktree's slot: the one it holds, else the lowest free one in every user's claims.
    fn claim(&self, root: &str, max: u32) -> Result<u32> {
        let _l = lock(&self.dir.join("slots.lock"))?;
        let mut reg: BTreeMap<String, u32> = read_json(&self.dir.join("slots.json"))?;
        if let Some(s) = reg.get(root) {
            return Ok(*s);
        }
        shared_dir(self.shared)?;
        let _m = lock(&self.shared.join(".lock"))?;
        let mut taken: BTreeSet<u32> = reg.values().copied().collect();
        taken.extend(others(self.shared));
        let slot = choose(&taken, max)
            .ok_or_else(|| anyhow!("no free slot: all of 1..={max} are taken ([slots] max)"))?;
        reg.insert(root.to_string(), slot);
        self.save(&reg)?;
        Ok(slot)
    }

    /// Frees `root`'s slot and forgets its database.
    fn release(&self, root: &str) -> Result<()> {
        let _l = lock(&self.dir.join("slots.lock"))?;
        let mut reg: BTreeMap<String, u32> = read_json(&self.dir.join("slots.json"))?;
        let mut dbs: BTreeMap<String, String> = read_json(&self.dir.join("dbs.json"))?;
        let slot = reg.remove(root).is_some();
        let db = dbs.remove(root).is_some();
        if slot || db {
            shared_dir(self.shared)?;
            let _m = lock(&self.shared.join(".lock"))?;
            if slot {
                self.save(&reg)?;
            }
            if db {
                self.save_dbs(&dbs)?;
            }
        }
        Ok(())
    }

    /// Records that `root` uses the database `name`, and publishes it for the other users.
    fn use_db(&self, root: &str, name: &str) -> Result<()> {
        let _l = lock(&self.dir.join("slots.lock"))?;
        let mut dbs: BTreeMap<String, String> = read_json(&self.dir.join("dbs.json"))?;
        if dbs.get(root).is_some_and(|n| n == name) {
            return Ok(());
        }
        dbs.insert(root.to_string(), name.to_string());
        shared_dir(self.shared)?;
        let _m = lock(&self.shared.join(".lock"))?;
        self.save_dbs(&dbs)
    }

    /// Who else uses the database `name`: your other worktrees that still exist, and other
    /// users.
    fn db_users(&self, root: &str, name: &str) -> Result<Vec<String>> {
        let dbs: BTreeMap<String, String> = read_json(&self.dir.join("dbs.json"))?;
        let mut out: Vec<String> = dbs
            .into_iter()
            .filter(|(r, n)| r != root && n == name && Path::new(r).exists())
            .map(|(r, _)| r)
            .collect();
        out.extend(others_using(self.shared, name));
        Ok(out)
    }

    /// The caller holds both locks. Worktrees that are gone are not published.
    fn save_dbs(&self, dbs: &BTreeMap<String, String>) -> Result<()> {
        write_json(&self.dir.join("dbs.json"), dbs)?;
        let mine: BTreeSet<&String> = dbs
            .iter()
            .filter(|(r, _)| Path::new(r).exists())
            .map(|(_, n)| n)
            .collect();
        write_json(&self.shared.join(format!("{}{DBS}", uid())), &mine)
    }

    fn save(&self, reg: &BTreeMap<String, u32>) -> Result<()> {
        write_json(&self.dir.join("slots.json"), reg)?;
        let mine: BTreeSet<u32> = reg.values().copied().collect();
        write_json(&self.shared.join(format!("{}.json", uid())), &mine)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Recorded {
    /// Every service `up` started here: what a bare `restart` restarts.
    #[serde(default)]
    set: BTreeSet<String>,
    #[serde(default)]
    running: BTreeMap<String, Run>,
}

#[derive(Serialize, Deserialize)]
struct Run {
    /// The process group, led by the service's shell.
    pid: i32,
    /// When the leader started: tells this group from a later one that reuses its id.
    #[serde(default)]
    started: Option<DateTime<Utc>>,
    env: BTreeMap<String, String>,
}

fn alive(pgid: i32) -> bool {
    pgid > 1 && unsafe { libc::kill(-pgid, 0) } == 0
}

/// The group is still the one the service started: its leader runs with the recorded start
/// time, or, the leader gone, every member started after it. Anything else is a reused id.
fn ours(r: &Run, procs: &Procs) -> bool {
    let Some(started) = r.started else {
        return alive(r.pid);
    };
    let pgid = r.pid as u32;
    if let Some(leader) = procs.0.get(&pgid) {
        return leader.pgid == Some(pgid) && leader.started == Some(started);
    }
    let mut members = procs.0.values().filter(|p| p.pgid == Some(pgid)).peekable();
    members.peek().is_some() && members.all(|p| p.started.is_some_and(|s| s >= started))
}

/// TERM to the whole group, then KILL after the grace.
fn stop(pgid: i32) {
    if !alive(pgid) {
        return;
    }
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    let t = Instant::now();
    while alive(pgid) && t.elapsed() < STOP_GRACE {
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(pgid) {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// Something listens on the port, or it cannot be bound.
fn busy(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300))
        .is_ok()
        || std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

fn sh(cmd: &str, dir: &Path, env: &BTreeMap<String, String>) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).current_dir(dir).envs(env);
    c
}

/// A health check passes: a URL answers 2xx, or a command exits 0.
async fn healthy(check: &str, dir: &Path, env: &BTreeMap<String, String>) -> bool {
    if check.starts_with("http://") || check.starts_with("https://") {
        return reqwest::Client::new()
            .get(check)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
    }
    let mut c = tokio::process::Command::from(sh(check, dir, env));
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    matches!(
        tokio::time::timeout(CHECK_TIMEOUT, c.status()).await,
        Ok(Ok(s)) if s.success()
    )
}

fn tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(LOG_LINES);
    format!("--- {} ---\n{}", path.display(), lines[from..].join("\n"))
}

struct Worktree {
    root: PathBuf,
    main: bool,
    id: String,
    slot: u32,
    cfg: Config,
    vars: Vars,
    state: PathBuf,
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).output().context("run git")?;
    if !out.status.success() {
        bail!(
            "not in a git worktree: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A short stable name for the worktree: its directory name, lowercased, `_` for the rest.
fn id_of(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

impl Worktree {
    fn open(reg: &Registry) -> Result<Self> {
        let root = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
        let dirs = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])?;
        let main = dirs.lines().next() == dirs.lines().nth(1);
        let file = root.join(CONFIG);
        let text = fs::read_to_string(&file).with_context(|| {
            format!("read {}; this repository has no dev setup", file.display())
        })?;
        let cfg = parse(&text)?;
        let id = id_of(&root);
        let slot = if main {
            0
        } else {
            reg.claim(&root.display().to_string(), cfg.slots.max)?
        };
        let state = reg.dir.join(format!("{slot}-{id}"));
        fs::create_dir_all(&state)?;
        let vars = Vars::new(&cfg, slot, &id, &root, &state)?;
        if let Some(db) = &cfg.db {
            reg.use_db(&root.display().to_string(), &render(&db.name, &vars)?)?;
        }
        Ok(Self {
            root,
            main,
            id,
            slot,
            cfg,
            vars,
            state,
        })
    }

    /// The state, with every service whose group is gone or no longer ours dropped.
    fn recorded(&self) -> Result<Recorded> {
        let mut rec: Recorded = read_json(&self.state.join("state.json"))?;
        let procs = Procs::read();
        rec.running.retain(|_, r| ours(r, &procs));
        Ok(rec)
    }

    fn save(&self, rec: &Recorded) -> Result<()> {
        write_json(&self.state.join("state.json"), rec)
    }

    fn log(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.log"))
    }

    /// [env], then the service's own env on top.
    fn env(&self, service: Option<&str>) -> Result<BTreeMap<String, String>> {
        self.env_with(service.map(|s| &self.cfg.services[s].env))
    }

    /// [env], then `own` on top.
    fn env_with(&self, own: Option<&BTreeMap<String, String>>) -> Result<BTreeMap<String, String>> {
        self.cfg
            .env
            .iter()
            .chain(own.into_iter().flatten())
            .map(|(k, t)| Ok((k.clone(), render(t, &self.vars)?)))
            .collect()
    }

    /// `dir` rendered against the root; the root itself when it is not set.
    fn dir(&self, dir: Option<&String>) -> Result<PathBuf> {
        Ok(match dir {
            Some(d) => self.root.join(render(d, &self.vars)?),
            None => self.root.clone(),
        })
    }

    fn url(&self, name: &str) -> Option<String> {
        let t = self.cfg.services[name].url.as_ref()?;
        render(t, &self.vars).ok()
    }

    /// The ports a service's templates name.
    fn ports_of(&self, name: &str) -> BTreeSet<u16> {
        self.cfg.services[name]
            .templates()
            .into_iter()
            .flat_map(|(_, t)| port_names(t))
            .filter_map(|n| self.vars.ports.get(n).copied())
            .collect()
    }

    fn dir_of(&self, name: &str) -> Result<PathBuf> {
        self.dir(self.cfg.services[name].dir.as_ref())
    }

    async fn infra(&self) -> Result<()> {
        let Some(infra) = &self.cfg.infra else {
            return Ok(());
        };
        let env = self.env(None)?;
        let checks: Vec<String> = infra
            .health
            .iter()
            .map(|t| render(t, &self.vars))
            .collect::<Result<_>>()?;
        let all_ok = || async {
            for c in &checks {
                if !healthy(c, &self.root, &env).await {
                    return Some(c.clone());
                }
            }
            None
        };
        if !checks.is_empty() && all_ok().await.is_none() {
            return Ok(());
        }
        if let Some(up) = &infra.up {
            let up = render(up, &self.vars)?;
            println!("infra: {up}");
            let st = sh(&up, &self.root, &env).stdin(Stdio::null()).status()?;
            if !st.success() {
                bail!("infra: `{up}` failed ({st})");
            }
        }
        let t = Instant::now();
        while let Some(failing) = all_ok().await {
            if t.elapsed() > INFRA_TIMEOUT {
                bail!(
                    "infra not healthy after {}s: {failing}",
                    INFRA_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Starts one service and waits for its health; on failure stops it again.
    async fn start(&self, name: &str, rec: &mut Recorded) -> Result<()> {
        let s = &self.cfg.services[name];
        let env = self.env(Some(name))?;
        // Ports a running service of this worktree names are its own, not a clash.
        let held: BTreeSet<u16> = rec
            .running
            .iter()
            .filter(|(n, _)| *n != name && self.cfg.services.contains_key(*n))
            .flat_map(|(n, _)| self.ports_of(n))
            .collect();
        for port in self.ports_of(name).difference(&held) {
            if busy(*port) {
                bail!("{name}: port {port} is in use by something else; not started");
            }
        }
        let dir = self.dir_of(name)?;
        let path = self.log(name);
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        if let Some(before) = &s.before {
            let before = render(before, &self.vars)?;
            println!("{name}: {before}");
            let st = sh(&before, &dir, &env)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log.try_clone()?)
                .status()?;
            if !st.success() {
                bail!("{name}: `{before}` failed ({st})\n{}", tail(&path));
            }
        }
        let mut child = sh(&render(&s.run, &self.vars)?, &dir, &env)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .with_context(|| format!("{name}: start"))?;
        let pid = child.id() as i32;
        rec.running.insert(
            name.to_string(),
            Run {
                pid,
                started: Procs::read().0.get(&(pid as u32)).and_then(|p| p.started),
                env: env.clone(),
            },
        );
        self.save(rec)?;
        let Some(check) = &s.health else {
            println!("{name}: started (pid {pid})");
            return Ok(());
        };
        let check = render(check, &self.vars)?;
        let t = Instant::now();
        let failed = loop {
            if let Some(st) = child.try_wait()? {
                break format!("exited ({st})");
            }
            if healthy(&check, &dir, &env).await {
                println!("{name}: healthy (pid {pid})");
                return Ok(());
            }
            if t.elapsed() > Duration::from_secs(s.timeout) {
                break format!("not healthy after {}s: {check}", s.timeout);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        // Reap the leader as it dies, so the group does not linger as a zombie.
        std::thread::spawn(move || child.wait());
        stop(pid);
        rec.running.remove(name);
        self.save(rec)?;
        bail!("{name}: {failed}\n{}", tail(&path))
    }

    fn stop_all(&self, rec: &mut Recorded, only: Option<&[String]>) {
        let names: Vec<String> = rec
            .running
            .keys()
            .filter(|n| only.is_none_or(|o| o.contains(n)))
            .cloned()
            .collect();
        for n in names {
            let r = rec.running.remove(&n).expect("listed");
            stop(r.pid);
            println!("{n}: stopped");
        }
    }

    async fn start_each(&self, names: &[String], rec: &mut Recorded) -> Result<()> {
        for name in names {
            rec.set.insert(name.clone());
            if rec.running.contains_key(name) {
                println!("{name}: already running");
                continue;
            }
            let r = self.start(name, rec).await;
            self.save(rec)?;
            r?;
        }
        Ok(())
    }

    /// `down --release`: stop every service, delete the state and logs, free the slot.
    fn release(&self, reg: &Registry) -> Result<()> {
        let _l = lock(&self.state.join("lock"))?;
        let mut rec = self.recorded()?;
        self.stop_all(&mut rec, None);
        fs::remove_dir_all(&self.state)?;
        reg.release(&self.root.display().to_string())?;
        println!("released slot {}", self.slot);
        Ok(())
    }

    /// Runs a `[db]` command; `reset` and `drop` first check nobody else uses the database.
    fn db(&self, reg: &Registry, action: DbAction) -> Result<()> {
        let db = self
            .cfg
            .db
            .as_ref()
            .ok_or_else(|| anyhow!("{CONFIG} has no [db]"))?;
        let (key, cmd) = match action {
            DbAction::Setup => ("setup", &db.setup),
            DbAction::Reset => ("reset", &db.reset),
            DbAction::Drop => ("drop", &db.drop),
        };
        let cmd = cmd
            .as_ref()
            .ok_or_else(|| anyhow!("{CONFIG} has no [db] {key}"))?;
        let name = render(&db.name, &self.vars)?;
        if !matches!(action, DbAction::Setup) {
            let users = reg.db_users(&self.root.display().to_string(), &name)?;
            if !users.is_empty() {
                bail!(
                    "refusing to {key} database {name}: also used by {}",
                    users.join(", ")
                );
            }
        }
        let cmd = render(cmd, &self.vars)?;
        println!("db {key}: {cmd}");
        let st = sh(&cmd, &self.root, &self.env(None)?).status()?;
        if !st.success() {
            bail!("db {key}: `{cmd}` failed ({st})");
        }
        if matches!(action, DbAction::Drop) {
            self.release(reg)?;
        }
        Ok(())
    }

    /// Replaces this process with `[test.<name>]`'s `run`, `args` appended.
    fn test(&self, name: &str, args: &[String]) -> Result<()> {
        let Some(t) = self.cfg.test.get(name) else {
            let known: Vec<&str> = self.cfg.test.keys().map(String::as_str).collect();
            bail!("no [test.{name}] in {CONFIG}; it has: {}", known.join(", "));
        };
        let run = render(&t.run, &self.vars)?;
        let err = sh(
            &format!("{run} \"$@\""),
            &self.dir(t.dir.as_ref())?,
            &self.env_with(Some(&t.env))?,
        )
        .arg("sh")
        .args(args)
        .exec();
        bail!("run sh: {err}")
    }

    fn print_urls(&self) {
        println!("slot {} ({})", self.slot, self.id);
        for name in self.cfg.services.keys() {
            if let Some(u) = self.url(name) {
                println!("{name:<12} {u}");
            }
        }
    }
}

/// This user's slot registry (`<data dir>/dev`) and the shared claims directory.
fn dirs() -> Result<(PathBuf, PathBuf)> {
    let dir = client::Paths::from_env(client::DEFAULT_INSTANCE)?
        .dir
        .join("dev");
    fs::create_dir_all(&dir)?;
    let shared = std::env::var_os("AGENT_FLEET_DEV_SHARED").map_or(SHARED.into(), PathBuf::from);
    Ok((dir, shared))
}

/// Worktree path → slot, for every linked worktree that holds one.
pub fn slots() -> Result<BTreeMap<String, u32>> {
    read_json(&dirs()?.0.join("slots.json"))
}

/// The keys `root` may be registered under: as given, and with its symlinks resolved (what
/// `git rev-parse --show-toplevel` gives `dev up`).
fn keys(root: &Path) -> Vec<String> {
    let mut out = vec![root.display().to_string()];
    if let Ok(r) = fs::canonicalize(root) {
        out.push(r.display().to_string());
    }
    out
}

/// The services of the state dir `state` still running as started.
fn running_in(state: &Path, procs: &Procs) -> Vec<String> {
    let rec: Recorded = read_json(&state.join("state.json")).unwrap_or_default();
    rec.running
        .iter()
        .filter(|(_, r)| ours(r, procs))
        .map(|(n, _)| n.clone())
        .collect()
}

/// `dev ls`'s line for the worktree at `root` (`main`: the main checkout, slot 0): its slot,
/// ports and running services. `None` when it has no slot.
pub fn line(root: &Path, main: bool, procs: &Procs) -> Option<String> {
    line_in(&dirs().ok()?.0, root, main, procs)
}

fn line_in(dir: &Path, root: &Path, main: bool, procs: &Procs) -> Option<String> {
    let config = fs::read_to_string(root.join(CONFIG)).ok();
    let (key, slot) = if main {
        (root.display().to_string(), config.as_ref().map(|_| 0)?)
    } else {
        let slots: BTreeMap<String, u32> = read_json(&dir.join("slots.json")).ok()?;
        keys(root)
            .into_iter()
            .find_map(|k| slots.get(&k).map(|s| (k, *s)))?
    };
    let id = id_of(Path::new(&key));
    let state = dir.join(format!("{slot}-{id}"));
    let mut out = format!("slot {slot} ({id})");
    let cfg = config.as_deref().map(parse);
    match cfg.map(|c| c.and_then(|c| Vars::new(&c, slot, &id, root, &state))) {
        Some(Ok(v)) if !v.ports.is_empty() => {
            let ports: Vec<String> = v.ports.iter().map(|(n, p)| format!("{n} {p}")).collect();
            out += &format!(", ports {}", ports.join(", "));
        }
        Some(Err(e)) => out += &format!(", {CONFIG}: {e:#}"),
        _ => {}
    }
    let running = running_in(&state, procs);
    out += &match running.is_empty() {
        true => ", nothing running".to_string(),
        false => format!(", running {}", running.join(", ")),
    };
    Some(out)
}

/// `dev down --release` for the worktree at `root`, from anywhere: stops its services, drops its
/// state and logs, frees its slot. `None` when it holds no slot.
pub fn release(root: &Path) -> Result<Option<String>> {
    let (dir, shared) = dirs()?;
    release_in(
        &Registry {
            dir: &dir,
            shared: &shared,
        },
        root,
    )
}

fn release_in(reg: &Registry, root: &Path) -> Result<Option<String>> {
    let slots: BTreeMap<String, u32> = read_json(&reg.dir.join("slots.json"))?;
    let Some((key, slot)) = keys(root)
        .into_iter()
        .find_map(|k| slots.get(&k).map(|s| (k, *s)))
    else {
        return Ok(None);
    };
    let state = reg.dir.join(format!("{slot}-{}", id_of(Path::new(&key))));
    let mut stopped = Vec::new();
    if state.is_dir() {
        let _l = lock(&state.join("lock"))?;
        let rec: Recorded = read_json(&state.join("state.json"))?;
        let procs = Procs::read();
        for (name, r) in &rec.running {
            if ours(r, &procs) {
                stop(r.pid);
                stopped.push(name.clone());
            }
        }
    }
    match fs::remove_dir_all(&state) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("remove {}", state.display()));
        }
        _ => {}
    }
    reg.release(&key)?;
    let stopped = match stopped.is_empty() {
        true => String::new(),
        false => format!(", stopped {}", stopped.join(", ")),
    };
    Ok(Some(format!("released slot {slot}{stopped}")))
}

/// `dev prune`: frees the slot, state and logs of each registered worktree that is gone from
/// disk and runs no service any more; one line per slot it looked at.
fn prune(reg: &Registry) -> Result<Vec<String>> {
    let slots: BTreeMap<String, u32> = read_json(&reg.dir.join("slots.json"))?;
    let procs = Procs::read();
    let mut out = Vec::new();
    for (root, slot) in slots {
        if Path::new(&root).exists() {
            continue;
        }
        let state = reg.dir.join(format!("{slot}-{}", id_of(Path::new(&root))));
        let running = running_in(&state, &procs);
        if !running.is_empty() {
            out.push(format!(
                "kept slot {slot} of {root} (deleted): {} still running; agent-fleet dev down \
                 cannot reach it, stop them by hand",
                running.join(", ")
            ));
            continue;
        }
        release_in(reg, Path::new(&root))?;
        out.push(format!("dropped slot {slot} of {root} (deleted)"));
    }
    Ok(out)
}

pub async fn run(cmd: &Cmd) -> Result<()> {
    let (dir, shared) = dirs()?;
    let reg = Registry {
        dir: &dir,
        shared: &shared,
    };
    if let Cmd::Prune = cmd {
        let lines = prune(&reg)?;
        if lines.is_empty() {
            println!("nothing to prune");
        }
        for l in lines {
            println!("{l}");
        }
        return Ok(());
    }
    let w = Worktree::open(&reg)?;
    match cmd {
        Cmd::Ls { .. } | Cmd::Clean { .. } | Cmd::Agents | Cmd::Prs => {
            unreachable!("main runs ls, clean, agents and prs")
        }
        Cmd::Prune => unreachable!("handled above"),
        Cmd::Db { action } => w.db(&reg, *action)?,
        Cmd::Test { name, args } => w.test(name, args)?,
        Cmd::Up { services } => {
            let names = pick(services, &w.cfg)?;
            w.infra().await?;
            let _l = lock(&w.state.join("lock"))?;
            let mut rec = w.recorded()?;
            w.start_each(&names, &mut rec).await?;
            w.print_urls();
        }
        Cmd::Down { release: true } => w.release(&reg)?,
        Cmd::Down { release: false } => {
            let _l = lock(&w.state.join("lock"))?;
            let mut rec = w.recorded()?;
            w.stop_all(&mut rec, None);
            w.save(&rec)?;
        }
        Cmd::Restart { services } => {
            let asked = pick(services, &w.cfg)?;
            w.infra().await?;
            let _l = lock(&w.state.join("lock"))?;
            let mut rec = w.recorded()?;
            let names = restart_set(asked, &rec.set);
            w.stop_all(&mut rec, Some(&names));
            w.save(&rec)?;
            w.start_each(&names, &mut rec).await?;
            w.print_urls();
        }
        Cmd::Status => {
            let rec = w.recorded()?;
            println!("slot {} ({}) {}", w.slot, w.id, w.root.display());
            for (name, s) in &w.cfg.services {
                let run = rec.running.get(name);
                let mut line = match run {
                    Some(r) => format!("{name:<12} running (pid {})", r.pid),
                    None => format!("{name:<12} stopped"),
                };
                if let (Some(r), Some(check)) = (run, &s.health) {
                    let ok = healthy(&render(check, &w.vars)?, &w.dir_of(name)?, &r.env).await;
                    line += if ok { ", healthy" } else { ", unhealthy" };
                }
                if run.is_some_and(|r| r.env != w.env(Some(name)).unwrap_or_default()) {
                    line += ", env changed since start";
                }
                println!("{line}");
            }
        }
        Cmd::Logs { service } => {
            let names: Vec<String> = match service {
                Some(s) => pick(std::slice::from_ref(s), &w.cfg)?,
                None => w.cfg.services.keys().cloned().collect(),
            };
            let files: Vec<PathBuf> = names
                .iter()
                .map(|n| w.log(n))
                .filter(|p| p.exists())
                .collect();
            if files.is_empty() {
                bail!("no logs yet; `dev up` writes them");
            }
            let err = Command::new("tail")
                .args(["-n", "50", "-F"])
                .args(&files)
                .exec();
            bail!("run tail: {err}");
        }
        Cmd::Env => {
            for (k, v) in w.env(None)? {
                println!("{k}={v}");
            }
        }
        Cmd::Url => {
            for name in w.cfg.services.keys() {
                if let Some(u) = w.url(name) {
                    println!("{name:<12} {u}");
                }
            }
        }
        Cmd::Info => {
            let rec = w.recorded()?;
            let services: BTreeMap<&String, serde_json::Value> = w
                .cfg
                .services
                .keys()
                .map(|n| {
                    let pid = rec.running.get(n).map(|r| r.pid);
                    (
                        n,
                        json!({ "running": pid.is_some(), "pid": pid, "url": w.url(n),
                                "log": w.log(n) }),
                    )
                })
                .collect();
            let info = json!({
                "slot": w.slot, "id": w.id, "root": w.root, "main": w.main,
                "ports": w.vars.ports, "env": w.env(None)?, "services": services,
                "recorded": rec.set,
            });
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Proc;

    const SAMPLE: &str = r#"
[slots]
max = 31
step = 10
ports = { api = 8990, web = 8991 }

[env]
DB_NAME = "app_{id}"
CACHE_DB = "{slot}"

[infra]
up = "docker compose up -d postgres redis"
health = ["pg_isready -h 127.0.0.1 -p 5435"]

[services.api]
dir = "server"
before = "make deps && make migrate"
run = "make serve PORT={port.api}"
health = "http://127.0.0.1:{port.api}/health"
timeout = 120
url = "http://localhost:{port.api}"

[services.web]
dir = "web"
run = "npm start"
env = { PORT = "{port.web}", API = "{port.api}" }
"#;

    #[test]
    fn the_issue_example_parses() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.slots.ports["web"], 8991);
        assert_eq!(c.services["api"].timeout, 120);
        assert_eq!(c.services["web"].timeout, 60);
        assert_eq!(c.infra.unwrap().health.len(), 1);
        // docs/DEV.md's minimal file and its `${VAR}` advice.
        let m = parse(
            "[slots]\nports = { api = 8990 }\n[services.api]\nrun = \"serve --port ${PORT}\"\n\
             env = { PORT = \"{port.api}\" }\n",
        )
        .unwrap();
        let v = Vars::new(&m, 2, "x", Path::new("/r"), Path::new("/s")).unwrap();
        assert_eq!(
            render(&m.services["api"].run, &v).unwrap(),
            "serve --port ${PORT}"
        );
        assert_eq!(render(&m.services["api"].env["PORT"], &v).unwrap(), "9010");
    }

    #[test]
    fn errors_name_the_line_or_the_key() {
        let e = parse("[slots]\nmax = 3\nstpe = 1\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 3"), "{e}");
        let e = parse("[services.a]\nrun = 1\n").unwrap_err().to_string();
        assert!(e.contains("line 2"), "{e}");
        let e = parse("[services.a]\nrun = \"x {port.nope}\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("services.a.run") && e.contains("nope"), "{e}");
        let e = parse("[slots]\nmax = 10\nstep = 10\nports = { a = 65500 }\n").unwrap_err();
        assert!(e.to_string().contains("65535"), "{e}");
        let e = parse("[services.all]\nrun = \"x\"\n").unwrap_err();
        assert!(e.to_string().contains("all"), "{e}");
    }

    #[test]
    fn templates_fill_known_names_and_leave_shell_braces() {
        let c = parse(SAMPLE).unwrap();
        let v = Vars::new(&c, 3, "c_99", Path::new("/w/c-99"), Path::new("/d/3-c_99")).unwrap();
        assert_eq!(
            render(
                "{root} {id} {slot} {state} {port.api} {port.web} ${HOME} {x",
                &v
            )
            .unwrap(),
            "/w/c-99 c_99 3 /d/3-c_99 9020 9021 ${HOME} {x"
        );
        assert!(render("{port.apx}", &v).is_err());
        assert_eq!(
            port_names("a {port.api} b {port.web}").collect::<Vec<_>>(),
            ["api", "web"]
        );
    }

    #[test]
    fn picks_services_by_flag_or_name() {
        let c = parse(SAMPLE).unwrap();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(pick(&s(&["--web"]), &c).unwrap(), ["web"]);
        assert_eq!(pick(&s(&["api", "--web"]), &c).unwrap(), ["api", "web"]);
        assert_eq!(pick(&s(&["--all"]), &c).unwrap(), ["api", "web"]);
        assert!(pick(&s(&["--db"]), &c).is_err());
    }

    #[test]
    fn restart_takes_the_named_services_or_the_recorded_set() {
        let set: BTreeSet<String> = ["api".into(), "web".into()].into();
        assert_eq!(restart_set(vec![], &set), ["api", "web"]);
        assert_eq!(restart_set(vec!["web".into()], &set), ["web"]);
    }

    #[test]
    fn the_lowest_free_slot_wins() {
        assert_eq!(choose(&[].into(), 3), Some(1));
        assert_eq!(choose(&[1, 3].into(), 3), Some(2));
        assert_eq!(choose(&[1, 2, 3].into(), 3), None);
    }

    #[test]
    fn claims_are_stable_and_skip_other_users() {
        let data = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        fs::set_permissions(shared.path(), fs::Permissions::from_mode(0o700)).unwrap();
        // Another user holds slot 1.
        fs::write(shared.path().join("4000000000.json"), "[1]").unwrap();
        let reg = Registry {
            dir: data.path(),
            shared: shared.path(),
        };
        assert_eq!(reg.claim("/w/a", 5).unwrap(), 2);
        assert_eq!(reg.claim("/w/b", 5).unwrap(), 3);
        assert_eq!(reg.claim("/w/a", 5).unwrap(), 2);
        let mine = shared.path().join(format!("{}.json", uid()));
        assert_eq!(
            fs::read_to_string(&mine)
                .unwrap()
                .split_whitespace()
                .collect::<String>(),
            "[2,3]"
        );
        reg.release("/w/a").unwrap();
        assert_eq!(
            fs::read_to_string(&mine)
                .unwrap()
                .split_whitespace()
                .collect::<String>(),
            "[3]"
        );
        assert_eq!(reg.claim("/w/c", 5).unwrap(), 2);
        assert!(reg.claim("/w/d", 3).is_err());
    }

    #[test]
    fn a_shared_dir_anyone_may_write_must_be_sticky() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("shared");
        shared_dir(&p).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, 0o1777);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(shared_dir(&p).is_err());
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        shared_dir(&p).unwrap();
    }

    #[test]
    fn only_the_group_the_service_started_is_ours() {
        let t = |s: i64| DateTime::from_timestamp(1_000_000 + s, 0);
        let p = |pgid: u32, started: i64| Proc {
            ppid: 1,
            cpu_ms: 0,
            rss_kb: 0,
            name: "sh".into(),
            cwd: None,
            deleted: false,
            args: None,
            started: t(started),
            pgid: Some(pgid),
            zombie: false,
        };
        let run = Run {
            pid: 500,
            started: t(10),
            env: BTreeMap::new(),
        };
        let procs = |v: Vec<(u32, Proc)>| Procs(v.into_iter().collect());
        // The leader runs with the recorded start time.
        assert!(ours(&run, &procs(vec![(500, p(500, 10))])));
        // A new leader reuses the id.
        assert!(!ours(&run, &procs(vec![(500, p(500, 99))])));
        // The leader is gone; its children still run.
        assert!(ours(
            &run,
            &procs(vec![(501, p(500, 11)), (502, p(500, 10))])
        ));
        // The leader is gone, and a member of the group is older than it: another group.
        assert!(!ours(
            &run,
            &procs(vec![(501, p(500, 11)), (400, p(500, 5))])
        ));
        // Nobody is in the group.
        assert!(!ours(&run, &procs(vec![(500, p(7, 10))])));
    }

    #[test]
    fn this_repositorys_own_file_parses() {
        let c = parse(include_str!("../.agent-fleet/dev.toml")).unwrap();
        assert_eq!(c.services.keys().collect::<Vec<_>>(), ["host", "hub"]);
    }

    #[test]
    fn ids_are_short_and_safe() {
        assert_eq!(id_of(Path::new("/w/C-99.x")), "c_99_x");
    }
    /// A registry in temp dirs, and a service group recorded in `slot`'s state for `root`.
    fn served(reg: &Registry, root: &str, slot: u32, service: Option<u32>) {
        let state = reg.dir.join(format!("{slot}-{}", id_of(Path::new(root))));
        fs::create_dir_all(&state).unwrap();
        let mut rec = Recorded::default();
        if let Some(pid) = service {
            let started = Procs::read().0.get(&pid).and_then(|p| p.started);
            rec.running.insert(
                "app".into(),
                Run {
                    pid: pid as i32,
                    started,
                    env: BTreeMap::new(),
                },
            );
        }
        write_json(&state.join("state.json"), &rec).unwrap();
    }

    /// A service's process group, reaped as it dies (as `dev up` does), so `stop` sees it go.
    fn group() -> u32 {
        let mut child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        std::thread::spawn(move || child.wait());
        pid
    }

    #[test]
    fn prune_drops_deleted_worktrees_whose_services_stopped_and_keeps_the_rest() {
        let (data, shared, live) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        fs::set_permissions(shared.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let reg = Registry {
            dir: data.path(),
            shared: shared.path(),
        };
        let live = live.path().display().to_string();
        let gone = data.path().join("gone").display().to_string();
        let busy = data.path().join("busy").display().to_string();
        let pid = group();
        for (root, service) in [(&live, None), (&gone, None), (&busy, Some(pid))] {
            let slot = reg.claim(root, 5).unwrap();
            served(&reg, root, slot, service);
        }
        let lines = prune(&reg).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("dropped slot 2 of ") && l.contains("gone"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("kept slot 3") && l.contains("app still"))
        );
        let slots: BTreeMap<String, u32> = read_json(&data.path().join("slots.json")).unwrap();
        let mut kept = vec![&busy, &live];
        kept.sort();
        assert_eq!(slots.keys().collect::<Vec<_>>(), kept);
        assert!(
            !data.path().join("2-gone").exists(),
            "its state and logs go too"
        );
        // Once its service is down, the deleted worktree's slot goes as well.
        assert_eq!(
            release_in(&reg, Path::new(&busy)).unwrap().unwrap(),
            "released slot 3, stopped app"
        );
        assert!(prune(&reg).unwrap().is_empty());
    }

    #[test]
    fn release_stops_a_worktrees_services_and_frees_its_slot_from_anywhere() {
        let (data, shared, root) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        fs::set_permissions(shared.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let reg = Registry {
            dir: data.path(),
            shared: shared.path(),
        };
        let r = root.path().display().to_string();
        let pid = group();
        let slot = reg.claim(&r, 5).unwrap();
        served(&reg, &r, slot, Some(pid));
        fs::create_dir_all(root.path().join(".agent-fleet")).unwrap();
        fs::write(
            root.path().join(CONFIG),
            "[slots]\nstep = 10\nports = { api = 21000 }\n[services.app]\nrun = \"x\"\n",
        )
        .unwrap();
        let line = line_in(data.path(), root.path(), false, &Procs::read()).unwrap();
        assert!(line.ends_with(", ports api 21010, running app"), "{line}");
        assert_eq!(
            release_in(&reg, root.path()).unwrap().unwrap(),
            "released slot 1, stopped app"
        );
        assert!(!alive(pid as i32));
        assert_eq!(
            line_in(data.path(), root.path(), false, &Procs::read()),
            None
        );
        assert_eq!(
            release_in(&reg, root.path()).unwrap(),
            None,
            "no slot: nothing to do"
        );
        let main = line_in(data.path(), root.path(), true, &Procs::read()).unwrap();
        assert!(main.starts_with("slot 0 (") && main.contains("api 21000, nothing running"));
    }
}
