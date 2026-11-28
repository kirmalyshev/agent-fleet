//! `agent-fleet host`: one per machine and herdr session. Presence from herdr, disk health, a local replica of the log,
//! and doorbell delivery into herdr agents.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Notify;

use crate::client::{Client, Refused, Sent};
use crate::event::*;
use crate::scan;
use crate::state::{Msg, State};
use crate::store::{Store, UnknownCursor};

/// herdr socket protocol this build understands.
pub const HERDR_PROTOCOL: u64 = 22;

// ---------- herdr socket API: newline-delimited JSON, one request per connection ----------

/// An open herdr subscription. herdr ends it when the client hangs up, so the write half lives here too.
struct Sub {
    lines: Lines<BufReader<OwnedReadHalf>>,
    _wr: OwnedWriteHalf,
}

async fn send(sock: &Path, method: &str, params: Value) -> Result<(Value, Sub)> {
    let stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connect herdr at {}", sock.display()))?;
    let (rd, mut wr) = stream.into_split();
    let req = json!({ "id": "fleet", "method": method, "params": params });
    wr.write_all(format!("{req}\n").as_bytes()).await?;
    let mut lines = BufReader::new(rd).lines();
    let line = lines
        .next_line()
        .await?
        .with_context(|| format!("herdr closed the connection on {method}"))?;
    let mut res: Value = serde_json::from_str(&line)?;
    if let Some(err) = res.get("error") {
        bail!("herdr {method}: {err}");
    }
    Ok((res["result"].take(), Sub { lines, _wr: wr }))
}

pub async fn herdr_call(sock: &Path, method: &str, params: Value) -> Result<Value> {
    Ok(send(sock, method, params).await?.0)
}

async fn herdr_subscribe(sock: &Path, subscriptions: Value) -> Result<Sub> {
    Ok(send(
        sock,
        "events.subscribe",
        json!({ "subscriptions": subscriptions }),
    )
    .await?
    .1)
}

fn agent_state(v: &Value) -> AgentState {
    serde_json::from_value(v.clone()).unwrap_or(AgentState::Unknown)
}

// ---------- the host agent ----------

pub struct Config {
    pub host: String,
    pub session: String,
    pub herdr_socket: PathBuf,
    pub replica: PathBuf,
    pub data_dir: PathBuf,
    /// Repositories whose worktrees to watch besides the ones agents work in.
    pub repos: Vec<String>,
    /// `owner/repo` → the local checkout a `spawn.requested` is executed against, from the
    /// `--repo` flags; `checkouts_now` adds the repos connected with `agent-fleet repo add`.
    pub checkouts: BTreeMap<String, PathBuf>,
    /// The machine's config dir (`spawn::machine_dir`): its fleet.toml, brief and `[repos]`.
    pub config_dir: Option<PathBuf>,
    /// Lock directories to report as resources: (name, directory).
    pub locks: Vec<(String, PathBuf)>,
    /// Minutes an agent may idle/block after working before its lead is told it's stalled.
    pub stall_min: u64,
    /// Minutes an agent's BLOCKED/QUESTION may go unanswered while it idles before the lead is re-rung.
    pub waiting_min: u64,
    /// `host --stuck-act`: let the host act on a stuck cause itself — keystrokes and
    /// handoffs. Off (the default) it only classifies, records `agent.stuck`, and tells.
    pub stuck_act: bool,
}

impl Config {
    /// The checkouts this host serves now: the machine's `[repos]`, re-read at each call so a
    /// repo connected after the host started is served without a restart, under the `--repo`
    /// flags, which win on a conflict.
    pub fn checkouts_now(&self) -> Result<BTreeMap<String, PathBuf>> {
        let mut all = match &self.config_dir {
            Some(d) => crate::setup::registered(d)?,
            None => BTreeMap::new(),
        };
        all.extend(self.checkouts.clone());
        Ok(all)
    }
}

#[derive(Clone)]
struct Local {
    agent: String,
    state: AgentState,
    /// How many times this host has logged the agent turning `working`.
    working: u64,
    /// Bumped each time an agent is (re)detected in the pane. A watcher serves one generation.
    epoch: u64,
}

/// One consistent read of the replica: `state` is the fold of `events`, both frozen at the
/// same instant. The per-tick loops share snapshots through `Host::view` instead of each
/// re-reading and re-folding the whole log.
#[derive(Clone, Default)]
pub(crate) struct View {
    /// The log through `state.last`, in arrival order.
    pub(crate) events: Vec<Event>,
    pub(crate) state: State,
}

pub(crate) struct Host {
    pub(crate) cfg: Config,
    pub(crate) client: Client,
    pub(crate) replica: Mutex<Store>,
    /// `view()`'s latest snapshot; swapped only when the log grew.
    view: Mutex<Arc<View>>,
    /// herdr pane id → the agent in it.
    panes: Mutex<HashMap<String, Local>>,
    /// herdr pane id → the name herdr last gave its agent. herdr forgets it when the agent
    /// restarts in the pane (#201); the next agent there gets it back. Dropped when the pane
    /// closes. ponytail: lost when the host restarts while an agent is between runs; persist under the data dir if that bites.
    names: Mutex<HashMap<String, String>>,
    next_epoch: AtomicU64,
    /// Wakes the delivery loop.
    pub(crate) wake: Notify,
    /// Fires after an `agent.state` is recorded.
    seen: Notify,
    started: Instant,
    n: Counters,
    /// Spawn executions and handoffs in flight, by id and what they are; a self-update restart
    /// waits for none.
    busy: Mutex<Vec<(u64, String)>>,
    busy_ids: AtomicU64,
    /// A self-update restart is waiting: no new spawn or handoff gets a guard.
    draining: AtomicBool,
    /// Since when the self-update timer has seen `origin/main` ahead of this binary.
    pub(crate) behind_since: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
}

/// Held while the host does work a restart would cut short.
pub(crate) struct Busy<'a> {
    work: &'a Mutex<Vec<(u64, String)>>,
    id: u64,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.work.lock().unwrap().retain(|(i, _)| *i != self.id);
    }
}

/// Since start; reported in `host.health`.
#[derive(Default)]
struct Counters {
    rings: AtomicU64,
    ring_failures: AtomicU64,
    herdr_errors: AtomicU64,
    write_failures: AtomicU64,
    spooled: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// How often the host reads the locks it watches.
const LOCKS_EVERY: Duration = Duration::from_secs(5);

/// How often the host samples agents' processes and the worktrees.
const SCAN_EVERY: Duration = Duration::from_secs(300);

/// A failed ring is retried at most this often.
const RETRY_AFTER: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 60 });
/// How long after a queued ring the host waits before it looks for the text on the screen.
const QUEUE_CHECK: Duration = Duration::from_secs(2);
/// How often the host reads each agent program's usage quota.
const QUOTA_EVERY: Duration = Duration::from_secs(60);

/// The expensive disk-use breakdown (`dust`, never du) is refreshed at most this often.
const DUST_EVERY: Duration = Duration::from_secs(30 * 60);

/// One `dust` scan may run this long before it is abandoned and the old breakdown kept.
const DUST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `devin auth status` refreshes devin's cached quota; run at most this often.
const DEVIN_STATUS_EVERY: Duration = Duration::from_secs(15 * 60);

/// agy's `/usage` probe starts a CLI process; five minutes is fresh enough for the ten-minute
/// spawn gate without multiplying its logs or process churn. The probe's own log goes to the
/// null device, so it does not write one `cli-*.log` per read.
const AGY_PROBE_EVERY: Duration = Duration::from_secs(5 * 60);
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";

fn backoff(d: Duration) -> Duration {
    (d * 2).min(Duration::from_secs(60))
}

/// What the quota sampler remembers between reads.
#[derive(Default)]
struct Quotas {
    /// The last published read per program, for change detection (`read_at` aside).
    published: HashMap<String, AgentQuota>,
    /// Sources already reported missing this run.
    missing: HashSet<String>,
    /// When `devin auth status` last ran.
    devin_status: Option<Instant>,
    /// The asynchronous `agy /usage` probe; joining it never blocks the scan loop.
    agy_probe: Option<tokio::task::JoinHandle<Option<AgentQuota>>>,
    /// When the last agy probe started.
    agy_probe_started: Option<Instant>,
}

/// `name` is a runnable file on PATH.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// The UDIDs still usable by a device/test tool: anything `devicectl` sees connected plus
/// every booted simulator. `None` when the toolchain cannot answer — a list that may be
/// incomplete proves no device gone, so the whole stray pass skips rather than guesses.
async fn live_devices() -> Option<HashSet<String>> {
    if !cfg!(target_os = "macos") {
        return Some(HashSet::new());
    }
    async fn out_of(args: &[&str]) -> Option<String> {
        let out = tokio::process::Command::new("xcrun")
            .args(args)
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
    let connected = out_of(&["devicectl", "list", "devices"]).await?;
    let booted = out_of(&["simctl", "list", "devices", "booted"]).await?;
    Some(
        scan::udids(&connected)
            .union(&scan::udids(&booted))
            .cloned()
            .collect(),
    )
}

async fn dust_scan(root: &Path, label: &str, lines: usize) -> Vec<DiskEntry> {
    if !root.is_dir() {
        return Vec::new();
    }
    let run = tokio::process::Command::new("dust")
        .arg("-j")
        .arg("-o")
        .arg("b")
        .arg("-P")
        .arg("-d")
        .arg("1")
        .arg("-n")
        .arg(lines.to_string())
        .arg(root)
        .kill_on_drop(true)
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(DUST_TIMEOUT, run).await else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    crate::stats::parse_dust_json(label, &String::from_utf8_lossy(&out.stdout))
}

/// The cached answer to "what uses the disk": top directories under home, herdr's worktree
/// directory and the platform cache directory. Empty when `dust` is absent or cannot scan one;
/// free space itself is statvfs and does not depend on this.
async fn dust_breakdown() -> Vec<DiskEntry> {
    if !on_path("dust") {
        return Vec::new();
    }
    let Some(base) = directories::BaseDirs::new() else {
        return Vec::new();
    };
    let home = base.home_dir().to_path_buf();
    let worktrees =
        crate::scan::herdr_worktrees().unwrap_or_else(|| home.join(".herdr").join("worktrees"));
    let cache = base.cache_dir().to_path_buf();
    let mut out = Vec::new();
    for (root, label, lines) in [
        (home, "home", 12),
        (worktrees, "worktrees", 8),
        (cache, "cache", 8),
    ] {
        out.extend(dust_scan(&root, label, lines).await);
    }
    crate::stats::fit_disk_entries(out, 6 * 1024)
}

/// One `agy -p "/usage"` probe. stdin is /dev/null because a logged-out CLI otherwise waits for
/// an authentication code; the timeout kills the process rather than letting it hold the host's
/// quota loop.
async fn agy_probe(host: String) -> Option<AgentQuota> {
    let run = tokio::process::Command::new("agy")
        .args([
            "-p",
            "/usage",
            "--output-format",
            "json",
            "--log-file",
            NULL_DEVICE,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    crate::quota::agy_output(&host, &text, Utc::now())
}

impl Host {
    pub(crate) fn new(cfg: Config, client: Client, replica: Store) -> Self {
        Self {
            cfg,
            client,
            replica: Mutex::new(replica),
            view: Mutex::default(),
            panes: Mutex::default(),
            names: Mutex::default(),
            next_epoch: AtomicU64::new(1),
            wake: Notify::new(),
            seen: Notify::new(),
            started: Instant::now(),
            n: Counters::default(),
            busy: Mutex::default(),
            busy_ids: AtomicU64::new(0),
            draining: AtomicBool::new(false),
            behind_since: Mutex::default(),
        }
    }

    /// A guard for work a restart would cut short (`what` names it), or `None` once a restart is
    /// waiting: the work is registered before the flag is read, so it either holds the guard the
    /// restart waits for or never starts.
    pub(crate) fn busy_guard(&self, what: impl Into<String>) -> Option<Busy<'_>> {
        let id = self.busy_ids.fetch_add(1, Ordering::SeqCst);
        self.busy.lock().unwrap().push((id, what.into()));
        let guard = Busy {
            work: &self.busy,
            id,
        };
        (!self.is_draining()).then_some(guard)
    }

    pub(crate) fn is_busy(&self) -> bool {
        !self.busy.lock().unwrap().is_empty()
    }

    /// What is in flight.
    pub(crate) fn busy_work(&self) -> Vec<String> {
        self.busy
            .lock()
            .unwrap()
            .iter()
            .map(|(_, w)| w.clone())
            .collect()
    }

    pub(crate) fn drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// The replica as one (events, state) snapshot. The first call folds the whole log; each
    /// later call reads only the rows that arrived since — the loops never scan the table again.
    pub(crate) fn view(&self) -> Result<Arc<View>> {
        let mut cur = self.view.lock().unwrap();
        let last = cur.events.last().map(|e| e.id.clone());
        let new = {
            let replica = self.replica.lock().unwrap();
            match replica.after(last.as_deref(), usize::MAX >> 1) {
                Ok(new) => new,
                // The replica was replaced under this connection: fold its new log from scratch.
                Err(err) if err.downcast_ref::<UnknownCursor>().is_some() => {
                    *cur = Arc::new(View::default());
                    replica.all()?
                }
                Err(err) => return Err(err),
            }
        };
        if new.is_empty() {
            return Ok(cur.clone());
        }
        let mut next = (**cur).clone();
        for e in new {
            next.state.apply(&e);
            next.events.push(e);
        }
        *cur = Arc::new(next);
        Ok(cur.clone())
    }

    fn stats(&self) -> HostStats {
        let (max_rss_kb, cpu_ms) = crate::stats::self_usage();
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        HostStats {
            uptime_s: self.started.elapsed().as_secs(),
            max_rss_kb,
            cpu_ms,
            rings: get(&self.n.rings),
            ring_failures: get(&self.n.ring_failures),
            herdr_errors: get(&self.n.herdr_errors),
            write_failures: get(&self.n.write_failures),
            spooled: get(&self.n.spooled),
        }
    }

    pub(crate) fn actor(&self) -> Actor {
        Actor {
            kind: ActorKind::Host,
            id: format!("host:{}", self.cfg.host),
        }
    }

    fn agent_id(&self, name: &str) -> String {
        format!("agent:{}/{}/{}", self.cfg.host, self.cfg.session, name)
    }

    /// Writes to the hub (or the spool) and to the local replica, so this host sees its own
    /// writes even while the hub is down. Copies merge by id, so the later echo is a no-op.
    /// Returns whether the event was written. One that was not stays out of the replica.
    pub(crate) async fn publish(&self, e: Envelope<Known>) -> bool {
        let ty = e.body.type_name();
        match self.client.write(&e).await {
            Ok(Sent::Spooled(why)) => {
                bump(&self.n.spooled);
                eprintln!("agent-fleet host: {ty} spooled ({why})");
            }
            Ok(Sent::Accepted { .. }) => {}
            Err(err) => {
                bump(&self.n.write_failures);
                eprintln!("agent-fleet host: {ty} NOT written: {err:#}");
                return false;
            }
        }
        if let Err(err) = self.replica.lock().unwrap().append(&e.into_event()) {
            eprintln!("agent-fleet host: replica append: {err}");
        }
        true
    }

    /// Each herdr pane the fleet knows: its agent and the pane's shell pid. A pane herdr cannot
    /// describe is skipped.
    pub(crate) async fn pane_shells(&self) -> Result<Vec<(String, u32)>> {
        let list = herdr_call(&self.cfg.herdr_socket, "agent.list", json!({})).await?;
        let Some(agents) = list["agents"].as_array() else {
            bail!("agent.list answered without agents");
        };
        let mut out = Vec::new();
        for a in agents {
            let pane = a["pane_id"].as_str().unwrap_or_default();
            let Some(agent) = self
                .panes
                .lock()
                .unwrap()
                .get(pane)
                .map(|l| l.agent.clone())
            else {
                continue;
            };
            let target = json!({ "pane_id": pane });
            let info = match herdr_call(&self.cfg.herdr_socket, "pane.process_info", target).await {
                Ok(info) => info,
                Err(err) => {
                    bump(&self.n.herdr_errors);
                    eprintln!("agent-fleet host: watch: processes of {pane}: {err:#}");
                    continue;
                }
            };
            if let Some(shell) = info["process_info"]["shell_pid"].as_u64() {
                out.push((agent, shell as u32));
            }
        }
        Ok(out)
    }

    pub(crate) fn presence(&self, body: Known) -> Envelope<Known> {
        Envelope::new(self.actor(), self.cfg.session.clone(), body)
    }

    fn current(&self, pane: &str, epoch: u64) -> bool {
        self.panes
            .lock()
            .unwrap()
            .get(pane)
            .is_some_and(|l| l.epoch == epoch)
    }

    /// Records the agent in `pane` and publishes `agent.up`. Returns the generation to watch.
    pub(crate) async fn agent_up(&self, pane: &str, info: &Value) -> u64 {
        let named = info["name"].as_str().filter(|n| !n.is_empty());
        let kept = match named {
            Some(_) => None,
            None => self.names.lock().unwrap().get(pane).cloned(),
        };
        let agent = self.agent_id(named.or(kept.as_deref()).unwrap_or(pane));
        if let Some(n) = named {
            self.names.lock().unwrap().insert(pane.into(), n.into());
        }
        if let Some(name) = &kept {
            // Say who renamed and why before the `agent.up` that carries the name.
            self.publish(self.presence(Known::AgentRenamed(AgentRenamed {
                agent: agent.clone(),
                pane: pane.into(),
                from: Some(self.agent_id(pane)),
                rule: "pane-name-memory".into(),
            })))
            .await;
            let params = json!({ "target": pane, "name": name });
            if let Err(e) = herdr_call(&self.cfg.herdr_socket, "agent.rename", params).await {
                bump(&self.n.herdr_errors);
                eprintln!("agent-fleet host: rename {pane} to {name}: {e:#}");
            }
        }
        let state = agent_state(&info["agent_status"]);
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        {
            let mut panes = self.panes.lock().unwrap();
            // The same agent re-detected keeps its counter, so a delivery in flight still sees it.
            let working = panes
                .get(pane)
                .filter(|l| l.agent == agent)
                .map_or(0, |l| l.working);
            let local = Local {
                agent: agent.clone(),
                state,
                working,
                epoch,
            };
            panes.insert(pane.to_string(), local);
        }
        self.publish(self.presence(Known::AgentUp(AgentUp {
            agent,
            host: self.cfg.host.clone(),
            kind: info["agent"].as_str().map(str::to_string),
            state,
            pane: Some(pane.to_string()),
        })))
        .await;
        self.wake.notify_one();
        epoch
    }

    /// `agent-fleet adopt` renamed `pane` in herdr: register its agent under the new name.
    pub(crate) async fn adopt(self: &Arc<Self>, pane: &str) {
        match herdr_call(
            &self.cfg.herdr_socket,
            "agent.get",
            json!({ "target": pane }),
        )
        .await
        {
            Ok(info) => self.adopted(pane, &info["agent"]).await,
            Err(e) => {
                bump(&self.n.herdr_errors);
                eprintln!("agent-fleet host: adopt {pane}: {e:#}");
            }
        }
    }

    /// Up under the name `info` carries; the record it had in this pane (the pane-id stub, or
    /// any other) goes down, `adopted as <name>`, unless another pane still holds that agent.
    pub(crate) async fn adopted(self: &Arc<Self>, pane: &str, info: &Value) {
        // The agent quit since `adopt` ran (or the event is replayed after a restart): a bare
        // shell is not an agent, and nothing would ever take the phantom down.
        if info["agent"].is_null() {
            return;
        }
        let old = self
            .panes
            .lock()
            .unwrap()
            .get(pane)
            .map_or_else(|| self.agent_id(pane), |l| l.agent.clone());
        let epoch = self.agent_up(pane, info).await;
        let new = self
            .panes
            .lock()
            .unwrap()
            .get(pane)
            .map(|l| l.agent.clone());
        let held = self.panes.lock().unwrap().values().any(|l| l.agent == old);
        if let Some(new) = new.filter(|n| *n != old && !held) {
            self.publish(self.presence(Known::AgentDown(AgentDown {
                agent: old,
                reason: Some(format!("adopted as {}", crate::spawn::short(&new))),
            })))
            .await;
        }
        self.watch_pane(pane.to_string(), epoch);
    }

    /// Forgets `pane`. `agent.down` is published only if that agent is not alive in another pane
    /// (a moved pane, or herdr restored with the agent elsewhere).
    pub(crate) async fn agent_down(self: &Arc<Self>, pane: &str, reason: &str) {
        // The pane's name survives an agent that merely left it, not a closed or vanished pane.
        if reason != "agent left the pane" {
            self.names.lock().unwrap().remove(pane);
        }
        let gone = {
            let mut panes = self.panes.lock().unwrap();
            let gone = panes.remove(pane);
            gone.filter(|l| !panes.values().any(|o| o.agent == l.agent))
        };
        if let Some(l) = gone {
            // Read before `agent.down` is applied: it drops the agent, and its spawn record with it
            // once that is 24 h old. Only a real close retires; a pane its agent merely left, or a
            // herdr listing gap, leaves a live shell in the tree.
            let closed = matches!(reason, "pane closed" | "workspace closed");
            let target = closed.then(|| self.retire_target(&l.agent)).flatten();
            self.publish(self.presence(Known::AgentDown(AgentDown {
                agent: l.agent.clone(),
                reason: Some(reason.into()),
            })))
            .await;
            // A closed pane's agent is gone, so is any use of its scratch dir: remove it now,
            // not in the retire that may follow. That one can end minutes later, when a respawn
            // under the same name already uses the dir; and none follows for a handoff's old
            // agent, a tree already removed or, on a close alone, a repo without a `retire` hook.
            if closed && let Some(d) = crate::tmpdir::dir(&l.agent) {
                crate::tmpdir::remove_logged(&d);
            }
            if let Some((repo, wt, issue)) = target {
                self.retire_closed(l.agent, repo, wt, issue);
            }
        }
    }

    /// fleet.toml's `retire` for a closed agent's worktree, off the event loop: the script can
    /// take minutes. A refusal (dirty tree, unpushed commits) becomes a blocker naming the
    /// agent, never a forced retire.
    fn retire_target(&self, agent: &str) -> Option<(String, String, Option<u64>)> {
        match self.view() {
            Ok(v) => v
                .state
                .retire_target(agent, &self.cfg.host)
                // Already gone (retired by hand or by a sweep): nothing to retire or report.
                .filter(|(_, wt, _)| Path::new(wt).exists()),
            Err(e) => {
                eprintln!("agent-fleet host: retire for {agent} skipped, no view: {e:#}");
                None
            }
        }
    }
    fn retire_closed(
        self: &Arc<Self>,
        agent: String,
        repo: String,
        wt: String,
        issue: Option<u64>,
    ) {
        let host = self.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::spawn::retire_in(&host, &repo, &wt, &agent).await {
                host.retire_refused(&agent, &repo, issue, &wt, &e).await;
            }
        });
    }

    /// Say a retire did not happen: stderr, a `BLOCKED` message to the agent's lead, and a
    /// blocker on the spawn's issue naming the agent (a blocker needs a task, so a spawn with no
    /// issue gets the message only).
    pub(crate) async fn retire_refused(
        &self,
        agent: &str,
        repo: &str,
        issue: Option<u64>,
        wt: &str,
        e: &anyhow::Error,
    ) {
        eprintln!("agent-fleet host: retire {wt}: {e:#}");
        self.tell_lead(
            agent,
            repo,
            issue,
            &format!("retire of {wt} not done: {e:#}"),
        )
        .await;
        let Some(issue) = issue else { return };
        let b = Known::BlockerRaised(BlockerRaised {
            what: format!(
                "retire of {wt} not done: {}",
                crate::spawn::tail(&format!("{e:#}"))
            ),
            owner: Some(agent.to_string()),
            unblocks_when: Some("the worktree is retired by hand".into()),
            pid: None,
            started: None,
        });
        let env = Envelope::new(self.actor(), crate::spawn::task_project(repo), b)
            .task(crate::spawn::task_id(repo, issue));
        if !self.publish(env).await {
            eprintln!("agent-fleet host: the blocker for {wt} was not written");
        }
    }

    /// A refused retire, told to the closed agent's lead (its spawn's `lead`, else whoever
    /// spawned it, else the root coordinator): the changes it found and the command that decides
    /// are in `text`, so the lead can act on the message alone.
    async fn tell_lead(&self, agent: &str, repo: &str, issue: Option<u64>, text: &str) {
        let lead = self
            .view()
            .ok()
            .and_then(|v| crate::spawn::find_spawn(&v.state, agent).map(crate::spawn::lead_of));
        let reference = match issue {
            Some(n) => format!("{repo}#{n}"),
            None => repo.to_string(),
        };
        let m = Known::MsgSent(crate::event::MsgSent {
            to: lead.unwrap_or_else(|| crate::event::COORDINATOR.into()),
            kind: "BLOCKED".into(),
            reference,
            text: text.into(),
            closes: Vec::new(),
        });
        if !self
            .publish(Envelope::new(
                self.actor(),
                crate::spawn::task_project(repo),
                m,
            ))
            .await
        {
            eprintln!("agent-fleet host: the lead was not told about {agent}'s retire");
        }
    }

    /// herdr's current list, reconciled against what the hub believes about this session: agents
    /// it has but herdr does not are marked down. Returns the panes to watch.
    async fn inventory(self: &Arc<Self>) -> Result<Vec<(String, u64)>> {
        let list = herdr_call(&self.cfg.herdr_socket, "agent.list", json!({})).await?;
        let mut watch = Vec::new();
        for a in list["agents"].as_array().into_iter().flatten() {
            let pane = a["pane_id"].as_str().unwrap_or_default().to_string();
            let epoch = self.agent_up(&pane, a).await;
            watch.push((pane, epoch));
        }
        let listed: Vec<&str> = watch.iter().map(|(p, _)| p.as_str()).collect();
        let vanished: Vec<String> = self
            .panes
            .lock()
            .unwrap()
            .keys()
            .filter(|p| !listed.contains(&p.as_str()))
            .cloned()
            .collect();
        for pane in vanished {
            self.agent_down(&pane, "gone from herdr").await;
        }
        // The hub's view, not the replica's: at start the replica may not have synced yet.
        let known = match self.client.state().await {
            Ok(s) => s,
            Err(_) => self.view()?.state.clone(),
        };
        let live: Vec<String> = self
            .panes
            .lock()
            .unwrap()
            .values()
            .map(|l| l.agent.clone())
            .collect();
        let prefix = self.agent_id("");
        for agent in known.agents.into_keys() {
            if agent.starts_with(&prefix) && !live.contains(&agent) {
                self.publish(self.presence(Known::AgentDown(AgentDown {
                    agent,
                    reason: Some("gone from herdr".into()),
                })))
                .await;
            }
        }
        Ok(watch)
    }

    /// One status watcher per agent generation (herdr scopes `pane.agent_status_changed` to a
    /// pane). It re-subscribes with backoff whenever herdr drops it, and stops once the pane
    /// closes or holds a newer agent.
    fn watch_pane(self: &Arc<Self>, pane: String, epoch: u64) {
        let host = self.clone();
        tokio::spawn(async move {
            let mut wait = Duration::from_secs(1);
            while host.current(&pane, epoch) {
                match host.watch(&pane, epoch).await {
                    Ok(()) => return,
                    Err(why) => {
                        bump(&host.n.herdr_errors);
                        eprintln!("agent-fleet host: watch {pane}: {why:#}; again in {wait:?}");
                        tokio::time::sleep(wait).await;
                        wait = backoff(wait);
                    }
                }
            }
        });
    }

    /// `Ok` when the watch is over for good (agent gone, or superseded); `Err` when it was lost.
    async fn watch(self: &Arc<Self>, pane: &str, epoch: u64) -> Result<()> {
        let sub = json!([{ "type": "pane.agent_status_changed", "pane_id": pane }]);
        let mut sub = herdr_subscribe(&self.cfg.herdr_socket, sub).await?;
        // Catch up on whatever changed before the subscription existed.
        let info = herdr_call(
            &self.cfg.herdr_socket,
            "agent.get",
            json!({ "target": pane }),
        )
        .await?;
        self.observe(pane, epoch, agent_state(&info["agent"]["agent_status"]))
            .await;
        while let Some(line) = sub.lines.next_line().await? {
            if !self.current(pane, epoch) {
                return Ok(());
            }
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                eprintln!("agent-fleet host: unreadable herdr event: {line}");
                continue;
            };
            if ev["event"] != "pane.agent_status_changed" {
                continue;
            }
            if ev["data"]["agent"].is_null() {
                self.agent_down(pane, "agent left the pane").await;
                return Ok(());
            }
            self.observe(pane, epoch, agent_state(&ev["data"]["agent_status"]))
                .await;
        }
        bail!("herdr ended the subscription")
    }

    /// Publishes a state change, then records it: a waiter that sees the record can rely on the
    /// event being in the log.
    async fn observe(&self, pane: &str, epoch: u64, state: AgentState) {
        let agent = match self.panes.lock().unwrap().get(pane) {
            Some(l) if l.epoch == epoch && l.state != state => l.agent.clone(),
            _ => return,
        };
        let logged = self
            .publish(self.presence(Known::AgentState(AgentStateChanged { agent, state })))
            .await;
        if let Some(l) = self.panes.lock().unwrap().get_mut(pane)
            && l.epoch == epoch
        {
            l.state = state;
            l.working += u64::from(logged && state == AgentState::Working);
        }
        self.seen.notify_waiters();
        self.wake.notify_one();
    }

    /// Presence is push. If herdr goes away, restarts or speaks another protocol, presence
    /// retries with backoff; replica sync and delivery keep running meanwhile.
    async fn presence_loop(self: Arc<Self>) {
        let mut wait = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            if let Err(err) = self.presence_once().await {
                bump(&self.n.herdr_errors);
                eprintln!("agent-fleet host: presence: {err:#}; again in {wait:?}");
            }
            if started.elapsed() > Duration::from_secs(60) {
                wait = Duration::from_secs(1);
            }
            tokio::time::sleep(wait).await;
            wait = backoff(wait);
        }
    }

    async fn presence_once(self: &Arc<Self>) -> Result<()> {
        let pong = herdr_call(&self.cfg.herdr_socket, "ping", json!({})).await?;
        if pong["protocol"].as_u64() != Some(HERDR_PROTOCOL) {
            bail!(
                "herdr speaks protocol {} but this agent-fleet build knows {HERDR_PROTOCOL}; presence is off",
                pong["protocol"]
            );
        }
        let subs = json!([
            { "type": "pane.agent_detected" }, { "type": "pane.closed" },
            { "type": "pane.exited" }, { "type": "workspace.closed" }
        ]);
        let mut sub = herdr_subscribe(&self.cfg.herdr_socket, subs).await?;
        for (pane, epoch) in self.inventory().await? {
            self.watch_pane(pane, epoch);
        }
        // One bad line or one failed lookup is logged and skipped.
        while let Some(line) = sub.lines.next_line().await? {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                eprintln!("agent-fleet host: unreadable herdr event: {line}");
                continue;
            };
            let data = &ev["data"];
            let pane = data["pane_id"].as_str().unwrap_or_default().to_string();
            match ev["event"].as_str().unwrap_or_default() {
                "pane_agent_detected" => {
                    let target = json!({ "target": pane });
                    match herdr_call(&self.cfg.herdr_socket, "agent.get", target).await {
                        Ok(info) => {
                            let epoch = self.agent_up(&pane, &info["agent"]).await;
                            self.watch_pane(pane, epoch);
                        }
                        Err(err) => {
                            bump(&self.n.herdr_errors);
                            eprintln!("agent-fleet host: agent in {pane}: {err}");
                        }
                    }
                }
                "pane_closed" | "pane_exited" => self.agent_down(&pane, "pane closed").await,
                "workspace_closed" => {
                    let ws = format!("{}:", data["workspace_id"].as_str().unwrap_or("?"));
                    let panes: Vec<String> = self
                        .panes
                        .lock()
                        .unwrap()
                        .keys()
                        .filter(|p| p.starts_with(&ws))
                        .cloned()
                        .collect();
                    for p in panes {
                        self.agent_down(&p, "workspace closed").await;
                    }
                }
                _ => {}
            }
        }
        bail!("herdr ended the event subscription")
    }

    async fn health_loop(self: Arc<Self>) {
        // First report once the inventory has had a moment, then every minute.
        let start = tokio::time::Instant::now() + Duration::from_secs(5);
        let every = Duration::from_secs(crate::stats::HEALTH_EVERY_S as u64);
        let mut tick = tokio::time::interval_at(start, every);
        let worktree_volume = crate::scan::herdr_worktrees()
            .and_then(|p| p.ancestors().find(|a| a.exists()).map(Path::to_path_buf))
            .unwrap_or_else(|| self.cfg.data_dir.clone());
        let mut disk: Option<(u64, u64, DateTime<Utc>)> = None;
        let breakdown = Arc::new(Mutex::new(None::<(DateTime<Utc>, Vec<DiskEntry>)>));
        let breakdown_running = Arc::new(AtomicBool::new(false));
        let mut breakdown_sampled: Option<Instant> = None;
        loop {
            tick.tick().await;
            match rustix::fs::statvfs(&worktree_volume) {
                Ok(s) => {
                    disk = Some((s.f_bavail * s.f_frsize, s.f_blocks * s.f_frsize, Utc::now()));
                }
                Err(err) => eprintln!("agent-fleet host: statvfs: {err}"),
            }
            if breakdown_sampled.is_none_or(|t| t.elapsed() >= DUST_EVERY)
                && !breakdown_running.swap(true, Ordering::Relaxed)
            {
                breakdown_sampled = Some(Instant::now());
                let cache = breakdown.clone();
                let running = breakdown_running.clone();
                tokio::spawn(async move {
                    let entries = dust_breakdown().await;
                    if !entries.is_empty() {
                        *cache.lock().unwrap() = Some((Utc::now(), entries));
                    }
                    running.store(false, Ordering::Relaxed);
                });
            }
            let Some((disk_free, disk_total, sampled_at)) = disk else {
                continue;
            };
            let cached = breakdown.lock().unwrap().clone();
            let agents = self.panes.lock().unwrap().len() as u32;
            let behind_since = *self.behind_since.lock().unwrap();
            let repos = self
                .cfg
                .checkouts_now()
                .map(|c| c.keys().cloned().collect())
                .unwrap_or_default();
            self.publish(self.presence(Known::HostHealth(Box::new(HostHealth {
                host: self.cfg.host.clone(),
                disk_free,
                disk_total,
                agents,
                machine: Some(crate::stats::machine_usage()),
                disk: Some(DiskReport {
                    sampled_at,
                    breakdown_at: cached.as_ref().map(|(ts, _)| *ts),
                    breakdown: cached.map(|(_, entries)| entries).unwrap_or_default(),
                }),
                repos,
                stats: Some(self.stats()),
                commit: crate::update::COMMIT.map(str::to_string),
                behind_since,
            }))))
            .await;
        }
    }

    /// Every `SCAN_EVERY`: each agent's process tree cost, and every worktree of the repositories
    /// agents work in, sent when it changed.
    async fn scan_loop(self: Arc<Self>) {
        // agent → (cpu ms so far, when sampled); CPU % needs two samples.
        let mut prev: HashMap<String, (u64, Instant)> = HashMap::new();
        let mut quotas = Quotas::default();
        let start = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut tick = tokio::time::interval_at(start, SCAN_EVERY);
        let mut qt = tokio::time::interval_at(start, QUOTA_EVERY);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(err) = self.scan_once(&mut prev).await {
                        bump(&self.n.herdr_errors);
                        eprintln!("agent-fleet host: scan: {err:#}");
                    }
                }
                _ = qt.tick() => self.quota_once(&mut quotas).await,
            }
        }
    }

    /// Publish one program's quota when it changed (or always, for a probe whose `read_at` is
    /// itself the freshness signal), and track sources missing once per host run.
    async fn publish_quota(
        &self,
        q: &mut Quotas,
        kind: &str,
        quota: Option<AgentQuota>,
        always: bool,
    ) {
        match quota {
            Some(quota) => {
                q.missing.remove(kind);
                let new = always
                    || q.published
                        .get(kind)
                        .is_none_or(|was| crate::quota::changed(was, &quota));
                if new
                    && self
                        .publish(self.presence(Known::AgentQuota(quota.clone())))
                        .await
                {
                    q.published.insert(kind.to_string(), quota);
                }
            }
            None => {
                if q.missing.insert(kind.to_string()) {
                    eprintln!("agent-fleet host: no {kind} quota source on this machine");
                }
            }
        }
    }

    /// Every `QUOTA_EVERY`: each agent program's usage quota on this machine, sent when
    /// anything but `read_at` changed — and once at start. A source that is not there is
    /// skipped; each is logged as missing at most once per host start.
    async fn quota_once(&self, q: &mut Quotas) {
        // The devin CLI refreshes its own cache when a Devin starts; nudge it otherwise.
        if q.devin_status
            .is_none_or(|t| t.elapsed() >= DEVIN_STATUS_EVERY)
            && on_path("devin")
        {
            q.devin_status = Some(Instant::now());
            tokio::spawn(async {
                let _ = tokio::time::timeout(
                    Duration::from_secs(30),
                    tokio::process::Command::new("devin")
                        .args(["auth", "status"])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .output(),
                )
                .await;
            });
        }
        let host = &self.cfg.host;
        for (kind, read) in [
            (
                "codex",
                crate::quota::codex_sessions().map(|d| crate::quota::codex(host, &d)),
            ),
            (
                "claude",
                crate::quota::claude_file().map(|f| crate::quota::claude(host, &f)),
            ),
            (
                "devin",
                crate::quota::devin_cache().map(|d| crate::quota::devin(host, &d)),
            ),
        ] {
            self.publish_quota(q, kind, read.flatten(), false).await;
        }
        if q.agy_probe.is_none()
            && q.agy_probe_started
                .is_none_or(|t| t.elapsed() >= AGY_PROBE_EVERY)
            && on_path("agy")
        {
            q.agy_probe_started = Some(Instant::now());
            q.agy_probe = Some(tokio::spawn(agy_probe(host.clone())));
        }
        if q.agy_probe.as_ref().is_some_and(|p| p.is_finished()) {
            let read = match q.agy_probe.take() {
                Some(p) => p.await.ok().flatten(),
                None => None,
            };
            // Unlike the file/cache readers, this probe's `read_at` is the only freshness fact
            // the fleet has. Publish it even when the buckets are unchanged; a suppressed
            // heartbeat would look exactly like a dead probe ten minutes later.
            self.publish_quota(q, "agy", read, true).await;
        }
    }

    async fn scan_once(&self, prev: &mut HashMap<String, (u64, Instant)>) -> Result<()> {
        let list = herdr_call(&self.cfg.herdr_socket, "agent.list", json!({})).await?;
        // Two snapshots a second apart, so CPU is known from the first scan on.
        let (before, t0) = (scan::Procs::read(), Instant::now());
        tokio::time::sleep(Duration::from_secs(1)).await;
        let procs = scan::Procs::read();
        let gap = t0.elapsed().as_millis() as u64;
        let mut repos: BTreeSet<String> = self.cfg.repos.iter().cloned().collect();
        match self.cfg.checkouts_now() {
            Ok(c) => repos.extend(c.values().map(|d| d.to_string_lossy().into_owned())),
            Err(e) => eprintln!("agent-fleet host: connected repos: {e:#}"),
        }
        let mut agent_pids: HashSet<u32> = HashSet::new();
        let mut sampled = HashSet::new();
        for a in list["agents"].as_array().into_iter().flatten() {
            let pane = a["pane_id"].as_str().unwrap_or_default();
            let cwd = a["foreground_cwd"]
                .as_str()
                .or(a["cwd"].as_str())
                .map(str::to_string);
            let target = json!({ "pane_id": pane });
            let info = match herdr_call(&self.cfg.herdr_socket, "pane.process_info", target).await {
                Ok(info) => info,
                Err(err) => {
                    bump(&self.n.herdr_errors);
                    eprintln!("agent-fleet host: processes of {pane}: {err:#}");
                    continue;
                }
            };
            let Some(shell) = info["process_info"]["shell_pid"].as_u64() else {
                continue;
            };
            let tree = procs.tree(shell as u32);
            // Every pane herdr reports is managed, whether or not the fleet knows its agent yet.
            agent_pids.extend(&tree);
            let Some(agent) = self
                .panes
                .lock()
                .unwrap()
                .get(pane)
                .map(|l| l.agent.clone())
            else {
                continue;
            };
            if let Some(repo) = match &cwd {
                Some(c) => scan::repo_of(c).await,
                None => None,
            } {
                repos.insert(repo);
            }
            if tree.is_empty() {
                // No process tree: where the agent works still places it in a worktree.
                if cwd.is_some() {
                    self.publish(self.presence(Known::AgentUsage(AgentUsage {
                        agent,
                        rss_kb: None,
                        cpu_pct: None,
                        procs: None,
                        cwd,
                    })))
                    .await;
                }
                continue;
            }
            let (rss_kb, cpu_ms) = procs.sum(&tree);
            let now = Instant::now();
            // CPU over the whole interval since the last scan when there was one.
            let cpu_pct = match prev.insert(agent.clone(), (cpu_ms, now)) {
                Some((was, at)) => {
                    (cpu_ms.saturating_sub(was) * 100
                        / now.duration_since(at).as_millis().max(1) as u64)
                        as u32
                }
                None => procs.cpu_pct(&before, &tree, gap),
            };
            sampled.insert(agent.clone());
            self.publish(self.presence(Known::AgentUsage(AgentUsage {
                agent,
                rss_kb: Some(rss_kb),
                cpu_pct: Some(cpu_pct),
                procs: Some(tree.len() as u32),
                cwd,
            })))
            .await;
        }
        prev.retain(|a, _| sampled.contains(a));
        let view = self.view()?;
        let state = &view.state;
        let known = &state.worktrees;
        let mine = |w: &&crate::state::Worktree| w.state.host == self.cfg.host;
        repos.extend(known.values().filter(mine).map(|w| w.state.repo.clone()));
        let mut all = Vec::new();
        let herdr = scan::herdr_worktrees();
        for repo in repos {
            let found = scan::worktrees(&self.cfg.host, &repo, herdr.as_deref()).await;
            for w in &found {
                let key = crate::state::worktree_key(&w.host, &w.path);
                if known.get(&key).map(|k| &k.state) != Some(w) {
                    self.publish(self.presence(Known::WorktreeState(w.clone())))
                        .await;
                }
            }
            for k in known.values().filter(mine).filter(|k| k.state.repo == repo) {
                if !found.iter().any(|w| w.path == k.state.path) {
                    self.publish(self.presence(Known::WorktreeGone(WorktreeGone {
                        host: self.cfg.host.clone(),
                        path: k.state.path.clone(),
                    })))
                    .await;
                }
            }
            all.extend(found.into_iter().map(|w| {
                let linked = w.path != w.repo;
                (w.path, linked)
            }));
        }
        let groups = scan::by_worktree(&procs, &agent_pids, &all, |d| Path::new(d).exists());
        for (path, g) in &groups {
            let (rss_kb, _) = procs.sum(&g.pids);
            let names: BTreeSet<&str> = g
                .pids
                .iter()
                .filter_map(|p| Some(procs.0.get(p)?.name.as_str()))
                .collect();
            self.publish(self.presence(Known::WorktreeProcs(WorktreeProcs {
                host: self.cfg.host.clone(),
                path: path.clone(),
                procs: g.pids.len() as u32,
                rss_kb,
                cpu_pct: procs.cpu_pct(&before, &g.pids, gap),
                names: names.into_iter().take(8).map(str::to_string).collect(),
                pids: g.pids.iter().take(20).copied().collect(),
                orphaned: g.orphaned,
            })))
            .await;
        }
        for p in state.worktree_procs.values() {
            if p.procs.host == self.cfg.host && !groups.contains_key(&p.procs.path) {
                self.publish(self.presence(Known::WorktreeProcs(WorktreeProcs {
                    procs: 0,
                    rss_kb: 0,
                    cpu_pct: 0,
                    names: Vec::new(),
                    pids: Vec::new(),
                    ..p.procs.clone()
                })))
                .await;
            }
        }
        // Agent programs outside every herdr pane tree: every agent runs in herdr. Managed =
        // any ancestor is `herdr`; the host's own tree (its `devin auth status` refresh) is
        // out, and so is a program's own infrastructure — an `app-server`/`daemon`
        // subcommand process and everything under it.
        let own: HashSet<u32> = procs.tree(std::process::id()).into_iter().collect();
        let infra: HashSet<u32> = procs
            .0
            .iter()
            .filter(|(_, p)| crate::kill::AGENTS.contains(&p.name.as_str()))
            .filter(|&(&pid, _)| crate::scan::daemonish(pid))
            .map(|(&pid, _)| pid)
            .collect();
        let mut seen = HashSet::new();
        for (&pid, p) in &procs.0 {
            if !crate::kill::AGENTS.contains(&p.name.as_str())
                || own.contains(&pid)
                || procs.under(pid, "herdr")
                || procs.under_any(pid, &infra)
            {
                continue;
            }
            seen.insert(pid);
            let key = format!("{}:{pid}", self.cfg.host);
            let known = state.unmanaged.get(&key);
            if known.is_none_or(|u| u.program != p.name || u.cwd != p.cwd) {
                self.publish(self.presence(Known::AgentUnmanaged(AgentUnmanaged {
                    host: self.cfg.host.clone(),
                    pid,
                    program: Some(p.name.clone()),
                    cwd: p.cwd.clone(),
                    since: Some(chrono::Utc::now()),
                })))
                .await;
            }
        }
        for u in state.unmanaged.values() {
            if u.host == self.cfg.host && !seen.contains(&u.pid) {
                self.publish(self.presence(Known::AgentUnmanaged(AgentUnmanaged {
                    host: self.cfg.host.clone(),
                    pid: u.pid,
                    program: None,
                    cwd: None,
                    since: None,
                })))
                .await;
            }
        }
        // Orphans only this host can prove: a reparented device/test tool whose group
        // leader is dead (or, the older proof, whose device is gone), a lock's recorded
        // holder alive under init after its owner died, and every member of the dead
        // owner's group. A dead leader needs no device list, so a toolchain that cannot
        // answer narrows the proofs, never blocks them.
        let mut proved: HashMap<u32, String> = HashMap::new();
        let live = if procs.0.values().any(scan::may_be_stray) {
            live_devices().await
        } else {
            None
        };
        for &pid in procs.0.keys() {
            if let Some(why) = scan::stray(&procs, pid, live.as_ref(), chrono::Utc::now()) {
                proved.insert(pid, why.to_string());
            }
        }
        for (name, dir) in &self.cfg.locks {
            for pid in scan::lock_orphan(dir, &procs) {
                proved
                    .entry(pid)
                    .or_insert_with(|| format!("{name} lock owner dead"));
            }
        }
        scan::descendants(&procs, &mut proved);
        // A zombie has exited: nothing to prove orphaned or to kill.
        proved.retain(|p, _| procs.0.get(p).is_none_or(|x| !x.zombie));
        let now = chrono::Utc::now();
        let mut seen_strays = HashSet::new();
        for (pid, why) in proved {
            let cwd = procs.0.get(&pid).and_then(|p| p.cwd.clone());
            seen_strays.insert(pid);
            let key = format!("{}:{pid}", self.cfg.host);
            let known = state.orphans.get(&key);
            if known.is_none_or(|o| o.why != why || o.cwd != cwd) {
                self.publish(self.presence(Known::ProcessOrphaned(ProcessOrphaned {
                    host: self.cfg.host.clone(),
                    pid,
                    cwd,
                    why: Some(why),
                    since: Some(known.map_or(now, |o| o.since)),
                })))
                .await;
            }
        }
        for o in state.orphans.values() {
            if o.host == self.cfg.host && !seen_strays.contains(&o.pid) {
                self.publish(self.presence(Known::ProcessOrphaned(ProcessOrphaned {
                    host: self.cfg.host.clone(),
                    pid: o.pid,
                    cwd: None,
                    why: None,
                    since: None,
                })))
                .await;
            }
        }
        for (id, pid) in gone_blockers(state, &self.cfg.host, &procs) {
            let note = Some(format!("pid {pid} is gone"));
            self.publish(
                self.presence(Known::BlockerResolved(BlockerResolved { note }))
                    .re(id),
            )
            .await;
        }
        Ok(())
    }

    /// Keeps the replica current: pull from the cursor, then follow the SSE feed. Reconnects forever.
    /// Every `LOCKS_EVERY`: each watched lock's holder and queue, sent as they change. A change
    /// that could not be written is sent again on the next read.
    async fn lock_loop(self: Arc<Self>) {
        let mut seen: HashMap<String, scan::Lock> = HashMap::new();
        let mut tick = tokio::time::interval(LOCKS_EVERY);
        loop {
            tick.tick().await;
            for (name, dir) in &self.cfg.locks {
                let now = scan::read_lock(dir);
                let mut ok = true;
                for c in scan::lock_changes(seen.get(name), &now) {
                    let hold = |h: &scan::Hold| ResourceHold {
                        host: self.cfg.host.clone(),
                        resource: name.clone(),
                        pid: h.pid,
                        cwd: h.cwd.clone().or_else(|| scan::cwd_of(h.pid)),
                        scope: h.scope.clone(),
                        since: h.since,
                    };
                    let released = |pid| {
                        Known::ResourceReleased(ResourceReleased {
                            host: self.cfg.host.clone(),
                            resource: name.clone(),
                            pid,
                        })
                    };
                    let body = match c {
                        scan::LockChange::ReleasedAll => released(None),
                        scan::LockChange::Released(pid) => released(Some(pid)),
                        scan::LockChange::Taken(h) => Known::ResourceTaken(hold(h)),
                        scan::LockChange::Waiting(h) => Known::ResourceWaiting(hold(h)),
                    };
                    ok &= self.publish(self.presence(body)).await;
                }
                if ok {
                    seen.insert(name.clone(), now);
                }
            }
        }
    }

    async fn sync_loop(self: Arc<Self>) {
        let mut wait = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            if let Err(err) = self.sync_once().await {
                eprintln!("agent-fleet host: sync: {err:#}; again in {wait:?}");
            }
            if started.elapsed() > Duration::from_secs(60) {
                wait = Duration::from_secs(1);
            }
            tokio::time::sleep(wait).await;
            wait = backoff(wait);
        }
    }

    async fn sync_once(&self) -> Result<()> {
        let f = self.client.flush_spool().await?;
        if f.rejected > 0 {
            eprintln!(
                "agent-fleet host: {} spooled events were refused by the hub; see spool.rejected.jsonl",
                f.rejected
            );
        }
        let mut cursor = self
            .replica
            .lock()
            .unwrap()
            .meta("cursor")?
            .filter(|c| !c.is_empty());
        loop {
            let page = match self.client.events_after(cursor.as_deref()).await {
                Ok(page) => page,
                // The hub no longer has our cursor (its log was replaced): start over. Appends
                // are idempotent, so re-reading everything is safe.
                Err(err)
                    if err
                        .downcast_ref::<Refused>()
                        .is_some_and(|r| r.status == StatusCode::GONE) =>
                {
                    eprintln!(
                        "agent-fleet host: the hub does not know cursor {cursor:?}; re-reading its whole log"
                    );
                    self.replica.lock().unwrap().set_meta("cursor", "")?;
                    cursor = None;
                    continue;
                }
                Err(err) => return Err(err),
            };
            let Some(last) = page.last() else { break };
            cursor = Some(last.id.clone());
            self.ingest(&page, cursor.as_deref())?;
        }
        self.wake.notify_one();
        self.client
            .follow(cursor.as_deref(), |e| {
                let id = e.id.clone();
                self.ingest(&[e], Some(&id))?;
                self.wake.notify_one();
                Ok(())
            })
            .await?;
        bail!("hub closed the event stream")
    }

    fn ingest(&self, events: &[Event], cursor: Option<&str>) -> Result<()> {
        let replica = self.replica.lock().unwrap();
        for e in events {
            replica.append(e)?;
        }
        if let Some(c) = cursor {
            replica.set_meta("cursor", c)?;
        }
        Ok(())
    }

    /// Rings doorbells for this host's agents. `msg.delivered` is written only after this host
    /// has itself logged the agent turning `working` after the prompt; herdr accepting the prompt
    /// proves nothing. The exception is a busy claude agent, which queues the input: that ring is
    /// logged on submission.
    async fn delivery_loop(self: Arc<Self>) {
        let mut tried: HashMap<String, Instant> = HashMap::new();
        let mut attempts: HashMap<String, u32> = HashMap::new();
        let mut waiting: HashSet<String> = HashSet::new();
        // Queued rings typed once whose text was not seen: never typed a second time.
        let mut typed: HashSet<String> = HashSet::new();
        // Retransmits typed, by the ring count they answer: one typing per backoff cycle, even when
        // its `msg.delivered` fails to write (the next pass writes it, never types again).
        let mut retyped: HashMap<String, u32> = HashMap::new();
        // Doorbells left in a pane's mailbox, with when; and those `hook mail` took, by the ring
        // count they answer, so a lagging view never posts or rings them again.
        let mut posted: HashMap<String, (PathBuf, chrono::DateTime<chrono::Utc>)> = HashMap::new();
        let mut mailed: HashMap<String, Option<u32>> = HashMap::new();
        // `posted` starts empty: a doorbell a previous run left would never be taken back, and
        // could reach the pane's next agent or one already read. Pending tells are posted anew.
        let root = self.paths().mail_root();
        if let Err(err) = std::fs::remove_dir_all(&root)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("agent-fleet host: clearing {}: {err}", root.display());
        }
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
            let view = match self.view() {
                Ok(view) => view,
                Err(err) => {
                    eprintln!("agent-fleet host: replica: {err}");
                    continue;
                }
            };
            let now = chrono::Utc::now();
            // Expired, or its recipient gone: handed to the coordinator, never rung. The dead
            // letter's id comes from the tell's, so a retry or a second writer is the same event.
            let dead = view.state.dead_letters(&self.cfg.host, now);
            for (id, letter) in &dead {
                let mut e = self.presence(Known::MsgSent(letter.clone())).re(id);
                e.id = dead_letter_id(std::slice::from_ref(id));
                if self.publish(e).await {
                    eprintln!(
                        "agent-fleet host: {id} handed to the coordinator: {}",
                        letter.text
                    );
                }
            }
            // Status reports that waited out a reconnect: one digest per pass, never one each.
            if let Some((ids, digest)) = view.state.report_digest(&self.cfg.host, now) {
                let mut e = self.presence(Known::MsgSent(digest));
                e.id = dead_letter_id(&ids);
                if self.publish(e).await {
                    eprintln!(
                        "agent-fleet host: {} stale reports handed to the coordinator in one digest",
                        ids.len()
                    );
                }
            }
            // Unread 10 min by a working agent: its sender is told, once; the tell stays queued.
            for (id, letter) in view.state.escalations(&self.cfg.host, now) {
                let mut e = self.presence(Known::MsgSent(letter)).re(&id);
                e.id = dead_letter_id(&[id.clone(), crate::state::UNREAD.into()]);
                if self.publish(e).await {
                    eprintln!("agent-fleet host: {id} unread by a working agent; its sender told");
                }
            }
            let dead: HashSet<&String> = dead.iter().map(|(id, _)| id).collect();
            let (pending, unread) = (&view.state.msgs, &view.state.unread);
            let live = |id: &String| pending.contains_key(id) || unread.contains_key(id);
            tried.retain(|id, _| live(id));
            attempts.retain(|id, _| live(id));
            waiting.retain(live);
            typed.retain(live);
            retyped.retain(|id, _| unread.contains_key(id));
            posted.retain(|id, (file, _)| {
                live(id) || {
                    if let Err(err) = std::fs::remove_file(&*file)
                        && err.kind() != std::io::ErrorKind::NotFound
                    {
                        eprintln!("agent-fleet host: {id}: mailbox: {err}");
                    }
                    false
                }
            });
            mailed.retain(|id, _| live(id));
            // Delivered and not read (`msg.read`) once its backoff is up: rung again, like a first
            // ring, except that its old ring on the screen proves nothing. An expired one rests.
            let due = unread
                .iter()
                .filter(|(_, u)| {
                    u.rings < MAX_RINGS
                        && now - u.delivered >= retransmit_after(u.rings)
                        && !u.msg.expired(now)
                })
                .map(|(id, u)| (id, &u.msg, Some(u.rings)));
            for (id, msg, cycle) in pending.iter().map(|(id, m)| (id, m, None)).chain(due) {
                let again = cycle.is_some();
                if dead.contains(id) || msg.expired(now) {
                    continue;
                }
                // Resolved per pass: a message to `coordinator` follows the role to its holder now.
                let Some(to) = view.state.recipient(&msg.to) else {
                    continue;
                };
                // A status report that waited out a reconnect went in this pass's digest.
                if msg.stale(now).is_some() && !again {
                    continue;
                }
                let target = {
                    let panes = self.panes.lock().unwrap();
                    panes
                        .iter()
                        .find(|(_, l)| l.agent == to)
                        .map(|(p, l)| (p.clone(), l.state, l.working))
                };
                if mailed.get(id) == Some(&cycle) {
                    continue;
                }
                // A busy agent that cannot take typed input finds its doorbell in its pane's
                // mailbox, which `hook mail` hands it at its next tool call: taken (gone from
                // there) is the delivery. Not taken while the agent is working in that pane, it
                // is taken back and rung as usual.
                // A retransmit already typed this backoff step is never posted too.
                let typed_now = cycle.is_some() && retyped.get(id) == cycle.as_ref();
                let mailbox = target
                    .as_ref()
                    .filter(|(_, state, _)| {
                        !typed_now
                            && *state == AgentState::Working
                            && !crate::state::queues_input(
                                view.state.agents.get(to).and_then(|a| a.kind.as_deref()),
                            )
                    })
                    .map(|(pane, ..)| self.paths().mailbox(pane, short(to)).join(id));
                if let Some((file, at)) = posted.get(id).cloned() {
                    let waits = mailbox.as_ref() == Some(&file);
                    // Only NotFound is a take: any other error (an unreadable dir) proves nothing.
                    let res = if waits {
                        file.try_exists().map(|there| !there)
                    } else {
                        match std::fs::remove_file(&file) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
                            r => r.map(|()| false),
                        }
                    };
                    let taken = res.unwrap_or_else(|err| {
                        eprintln!("agent-fleet host: {id}: mailbox: {err}; will retry");
                        false
                    });
                    if !taken && !waits && file.exists() {
                        continue;
                    }
                    if taken {
                        let delivered = MsgDelivered {
                            to: to.to_string(),
                            observed: AgentState::Working,
                            rung: Some(at),
                            attempts: attempts.get(id).copied(),
                            ack: true,
                        };
                        if self
                            .publish(self.presence(Known::MsgDelivered(delivered)).re(id))
                            .await
                        {
                            posted.remove(id);
                            mailed.insert(id.clone(), cycle);
                            let how = if again { "retransmitted" } else { "delivered" };
                            eprintln!("agent-fleet host: {how} {id} to {} at a tool call", msg.to);
                        } else {
                            eprintln!(
                                "agent-fleet host: {id} taken by {} but msg.delivered was not written; will retry",
                                msg.to
                            );
                        }
                        continue;
                    }
                    if waits {
                        continue;
                    }
                    posted.remove(id);
                }
                if let Some(file) = mailbox {
                    let text =
                        doorbell(&msg.from, &msg.to, &msg.kind, &msg.reference, &msg.text, id);
                    match post_mail(&file, &text) {
                        Ok(()) => {
                            posted.insert(id.clone(), (file, chrono::Utc::now()));
                            *attempts.entry(id.clone()).or_default() += 1;
                            bump(&self.n.rings);
                            eprintln!(
                                "agent-fleet host: {id} to {}: working, left in its mailbox for its next tool call",
                                msg.to
                            );
                        }
                        Err(err) => eprintln!("agent-fleet host: {id}: mailbox: {err}; will retry"),
                    }
                    continue;
                }
                let Some((pane, state, before)) = target else {
                    continue;
                };
                // Never type into a blocked agent, or a busy one that cannot queue input; retry a
                // failed ring at most once a minute.
                let queue = state == AgentState::Working
                    && crate::state::queues_input(
                        view.state.agents.get(to).and_then(|a| a.kind.as_deref()),
                    );
                if !queue && !matches!(state, AgentState::Idle | AgentState::Done) {
                    if waiting.insert(id.clone()) {
                        eprintln!("agent-fleet host: {id} waits: {} is {state:?}", msg.to);
                    }
                    continue;
                }
                if tried.get(id).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
                    continue;
                }
                tried.insert(id.clone(), Instant::now());
                // Typed once as a queued ring and not seen, the agent since idle: still never a
                // second typing. Acknowledge it if it shows, else leave it pending.
                if !queue && !again && typed.contains(id) {
                    let shown = crate::stuck::screen_of(&self, &pane).await;
                    if shown.as_deref().is_some_and(|t| has_id(t, id)) {
                        self.ack_seen(id, to, msg, attempts.get(id).copied()).await;
                    }
                    continue;
                }
                // A queued ring types into a pane that may have turned blocked since the snapshot,
                // and herdr's state lags a dialog: read the screen, and never ring over one.
                if queue {
                    if self
                        .panes
                        .lock()
                        .unwrap()
                        .get(&pane)
                        .is_none_or(|l| l.state != AgentState::Working)
                    {
                        continue;
                    }
                    let shown = crate::stuck::screen_of(&self, &pane).await;
                    if shown.as_deref().is_none_or(crate::stuck::dialog_open) {
                        if waiting.insert(id.clone()) {
                            eprintln!(
                                "agent-fleet host: {id} to {}: blocked on a dialog (or screen unreadable); not rung",
                                msg.to
                            );
                        }
                        continue;
                    }
                    // Already on the screen (a retry after a missed check, or a lost
                    // `msg.delivered`): acknowledge, never type it twice. Typed once and still
                    // not seen: wait for it, do not type again.
                    if !again {
                        if shown.as_deref().is_some_and(|t| has_id(t, id)) {
                            self.ack_seen(id, to, msg, attempts.get(id).copied()).await;
                            continue;
                        }
                        if typed.contains(id) {
                            continue;
                        }
                    }
                }
                if cycle.is_some() && retyped.get(id) == cycle.as_ref() {
                    let delivered = MsgDelivered {
                        to: to.to_string(),
                        observed: state,
                        rung: None,
                        attempts: attempts.get(id).copied(),
                        ack: true,
                    };
                    if self
                        .publish(self.presence(Known::MsgDelivered(delivered)).re(id))
                        .await
                    {
                        eprintln!("agent-fleet host: retransmitted {id} to {}", msg.to);
                    }
                    continue;
                }
                let text = doorbell(&msg.from, &msg.to, &msg.kind, &msg.reference, &msg.text, id);
                // A queued ring is already `working`: there is no new turn to wait for.
                let params = if queue {
                    json!({ "target": pane, "text": text })
                } else {
                    json!({ "target": pane, "text": text, "wait": { "until": ["working"], "timeout_ms": 20000 } })
                };
                let attempt = attempts.entry(id.clone()).or_default();
                *attempt += 1;
                let attempt = *attempt;
                bump(&self.n.rings);
                let rung = chrono::Utc::now();
                match herdr_call(&self.cfg.herdr_socket, "agent.prompt", params).await {
                    // A retransmit's id is on the screen already (its first ring): seeing it proves
                    // nothing, so a retyped one counts as rung and waits for its `msg.read`.
                    Ok(_)
                        if queue
                            && !again
                            && self.queued_visible(&pane, id).await != Some(true) =>
                    {
                        typed.insert(id.clone());
                        bump(&self.n.ring_failures);
                        eprintln!(
                            "agent-fleet host: ALERT {id} to {}: typed into a busy pane, not seen in its input after {}s (or its screen was unreadable); left pending, not typed again",
                            msg.to,
                            QUEUE_CHECK.as_secs()
                        );
                    }
                    Ok(_) if queue || self.saw_working(&pane, before).await => {
                        if queue {
                            // Typed: if `msg.delivered` fails to write, never type it again.
                            typed.insert(id.clone());
                        }
                        if let Some(r) = cycle {
                            retyped.insert(id.clone(), r);
                        }
                        let delivered = MsgDelivered {
                            to: to.to_string(),
                            observed: AgentState::Working,
                            rung: Some(rung),
                            attempts: Some(attempt),
                            ack: true,
                        };
                        if self
                            .publish(self.presence(Known::MsgDelivered(delivered)).re(id))
                            .await
                        {
                            let how = if again { "retransmitted" } else { "delivered" };
                            eprintln!("agent-fleet host: {how} {id} to {}", msg.to);
                        } else {
                            eprintln!(
                                "agent-fleet host: {id} reached {} but msg.delivered was not written; will retry",
                                msg.to
                            );
                        }
                    }
                    Ok(_) => {
                        bump(&self.n.ring_failures);
                        eprintln!(
                            "agent-fleet host: {id}: prompted, but no `working` seen; will retry"
                        )
                    }
                    Err(err) => {
                        bump(&self.n.ring_failures);
                        eprintln!("agent-fleet host: {id}: {err}; will retry");
                    }
                }
            }
        }
    }

    /// After a queued ring: whether the doorbell's event id shows on the agent's screen (its
    /// input or queue area) `QUEUE_CHECK` later. Whitespace is dropped: the screen wraps lines.
    /// `msg.delivered` for a doorbell already on the agent's screen: acknowledged, not retyped.
    async fn ack_seen(&self, id: &str, to: &str, msg: &Msg, attempts: Option<u32>) {
        let delivered = MsgDelivered {
            to: to.to_string(),
            observed: AgentState::Working,
            rung: None,
            attempts: Some(attempts.unwrap_or(1)),
            ack: true,
        };
        if self
            .publish(self.presence(Known::MsgDelivered(delivered)).re(id))
            .await
        {
            eprintln!(
                "agent-fleet host: delivered {id} to {} (seen queued)",
                msg.to
            );
        }
    }

    /// This host's data dir.
    fn paths(&self) -> crate::client::Paths {
        crate::client::Paths {
            dir: self.cfg.data_dir.clone(),
        }
    }

    async fn queued_visible(&self, pane: &str, id: &str) -> Option<bool> {
        tokio::time::sleep(QUEUE_CHECK).await;
        crate::stuck::screen_of(self, pane)
            .await
            .map(|t| has_id(&t, id))
    }
}

/// The id of the dead letter (or digest) handing over the tells `ids`: a ULID made from the whole
/// set, the earliest id's time and a stable hash (FNV-1a, the same on every host) of the sorted
/// ids. The same set handed over twice is the same event, which the hub keeps once; a larger set is
/// a new one.
pub(crate) fn dead_letter_id(ids: &[String]) -> String {
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let fnv = |seed: u64| {
        sorted.iter().fold(seed, |h, id| {
            id.bytes()
                .chain(*b"\n")
                .fold(h, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
        })
    };
    let random = (u128::from(fnv(0xcbf2_9ce4_8422_2325)) << 64
        | u128::from(fnv(0x6c62_272e_07bb_0142)))
        & ((1u128 << 80) - 1);
    let ms = sorted
        .first()
        .and_then(|id| ulid::Ulid::from_string(id).ok())
        .map_or(0, |u| u.timestamp_ms());
    ulid::Ulid::from_parts(ms, random).to_string()
}

/// Leaves a doorbell at `file` in a pane's mailbox, whole or not at all: written aside, then renamed.
fn post_mail(file: &Path, text: &str) -> std::io::Result<()> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        return Err(std::io::ErrorKind::InvalidInput.into());
    };
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file)
}

/// `hook mail`: takes every doorbell in `mailbox`, oldest first (ids are ULIDs), as the text the
/// agent is given; `None` with none. Each is renamed before it is read, so this and the host taking
/// it back (`remove_file`) never both have it: whichever moves it first does.
pub(crate) fn take_mail(mailbox: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(mailbox)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let rings: Vec<String> = names
        .iter()
        .filter_map(|n| {
            let (file, taken) = (mailbox.join(n), mailbox.join(format!(".{n}.taken")));
            std::fs::rename(&file, &taken).ok()?;
            match std::fs::read_to_string(&taken) {
                Ok(text) => {
                    let _ = std::fs::remove_file(&taken);
                    Some(text)
                }
                // Put back: the host sees it still there and does not count it delivered.
                Err(err) => {
                    eprintln!("agent-fleet: mailbox {n}: {err}");
                    let _ = std::fs::rename(&taken, &file);
                    None
                }
            }
        })
        .collect();
    (!rings.is_empty()).then(|| {
        format!(
            "agent-fleet: tells sent to you while you worked. Run `agent-fleet read` now and act on them:\n{}",
            rings.join("\n")
        )
    })
}

/// `hook mail`'s output, the JSON line for the harness, from the environment `var` reads: only in a
/// herdr pane, and never under Claude Code (`CLAUDECODE`), which no mailbox is for (a `claude -p`
/// a busy Devin runs inherits its pane). herdr is asked for the pane's agent only when the pane
/// has mail, within a second, so a tool call without mail costs two directory reads.
pub(crate) async fn mail_hook(
    paths: &crate::client::Paths,
    var: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if var("HERDR_ENV").as_deref() != Some("1") || var("CLAUDECODE").is_some_and(|v| !v.is_empty())
    {
        return None;
    }
    let pane = var("HERDR_PANE_ID")?;
    let any = paths.mailbox(&pane, "x");
    let has_mail = std::fs::read_dir(any.parent()?).ok()?.flatten().any(|d| {
        std::fs::read_dir(d.path()).is_ok_and(|mut f| {
            f.any(|e| e.is_ok_and(|e| !e.file_name().to_string_lossy().starts_with('.')))
        })
    });
    if !has_mail {
        return None;
    }
    let sock = var("HERDR_SOCKET_PATH")?;
    let got = tokio::time::timeout(
        Duration::from_secs(1),
        herdr_call(sock.as_ref(), "agent.get", json!({ "target": pane })),
    )
    .await
    .ok()?
    .ok()?;
    // Named as the host names it (`agent_up`): herdr's name, else the pane id.
    let name = got["agent"]["name"].as_str().unwrap_or(&pane);
    let text = take_mail(&paths.mailbox(&pane, name))?;
    let out = json!({ "hookSpecificOutput": {
        "hookEventName": "PostToolUse", "additionalContext": text } });
    Some(out.to_string())
}

/// Deliveries after which an unread tell is rung no more (about 2 h 15 min of retransmits): an
/// agent that never runs `agent-fleet read` is not typed into all day. It stays unread on the board.
pub(crate) const MAX_RINGS: u32 = 6;

/// How long a delivered tell waits for its `msg.read` after its `rings`-th delivery before it is
/// rung again: 5 min, doubling, at most an hour.
pub(crate) fn retransmit_after(rings: u32) -> chrono::Duration {
    let first = chrono::Duration::minutes(5);
    (first * 2i32.saturating_pow(rings.saturating_sub(1))).min(chrono::Duration::hours(1))
}

/// Whether the doorbell's event `id` shows in `screen`; whitespace is dropped, the screen wraps.
fn has_id(screen: &str, id: &str) -> bool {
    screen.split_whitespace().collect::<String>().contains(id)
}

impl Host {
    /// Waits up to 20 s for this host to log the agent in `pane` turning `working` again.
    async fn saw_working(&self, pane: &str, before: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let next = self.seen.notified();
            let now = self.panes.lock().unwrap().get(pane).map(|l| l.working);
            if now.is_some_and(|n| n > before) {
                return true;
            }
            if tokio::time::timeout_at(deadline, next).await.is_err() {
                return false;
            }
        }
    }
}

/// Open blockers raised on `host` whose pid is gone or now another process, as `(id, pid)`. An
/// empty table (a platform that reads none) proves nothing.
fn gone_blockers(s: &State, host: &str, procs: &scan::Procs) -> Vec<(String, u32)> {
    if procs.0.is_empty() {
        return Vec::new();
    }
    s.blockers
        .iter()
        .filter_map(|(id, b)| {
            let pid = b.pid?;
            // `agent:<host>/<session>/<name>` or `host:<host>`.
            let raiser = b.by.split_once(':')?.1.split('/').next()?;
            let gone = procs.0.get(&pid).is_none_or(
                |p| matches!((b.started, p.started), (Some(was), Some(now)) if was != now),
            );
            (raiser == host && gone).then(|| (id.clone(), pid))
        })
        .collect()
}

pub(crate) fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// The agy log `spawn` gives the program and `stuck::agy_logs` watches. One file per agent name
/// under the host's data dir, truncated at spawn so its contents belong to that agent's run.
pub(crate) fn agy_log_path(data_dir: &Path, name: &str) -> PathBuf {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hash = 0xcbf29ce484222325u64;
    for b in name.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let file = if clean == name {
        clean
    } else {
        format!("{clean}-{hash:x}")
    };
    data_dir.join("agy").join(format!("{file}.log"))
}

/// The text typed into the agent. The `[agent-fleet …]` prefix marks it as machine-sent, never a human turn.
pub fn doorbell(from: &str, to: &str, kind: &str, reference: &str, text: &str, id: &str) -> String {
    format!(
        "[agent-fleet {}→{}] {kind} {reference}: {text} (event {id}; ack: agent-fleet read)",
        short(from),
        short(to)
    )
}

/// Runs until one of its loops stops. They are meant to run forever, so any stop (a panic, a
/// poisoned lock) ends the process, for a supervisor to restart, instead of leaving a host that
/// looks healthy and no longer syncs or delivers.
pub async fn run(cfg: Config, client: Client) -> Result<()> {
    let replica = Store::open(&cfg.replica)?;
    eprintln!(
        "agent-fleet host: host={} session={} herdr={} replica={} ({} events)",
        cfg.host,
        cfg.session,
        cfg.herdr_socket.display(),
        cfg.replica.display(),
        replica.len()?
    );
    let host = Arc::new(Host::new(cfg, client, replica));
    tokio::select! {
        r = tokio::spawn(host.clone().sync_loop()) => bail!("replica sync stopped: {r:?}"),
        r = tokio::spawn(host.clone().health_loop()) => bail!("health reports stopped: {r:?}"),
        r = tokio::spawn(host.clone().delivery_loop()) => bail!("doorbell delivery stopped: {r:?}"),
        r = tokio::spawn(host.clone().presence_loop()) => bail!("presence stopped: {r:?}"),
        r = tokio::spawn(host.clone().scan_loop()) => bail!("scans stopped: {r:?}"),
        r = tokio::spawn(host.clone().lock_loop()) => bail!("lock reads stopped: {r:?}"),
        r = tokio::spawn(crate::watch::watch_loop(host.clone())) => bail!("the watch tick stopped: {r:?}"),
        r = tokio::spawn(crate::spawn::spawn_loop(host.clone())) => bail!("spawning stopped: {r:?}"),
        r = tokio::spawn(crate::update::update_loop(host.clone())) => bail!("self-update stopped: {r:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[test]
    fn doorbell_is_prefixed() {
        let d = doorbell(
            "agent:h/s/lead",
            "agent:h/s/impl",
            "REVIEW",
            "task:p/7",
            "please look",
            "01ABC",
        );
        assert_eq!(
            d,
            "[agent-fleet lead→impl] REVIEW task:p/7: please look (event 01ABC; ack: agent-fleet read)"
        );
    }

    /// A herdr stand-in: one pane `w:1` with one agent `a`, speaking the same NDJSON socket API.
    #[derive(Default)]
    struct FakeHerdr {
        status: Mutex<String>,
        prompts: Mutex<Vec<String>>,
        /// The `target` of each `agent.read`.
        reads: Mutex<Vec<String>>,
        /// What `agent.read` shows; a prompt is appended to it when `echo` is set.
        screen: Mutex<String>,
        echo: Mutex<bool>,
        subs: tokio::sync::Mutex<Vec<OwnedWriteHalf>>,
        /// The `(target, name)` of each `agent.rename`.
        renames: Mutex<Vec<(String, String)>>,
    }

    impl FakeHerdr {
        async fn set(&self, status: &str) {
            *self.status.lock().unwrap() = status.into();
            let ev = json!({ "event": "pane.agent_status_changed", "data": {
                "agent": "claude", "agent_status": status, "pane_id": "w:1", "workspace_id": "w" } });
            for w in self.subs.lock().await.iter_mut() {
                let _ = w.write_all(format!("{ev}\n").as_bytes()).await;
            }
        }

        async fn serve(self: Arc<Self>, listener: UnixListener) {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                let fake = self.clone();
                tokio::spawn(async move {
                    let (rd, mut wr) = s.into_split();
                    let Ok(Some(line)) = BufReader::new(rd).lines().next_line().await else {
                        return;
                    };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let reply =
                        |result: Value| format!("{}\n", json!({ "id": "fleet", "result": result }));
                    match req["method"].as_str().unwrap() {
                        "events.subscribe" => {
                            wr.write_all(
                                reply(json!({ "type": "subscription_started" })).as_bytes(),
                            )
                            .await
                            .unwrap();
                            fake.subs.lock().await.push(wr);
                        }
                        "agent.get" => {
                            let status = fake.status.lock().unwrap().clone();
                            let agent = json!({ "name": "a", "agent": "claude", "agent_status": status, "pane_id": "w:1" });
                            wr.write_all(reply(json!({ "agent": agent })).as_bytes())
                                .await
                                .unwrap();
                        }
                        "agent.read" => {
                            fake.reads
                                .lock()
                                .unwrap()
                                .push(req["params"]["target"].as_str().unwrap().into());
                            let text = fake.screen.lock().unwrap().clone();
                            wr.write_all(reply(json!({ "text": text })).as_bytes())
                                .await
                                .unwrap();
                        }
                        "agent.prompt" => {
                            let text: String = req["params"]["text"].as_str().unwrap().into();
                            if *fake.echo.lock().unwrap() {
                                fake.screen.lock().unwrap().push_str(&format!("\n> {text}"));
                            }
                            fake.prompts.lock().unwrap().push(text);
                            wr.write_all(
                                reply(json!({ "agent": { "agent_status": "working" } })).as_bytes(),
                            )
                            .await
                            .unwrap();
                        }
                        "agent.rename" => {
                            let p = &req["params"];
                            fake.renames.lock().unwrap().push((
                                p["target"].as_str().unwrap().into(),
                                p["name"].as_str().unwrap().into(),
                            ));
                            wr.write_all(reply(json!({ "type": "ok" })).as_bytes())
                                .await
                                .unwrap();
                        }
                        m => panic!("unexpected herdr call {m}"),
                    }
                });
            }
        }
    }

    /// A host on a fake herdr, with a hub that is down: its writes go to the spool and replica.
    async fn host_on_fake(dir: &Path, status: &str) -> (Arc<Host>, Arc<FakeHerdr>, u64) {
        host_on_fake_msg(dir, status, "FYI", 0).await
    }
    /// As `host_on_fake`, the one queued message being a `kind` sent `age_h` hours ago.
    async fn host_on_fake_msg(
        dir: &Path,
        status: &str,
        kind: &str,
        age_h: i64,
    ) -> (Arc<Host>, Arc<FakeHerdr>, u64) {
        host_on_fake_prog(dir, status, kind, age_h, "claude", ("> ", true)).await
    }
    /// As `host_on_fake_msg`, the agent being a `program` (`claude`, `devin`).
    async fn host_on_fake_prog(
        dir: &Path,
        status: &str,
        kind: &str,
        age_h: i64,
        program: &str,
        (screen, echo): (&str, bool),
    ) -> (Arc<Host>, Arc<FakeHerdr>, u64) {
        let (host, fake, epoch) = fake_host(dir, status, program, (screen, echo)).await;
        let mut msg = tell(kind).into_event();
        msg.ts -= chrono::Duration::hours(age_h);
        host.replica.lock().unwrap().append(&msg).unwrap();
        tokio::spawn(host.clone().delivery_loop());
        host.wake.notify_one();
        (host, fake, epoch)
    }

    /// A tell from `lead` to `a`, as `agent-fleet tell` writes it.
    fn tell(kind: &str) -> Envelope<Known> {
        Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/lead".into(),
            },
            "p",
            Known::MsgSent(MsgSent {
                to: "agent:h/s/a".into(),
                kind: kind.into(),
                reference: "x".into(),
                text: "hi".into(),
                closes: Vec::new(),
            }),
        )
    }

    /// A host on a fake herdr whose one pane `w:1` holds agent `a`, a `program`; no delivery loop yet.
    async fn fake_host(
        dir: &Path,
        status: &str,
        program: &str,
        (screen, echo): (&str, bool),
    ) -> (Arc<Host>, Arc<FakeHerdr>, u64) {
        let sock = dir.join("herdr.sock");
        let fake = Arc::new(FakeHerdr::default());
        *fake.status.lock().unwrap() = status.into();
        *fake.screen.lock().unwrap() = screen.into();
        *fake.echo.lock().unwrap() = echo;
        tokio::spawn(fake.clone().serve(UnixListener::bind(&sock).unwrap()));
        let cfg = Config {
            host: "h".into(),
            session: "s".into(),
            herdr_socket: sock,
            replica: dir.join("replica.db"),
            data_dir: dir.to_path_buf(),
            repos: Vec::new(),
            checkouts: Default::default(),
            config_dir: None,
            locks: Vec::new(),
            stall_min: 15,
            waiting_min: 10,
            stuck_act: false,
        };
        let client = Client::new("http://127.0.0.1:1", "t".into(), dir.join("spool.jsonl"));
        let host = Arc::new(Host::new(
            cfg,
            client,
            Store::open(&dir.join("replica.db")).unwrap(),
        ));
        let info = json!({ "name": "a", "agent": program, "agent_status": status });
        let epoch = host.agent_up("w:1", &info).await;
        host.watch_pane("w:1".into(), epoch);
        for _ in 0..100 {
            if !fake.subs.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (host, fake, epoch)
    }

    /// A host whose replica holds a tell sent 1 h ago and delivered `ago_min` minutes ago after
    /// `rings` deliveries, then `read` by `a` if set; the delivery loop runs.
    async fn host_with_delivered(
        dir: &Path,
        status: &str,
        ago_min: i64,
        rings: u32,
        read: bool,
    ) -> (Arc<Host>, Arc<FakeHerdr>) {
        host_with_delivered_prog(dir, status, ago_min, rings, read, "claude").await
    }
    /// As `host_with_delivered`, the agent being a `program`.
    async fn host_with_delivered_prog(
        dir: &Path,
        status: &str,
        ago_min: i64,
        rings: u32,
        read: bool,
        program: &str,
    ) -> (Arc<Host>, Arc<FakeHerdr>) {
        let (host, fake, _) = fake_host(dir, status, program, ("> ", true)).await;
        let now = chrono::Utc::now();
        let mut sent = tell("ASK").into_event();
        sent.ts = now - chrono::Duration::hours(1);
        // The first ring is still in the scrollback: that is no proof it was read.
        *fake.screen.lock().unwrap() = format!("{} (event {})\n> ", "old ring", sent.id);
        let mut log = vec![sent.clone()];
        let actor = |id: &str, kind| Actor {
            kind,
            id: id.into(),
        };
        for _ in 0..rings {
            let mut d = Envelope::new(
                actor("host:h", ActorKind::Host),
                "p",
                Known::MsgDelivered(MsgDelivered {
                    to: "agent:h/s/a".into(),
                    observed: AgentState::Working,
                    rung: None,
                    attempts: Some(1),
                    ack: true,
                }),
            )
            .re(&sent.id)
            .into_event();
            d.ts = now - chrono::Duration::minutes(ago_min);
            log.push(d);
        }
        if read {
            let mut r = Envelope::new(
                actor("agent:h/s/a", ActorKind::Agent),
                "p",
                Known::MsgRead(MsgRead {}),
            )
            .re(&sent.id)
            .into_event();
            r.ts = now - chrono::Duration::minutes(ago_min - 1);
            log.push(r);
        }
        for e in &log {
            host.replica.lock().unwrap().append(e).unwrap();
        }
        tokio::spawn(host.clone().delivery_loop());
        host.wake.notify_one();
        (host, fake)
    }

    fn delivered_count(host: &Host) -> usize {
        types(host).iter().filter(|t| *t == "msg.delivered").count()
    }

    #[tokio::test]
    async fn an_unread_tell_is_retransmitted_after_its_backoff_and_a_read_one_never_is() {
        // Delivered 6 min ago, once: past the 5 min first backoff. Idle, so rung as a new turn.
        let dir = tempfile::tempdir().unwrap();
        let (host, fake) = host_with_delivered(dir.path(), "idle", 6, 1, false).await;
        for _ in 0..200 {
            if !fake.prompts.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        fake.set("working").await;
        for _ in 0..200 {
            if delivered_count(&host) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let prompts = fake.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].contains("agent-fleet read"), "{prompts:?}");
        assert_eq!(
            delivered_count(&host),
            2,
            "the retransmit is a second msg.delivered"
        );

        // Read: never retransmitted, however long ago it was delivered, busy or not.
        let dir = tempfile::tempdir().unwrap();
        let (_host, fake) = host_with_delivered(dir.path(), "working", 50, 1, true).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(fake.prompts.lock().unwrap().is_empty());

        // Rung MAX_RINGS times: left unread, never typed again.
        let dir = tempfile::tempdir().unwrap();
        let (_host, fake) = host_with_delivered(dir.path(), "idle", 120, MAX_RINGS, false).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(fake.prompts.lock().unwrap().is_empty());

        // Twice delivered, the last 6 min ago: the backoff is now 10 min, so not yet.
        let dir = tempfile::tempdir().unwrap();
        let (_host, fake) = host_with_delivered(dir.path(), "idle", 6, 2, false).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(fake.prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_busy_claude_agent_gets_an_unread_tell_retyped_though_its_old_ring_is_on_screen() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake) = host_with_delivered(dir.path(), "working", 6, 1, false).await;
        for _ in 0..200 {
            if delivered_count(&host) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(fake.prompts.lock().unwrap().len(), 1);
        assert_eq!(delivered_count(&host), 2);
    }

    #[test]
    fn a_hand_over_id_is_the_same_for_the_same_set_and_new_for_a_larger_one() {
        let (a, b, c) = (
            ulid::Ulid::new().to_string(),
            ulid::Ulid::new().to_string(),
            ulid::Ulid::new().to_string(),
        );
        let ab = dead_letter_id(&[a.clone(), b.clone()]);
        assert_eq!(ab, dead_letter_id(&[b.clone(), a.clone()]), "any order");
        assert_ne!(
            ab,
            dead_letter_id(&[a.clone(), b.clone(), c]),
            "a larger set is a new event"
        );
        assert_ne!(ab, dead_letter_id(std::slice::from_ref(&a)));
        assert!(ulid::Ulid::from_string(&ab).is_ok());
        let first = ulid::Ulid::from_string(&a).unwrap().timestamp_ms();
        assert_eq!(ulid::Ulid::from_string(&ab).unwrap().timestamp_ms(), first);
        // Pinned: every host, every build, computes the same id.
        assert_eq!(
            dead_letter_id(&["01M7TX00000000000000000000".into()]),
            "01M7TX0000HT0BP7S8N0Q4DNT9"
        );
    }

    #[test]
    fn retransmits_back_off_from_five_minutes_doubling_to_an_hour() {
        let m = chrono::Duration::minutes;
        let got: Vec<_> = (1..=6).map(retransmit_after).collect();
        assert_eq!(got, vec![m(5), m(10), m(20), m(40), m(60), m(60)]);
    }

    fn types(host: &Host) -> Vec<String> {
        host.replica
            .lock()
            .unwrap()
            .all()
            .unwrap()
            .iter()
            .map(|e| match &e.body {
                Body::Known(k) => k.type_name().to_string(),
                Body::Unknown(_) => "?".into(),
            })
            .collect()
    }

    #[tokio::test]
    async fn an_idle_agent_is_rung_once_and_delivery_follows_logged_working() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake(dir.path(), "idle").await;
        let delivered = || types(&host).contains(&"msg.delivered".to_string());
        for _ in 0..200 {
            if !fake.prompts.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Prompted, but the agent has not turned working: no delivery may be claimed yet.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!delivered(), "delivered before working was observed");
        fake.set("working").await;
        for _ in 0..200 {
            if delivered() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let t = types(&host);
        let working = t
            .iter()
            .position(|t| t == "agent.state")
            .expect("working logged");
        let delivered = t
            .iter()
            .position(|t| t == "msg.delivered")
            .expect("delivered");
        assert!(working < delivered, "{t:?}");
        let prompts = fake.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].starts_with("[agent-fleet lead→a] FYI x: hi"),
            "{prompts:?}"
        );
    }

    #[tokio::test]
    async fn a_status_report_that_waited_out_a_reconnect_goes_to_the_coordinator_in_a_digest() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake_msg(dir.path(), "idle", "MERGED", 20).await;
        let digests = || {
            host.replica
                .lock()
                .unwrap()
                .all()
                .unwrap()
                .into_iter()
                .filter_map(|e| match e.body {
                    Body::Known(Known::MsgSent(m)) if m.kind == "DEADLETTER" => Some(m),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..200 {
            if !digests().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        for _ in 0..3 {
            host.wake.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let got = digests();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0]
                .text
                .starts_with("1 status report unread over 6 h:\n- 20h"),
            "{}",
            got[0].text
        );
        assert_eq!(got[0].closes.len(), 1);
        assert!(
            fake.prompts.lock().unwrap().is_empty(),
            "a 20 h old MERGED rang"
        );
        assert!(
            !types(&host).contains(&"msg.delivered".to_string()),
            "nothing is closed unrung"
        );
    }

    #[tokio::test]
    async fn an_expired_tell_goes_to_the_coordinator_once_and_is_never_rung() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake_msg(dir.path(), "idle", "ASK", 25).await;
        let dead = || {
            let log = host.replica.lock().unwrap().all().unwrap();
            log.into_iter()
                .filter(
                    |e| matches!(&e.body, Body::Known(Known::MsgSent(m)) if m.kind == "DEADLETTER"),
                )
                .collect::<Vec<_>>()
        };
        for _ in 0..200 {
            if !dead().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // More passes: it is handed over once, never again.
        for _ in 0..3 {
            host.wake.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let got = dead();
        assert_eq!(got.len(), 1, "{got:?}");
        let Body::Known(Known::MsgSent(m)) = &got[0].body else {
            unreachable!()
        };
        assert_eq!(m.to, "coordinator");
        assert!(m.text.contains("from agent:h/s/lead to agent:h/s/a") && m.text.ends_with("] hi"));
        assert!(
            fake.prompts.lock().unwrap().is_empty(),
            "an expired tell is never rung"
        );
        let view = host.view().unwrap();
        assert!(
            view.state.msgs.values().all(|m| m.kind == "DEADLETTER"),
            "the original is closed"
        );
        // A replay of the same hand-over is the same event: the hub keeps one.
        let orig = got[0].re.clone().unwrap();
        assert_eq!(dead_letter_id(&[orig]), got[0].id);
    }

    #[tokio::test]
    async fn a_busy_claude_agent_gets_the_tell_queued() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake_msg(dir.path(), "working", "OK", 0).await;
        for _ in 0..200 {
            if types(&host).contains(&"msg.delivered".to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(fake.prompts.lock().unwrap().len(), 1);
        assert!(types(&host).contains(&"msg.delivered".to_string()));
    }

    const DIALOG: &str = "Bash command\n  rm -rf build\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No, and tell Claude what to do differently\n\nEsc to cancel";

    #[test]
    fn dialog_open_sees_a_permission_menu_and_not_prose() {
        assert!(crate::stuck::dialog_open(DIALOG));
        assert!(crate::stuck::dialog_open("Which?\n❯ 1. A\n  2. B"));
        assert!(!crate::stuck::dialog_open(
            "⏺ I will ask: Do you want to proceed? later\n\n────────────\n❯ \n────────────"
        ));
        let old = format!("{DIALOG}{}", "\nfiller".repeat(40));
        assert!(
            !crate::stuck::dialog_open(&old),
            "a dialog scrolled far up is history"
        );
    }

    #[tokio::test]
    async fn a_busy_claude_agents_screen_is_read_by_pane_not_by_name() {
        // A name can be missing or shared across herdr sessions; the pane is the one agent.
        let dir = tempfile::tempdir().unwrap();
        let (_host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "claude", ("> ", true)).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let reads = fake.reads.lock().unwrap().clone();
        assert!(!reads.is_empty(), "the screen was read");
        assert!(reads.iter().all(|t| t == "w:1"), "{reads:?}");
    }

    #[tokio::test]
    async fn a_busy_claude_agent_on_a_dialog_is_not_rung() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "claude", (DIALOG, true)).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            fake.prompts.lock().unwrap().is_empty(),
            "typed over a dialog"
        );
        assert!(!types(&host).contains(&"msg.delivered".to_string()));
    }

    #[tokio::test]
    async fn a_queued_ring_that_never_shows_in_the_input_is_left_undelivered() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "claude", ("> ", false)).await;
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert_eq!(fake.prompts.lock().unwrap().len(), 1, "it was typed once");
        assert!(!types(&host).contains(&"msg.delivered".to_string()));
    }

    #[tokio::test]
    async fn an_unseen_queued_ring_is_never_typed_twice_and_is_acked_once_seen() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "claude", ("> ", false)).await;
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(fake.prompts.lock().unwrap().len(), 1, "typed once");
        assert!(!types(&host).contains(&"msg.delivered".to_string()));
        // Past RETRY_AFTER, still unseen: no second typing.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(fake.prompts.lock().unwrap().len(), 1, "never typed twice");
        // The text turns up on the screen (a slow redraw): acknowledged without typing again.
        let typed = fake.prompts.lock().unwrap()[0].clone();
        *fake.screen.lock().unwrap() = format!("> {typed}");
        for _ in 0..200 {
            if types(&host).contains(&"msg.delivered".to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(types(&host).contains(&"msg.delivered".to_string()));
        assert_eq!(fake.prompts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unseen_queued_ring_is_not_typed_again_when_the_agent_turns_idle() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "claude", ("> ", false)).await;
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert_eq!(fake.prompts.lock().unwrap().len(), 1);
        fake.set("idle").await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(
            fake.prompts.lock().unwrap().len(),
            1,
            "typed again after idle"
        );
    }

    #[tokio::test]
    async fn a_busy_agent_is_not_rung() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 1, "devin", ("> ", true)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(fake.prompts.lock().unwrap().is_empty());
        assert!(!types(&host).contains(&"msg.delivered".to_string()));
    }

    /// Polls `f` for up to 4 s.
    async fn until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_busy_agent_that_cannot_queue_gets_the_tell_at_its_next_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "devin", ("> ", true)).await;
        let mailbox = crate::client::Paths {
            dir: dir.path().into(),
        }
        .mailbox("w:1", "a");
        let posted = || std::fs::read_dir(&mailbox).is_ok_and(|d| d.count() > 0);
        assert!(until(posted).await, "posted to the pane's mailbox");
        assert!(fake.prompts.lock().unwrap().is_empty(), "never typed");
        assert_eq!(delivered_count(&host), 0, "not delivered until taken");
        // `hook mail` at the agent's next tool call takes it: that is the delivery.
        let got = take_mail(&mailbox).unwrap();
        assert!(
            got.contains("[agent-fleet lead→a] OK x: hi") && got.contains("ack: agent-fleet read"),
            "{got}"
        );
        assert!(take_mail(&mailbox).is_none(), "taken once");
        host.wake.notify_one();
        assert!(until(|| delivered_count(&host) == 1).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        host.wake.notify_one();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(delivered_count(&host), 1, "delivered once");
        assert!(take_mail(&mailbox).is_none(), "never posted again");
        assert!(fake.prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_mailbox_tell_not_taken_is_rung_once_the_agent_turns_idle() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "devin", ("> ", true)).await;
        let mailbox = crate::client::Paths {
            dir: dir.path().into(),
        }
        .mailbox("w:1", "a");
        assert!(until(|| std::fs::read_dir(&mailbox).is_ok_and(|d| d.count() > 0)).await);
        fake.set("idle").await;
        host.wake.notify_one();
        assert!(
            until(|| !fake.prompts.lock().unwrap().is_empty()).await,
            "rung"
        );
        assert!(take_mail(&mailbox).is_none(), "taken back from the mailbox");
        assert_eq!(fake.prompts.lock().unwrap().len(), 1);
    }

    fn mail_of(dir: &Path) -> PathBuf {
        crate::client::Paths { dir: dir.into() }.mailbox("w:1", "a")
    }
    fn has_mail(dir: &Path) -> bool {
        std::fs::read_dir(mail_of(dir)).is_ok_and(|d| d.count() > 0)
    }

    #[tokio::test]
    async fn a_retransmit_to_a_working_devin_goes_through_its_mailbox_once_per_step() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake) =
            host_with_delivered_prog(dir.path(), "working", 6, 1, false, "devin").await;
        assert!(
            until(|| has_mail(dir.path())).await,
            "the retransmit is posted"
        );
        assert!(take_mail(&mail_of(dir.path())).is_some());
        host.wake.notify_one();
        assert!(until(|| delivered_count(&host) == 2).await);
        for _ in 0..2 {
            host.wake.notify_one();
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert_eq!(delivered_count(&host), 2, "one delivery for the step");
        assert!(take_mail(&mail_of(dir.path())).is_none(), "posted once");
        assert!(fake.prompts.lock().unwrap().is_empty(), "never typed");
    }

    #[tokio::test]
    async fn a_mailbox_tell_is_taken_back_when_the_agent_blocks_or_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) =
            host_on_fake_prog(dir.path(), "working", "OK", 0, "devin", ("> ", true)).await;
        assert!(until(|| has_mail(dir.path())).await);
        // Blocked on a dialog: taken back, nothing typed; working again, posted again.
        fake.set("blocked").await;
        host.wake.notify_one();
        assert!(until(|| !has_mail(dir.path())).await, "taken back");
        assert!(fake.prompts.lock().unwrap().is_empty() && delivered_count(&host) == 0);
        fake.set("working").await;
        host.wake.notify_one();
        assert!(until(|| has_mail(dir.path())).await, "posted again");
        // Read meanwhile (`agent-fleet read`): gone from the mailbox, never handed over.
        let id = host
            .view()
            .unwrap()
            .state
            .msgs
            .keys()
            .next()
            .unwrap()
            .clone();
        let read = Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::MsgRead(MsgRead {}),
        )
        .re(&id)
        .into_event();
        host.replica.lock().unwrap().append(&read).unwrap();
        host.wake.notify_one();
        assert!(until(|| !has_mail(dir.path())).await, "removed once read");
        assert_eq!(delivered_count(&host), 0);
    }

    #[tokio::test]
    async fn a_doorbell_a_previous_run_left_is_cleared_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let stale = crate::client::Paths {
            dir: dir.path().into(),
        }
        .mailbox("w:9", "gone");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("01OLD"), "old ring").unwrap();
        let (_host, _fake, _) =
            host_on_fake_prog(dir.path(), "idle", "OK", 0, "devin", ("> ", true)).await;
        assert!(until(|| !stale.join("01OLD").exists()).await);
    }

    #[tokio::test]
    async fn an_unread_tell_goes_back_to_its_sender_once_and_stays_queued() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _fake, _) = fake_host(dir.path(), "working", "devin", ("> ", true)).await;
        let lead = json!({ "name": "lead", "agent": "claude", "agent_status": "idle" });
        host.agent_up("w:2", &lead).await;
        let mut msg = tell("ASK").into_event();
        msg.ts -= chrono::Duration::minutes(11);
        host.replica.lock().unwrap().append(&msg).unwrap();
        tokio::spawn(host.clone().delivery_loop());
        host.wake.notify_one();
        assert!(until(|| host.view().unwrap().state.escalated.contains(&msg.id)).await);
        for _ in 0..2 {
            host.wake.notify_one();
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let v = host.view().unwrap();
        let letter = dead_letter_id(&[msg.id.clone(), crate::state::UNREAD.into()]);
        let back: Vec<&crate::state::Msg> = v
            .state
            .msgs
            .iter()
            .chain(v.state.unread.iter().map(|(id, u)| (id, &u.msg)))
            .filter(|(_, m)| m.kind == crate::state::UNREAD)
            .map(|(id, m)| {
                assert_eq!(id, &letter, "a stable id");
                m
            })
            .collect();
        assert_eq!(back.len(), 1, "once");
        assert_eq!(back[0].to, "agent:h/s/lead");
        assert!(v.state.msgs.contains_key(&msg.id), "the tell stays queued");
    }

    #[tokio::test]
    async fn hook_mail_hands_its_own_agent_its_mail_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, _fake, _) = fake_host(dir.path(), "working", "devin", ("> ", true)).await;
        let paths = crate::client::Paths {
            dir: dir.path().into(),
        };
        let post = |name: &str, id: &str| {
            post_mail(
                &paths.mailbox("w:1", name).join(id),
                &format!("ring for {name}"),
            )
            .unwrap()
        };
        let sock = dir.path().join("herdr.sock").to_string_lossy().to_string();
        let env = |extra: &'static [(&'static str, &'static str)]| {
            let sock = sock.clone();
            move |k: &str| {
                let base = [("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w:1")];
                if k == "HERDR_SOCKET_PATH" {
                    return Some(sock.clone());
                }
                base.iter()
                    .chain(extra)
                    .rev()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(mail_hook(&paths, env(&[])).await.is_none(), "no mail");
        // The pane's previous agent's mail is not this one's (herdr names it `a`).
        post("b", "01B");
        assert!(mail_hook(&paths, env(&[])).await.is_none());
        post("a", "01A");
        assert!(
            mail_hook(&paths, env(&[("HERDR_ENV", "0")]))
                .await
                .is_none()
        );
        assert!(
            mail_hook(&paths, env(&[("CLAUDECODE", "1")]))
                .await
                .is_none()
        );
        let out: Value = serde_json::from_str(&mail_hook(&paths, env(&[])).await.unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let text = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("ring for a") && !text.contains("ring for b"),
            "{text}"
        );
        assert!(paths.mailbox("w:1", "b").join("01B").exists());
    }

    #[test]
    fn take_mail_hands_over_each_doorbell_once_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = dir.path().join("w_1");
        assert!(take_mail(&mailbox).is_none(), "no mailbox, no mail");
        std::fs::create_dir_all(&mailbox).unwrap();
        std::fs::write(mailbox.join("01B"), "second").unwrap();
        std::fs::write(mailbox.join("01A"), "first").unwrap();
        std::fs::write(mailbox.join(".01C.tmp"), "half written").unwrap();
        let got = take_mail(&mailbox).unwrap();
        assert!(got.contains("agent-fleet read"), "{got}");
        assert!(got.find("first").unwrap() < got.find("second").unwrap());
        assert!(!got.contains("half written"));
        assert!(take_mail(&mailbox).is_none());
    }

    #[tokio::test]
    async fn a_newer_agent_in_the_pane_retires_the_old_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _fake, old) = host_on_fake(dir.path(), "working").await;
        let info = json!({ "name": "a", "agent": "claude", "agent_status": "working" });
        let new = host.agent_up("w:1", &info).await;
        assert!(new > old);
        assert!(!host.current("w:1", old) && host.current("w:1", new));
    }

    #[tokio::test]
    async fn closing_an_old_pane_does_not_down_the_agent_alive_in_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _fake, _) = host_on_fake(dir.path(), "working").await;
        let info = json!({ "name": "a", "agent": "claude", "agent_status": "idle" });
        host.agent_up("w:2", &info).await; // the same agent, now in another pane
        host.agent_down("w:1", "gone from herdr").await;
        assert!(
            !types(&host).contains(&"agent.down".to_string()),
            "{:?}",
            types(&host)
        );
        host.agent_down("w:2", "pane closed").await;
        assert!(types(&host).contains(&"agent.down".to_string()));
    }

    /// #201: herdr drops an agent's name when the agent restarts in its pane; the host used to
    /// log the next one as the pane id (`agent:h/s/w:1`), a different agent with no record.
    #[tokio::test]
    async fn a_restart_in_the_pane_keeps_the_agents_name_and_logs_the_rule() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake(dir.path(), "working").await;
        host.agent_down("w:1", "agent left the pane").await;
        let nameless = json!({ "agent": "claude", "agent_status": "idle" });
        host.agent_up("w:1", &nameless).await;
        assert!(
            host.panes
                .lock()
                .unwrap()
                .get("w:1")
                .is_some_and(|l| l.agent == "agent:h/s/a")
        );
        let renamed: Vec<_> = host
            .replica
            .lock()
            .unwrap()
            .all()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.body {
                Body::Known(Known::AgentRenamed(r)) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(renamed.len(), 1, "{renamed:?}");
        assert_eq!(
            (renamed[0].agent.as_str(), renamed[0].rule.as_str()),
            ("agent:h/s/a", "pane-name-memory")
        );
        // herdr gets its name back, so `herdr agent list` and the fleet agree
        assert_eq!(*fake.renames.lock().unwrap(), [("w:1".into(), "a".into())]);
    }

    #[tokio::test]
    async fn a_closed_pane_forgets_its_name_and_an_unnamed_pane_never_had_one() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake(dir.path(), "working").await;
        host.agent_down("w:1", "pane closed").await;
        let nameless = json!({ "agent": "claude", "agent_status": "idle" });
        host.agent_up("w:1", &nameless).await;
        host.agent_up("w:9", &nameless).await;
        let panes = host.panes.lock().unwrap();
        assert_eq!(panes["w:1"].agent, "agent:h/s/w:1");
        assert_eq!(panes["w:9"].agent, "agent:h/s/w:9");
        assert!(fake.renames.lock().unwrap().is_empty());
        assert!(!types(&host).contains(&"agent.renamed".to_string()));
    }

    #[tokio::test]
    async fn an_adopted_pane_is_up_under_its_new_name_and_the_old_record_goes_down() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _fake, _) = host_on_fake(dir.path(), "working").await;
        let info = json!({ "name": "b", "agent": "claude", "agent_status": "idle" });
        host.adopted("w:1", &info).await;
        assert_eq!(host.panes.lock().unwrap()["w:1"].agent, "agent:h/s/b");
        let downs: Vec<_> = host
            .replica
            .lock()
            .unwrap()
            .all()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.body {
                Body::Known(Known::AgentDown(d)) => Some((d.agent, d.reason)),
                _ => None,
            })
            .collect();
        assert_eq!(
            downs,
            [("agent:h/s/a".to_string(), Some("adopted as b".to_string()))]
        );
    }

    #[tokio::test]
    async fn adopting_a_pane_with_no_agent_in_it_registers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _fake, _) = host_on_fake(dir.path(), "working").await;
        host.adopted("w:9", &json!({ "name": "b", "agent_status": "idle" }))
            .await;
        assert!(!host.panes.lock().unwrap().contains_key("w:9"));
    }

    #[tokio::test]
    async fn a_closed_pane_takes_its_agents_scratch_dir_and_a_left_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _fake, _) = host_on_fake(dir.path(), "working").await;
        let scratch = crate::tmpdir::dir("agent:h/s/a").unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("x"), "x").unwrap();
        host.agent_down("w:1", "agent left the pane").await;
        assert!(scratch.exists(), "a live shell may still use it");
        let info = json!({ "name": "a", "agent": "claude", "agent_status": "idle" });
        host.agent_up("w:1", &info).await;
        host.agent_down("w:1", "pane closed").await;
        assert!(!scratch.exists());
    }

    /// `view` folds the log once, then only applies what arrived: a quiet log hands back the
    /// same snapshot rather than re-reading and re-folding every row.
    #[test]
    fn the_view_is_kept_live_not_refolded() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            host: "h".into(),
            session: "s".into(),
            herdr_socket: dir.path().join("none.sock"),
            replica: dir.path().join("replica.db"),
            data_dir: dir.path().to_path_buf(),
            repos: Vec::new(),
            checkouts: Default::default(),
            config_dir: None,
            locks: Vec::new(),
            stall_min: 15,
            waiting_min: 10,
            stuck_act: false,
        };
        let host = Host::new(
            cfg,
            Client::new(
                "http://127.0.0.1:1",
                "t".into(),
                dir.path().join("spool.jsonl"),
            ),
            Store::open(&dir.path().join("replica.db")).unwrap(),
        );
        let first = host.view().unwrap();
        assert!(
            Arc::ptr_eq(&first, &host.view().unwrap()),
            "nothing new: the same snapshot, no re-read"
        );
        let e = Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::TaskCreated(TaskCreated {
                title: "t".into(),
                url: None,
            }),
        )
        .task("task:p/1")
        .into_event();
        host.replica.lock().unwrap().append(&e).unwrap();
        let grown = host.view().unwrap();
        assert!(!Arc::ptr_eq(&first, &grown));
        assert_eq!(grown.events.last().map(|e| &e.id), Some(&e.id));
        assert_eq!(grown.state, State::fold(&grown.events));
        assert!(Arc::ptr_eq(&grown, &host.view().unwrap()));
    }

    #[tokio::test]
    async fn sync_starts_over_when_the_hub_forgot_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::hub::router(
            crate::hub::Hub::new(Store::open(&dir.path().join("hub.db")).unwrap()).unwrap(),
            "t".into(),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(&base, "t".into(), dir.path().join("spool.jsonl"));
        for n in 0..2 {
            let e = Envelope::new(
                Actor {
                    kind: ActorKind::Agent,
                    id: "agent:h/s/a".into(),
                },
                "p",
                Known::BlockerRaised(BlockerRaised {
                    what: format!("b{n}"),
                    owner: None,
                    unblocks_when: None,
                    pid: None,
                    started: None,
                }),
            )
            .task("task:p/1");
            client.write(&e).await.unwrap();
        }
        let cfg = Config {
            host: "h".into(),
            session: "s".into(),
            herdr_socket: dir.path().join("none.sock"),
            replica: dir.path().join("replica.db"),
            data_dir: dir.path().to_path_buf(),
            repos: Vec::new(),
            checkouts: Default::default(),
            config_dir: None,
            locks: Vec::new(),
            stall_min: 15,
            waiting_min: 10,
            stuck_act: false,
        };
        let replica = Store::open(&cfg.replica).unwrap();
        replica
            .set_meta("cursor", "01J00000000000000000000000")
            .unwrap();
        let host = Host::new(cfg, client, replica);
        let _ = tokio::time::timeout(Duration::from_secs(1), host.sync_once()).await;
        assert_eq!(host.replica.lock().unwrap().len().unwrap(), 2);
    }

    #[test]
    fn a_blocker_whose_pid_is_gone_or_reused_is_resolved_by_its_host() {
        let t0 = chrono::Utc::now();
        let proc_at = |started| scan::Proc {
            ppid: 1,
            cpu_ms: 0,
            rss_kb: 0,
            name: "sim".into(),
            cwd: None,
            deleted: false,
            args: None,
            started,
            pgid: None,
            zombie: false,
        };
        let procs = scan::Procs(HashMap::from([
            (10, proc_at(Some(t0))),
            (11, proc_at(Some(t0 + chrono::Duration::seconds(5)))),
            (12, proc_at(None)),
        ]));
        let mut s = State::default();
        for (id, by, pid) in [
            ("alive", "agent:h/s/a", Some(10)),
            ("reused", "agent:h/s/a", Some(11)),
            ("unreadable", "agent:h/s/a", Some(12)),
            ("dead", "host:h", Some(13)),
            ("elsewhere", "agent:other/s/a", Some(13)),
            ("no-pid", "agent:h/s/a", None),
        ] {
            s.blockers.insert(
                id.into(),
                crate::state::Blocker {
                    task: None,
                    what: "sim lock".into(),
                    owner: None,
                    unblocks_when: None,
                    pid,
                    started: pid.map(|_| t0),
                    by: by.into(),
                    since: t0,
                },
            );
        }
        let mut gone = gone_blockers(&s, "h", &procs);
        gone.sort();
        assert_eq!(gone, [("dead".into(), 13), ("reused".into(), 11)]);
        let empty = scan::Procs::default();
        assert_eq!(
            gone_blockers(&s, "h", &empty),
            [],
            "no table proves nothing"
        );
    }
}
