//! `agent-fleet host`: one per machine and herdr session. Presence from herdr, disk health, a local replica of the log,
//! and doorbell delivery into herdr agents.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
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

/// `devin auth status` refreshes devin's cached quota; run at most this often.
const DEVIN_STATUS_EVERY: Duration = Duration::from_secs(15 * 60);

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

impl Host {
    pub(crate) fn new(cfg: Config, client: Client, replica: Store) -> Self {
        Self {
            cfg,
            client,
            replica: Mutex::new(replica),
            view: Mutex::default(),
            panes: Mutex::default(),
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
        let agent = self.agent_id(info["name"].as_str().unwrap_or(pane));
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

    /// Forgets `pane`. `agent.down` is published only if that agent is not alive in another pane
    /// (a moved pane, or herdr restored with the agent elsewhere).
    pub(crate) async fn agent_down(self: &Arc<Self>, pane: &str, reason: &str) {
        let gone = {
            let mut panes = self.panes.lock().unwrap();
            let gone = panes.remove(pane);
            gone.filter(|l| !panes.values().any(|o| o.agent == l.agent))
        };
        if let Some(l) = gone {
            // Read before `agent.down` is applied: it drops the agent, and its spawn record with it
            // once that is 24 h old. Only a real close retires; a pane its agent merely left, or a
            // herdr listing gap, leaves a live shell in the tree.
            let target = matches!(reason, "pane closed" | "workspace closed")
                .then(|| self.retire_target(&l.agent))
                .flatten();
            self.publish(self.presence(Known::AgentDown(AgentDown {
                agent: l.agent.clone(),
                reason: Some(reason.into()),
            })))
            .await;
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
        loop {
            tick.tick().await;
            match rustix::fs::statvfs(&self.cfg.data_dir) {
                Ok(s) => {
                    let agents = self.panes.lock().unwrap().len() as u32;
                    let behind_since = *self.behind_since.lock().unwrap();
                    self.publish(self.presence(Known::HostHealth(HostHealth {
                        host: self.cfg.host.clone(),
                        disk_free: s.f_bavail * s.f_frsize,
                        disk_total: s.f_blocks * s.f_frsize,
                        agents,
                        stats: Some(self.stats()),
                        commit: crate::update::COMMIT.map(str::to_string),
                        behind_since,
                    })))
                    .await;
                }
                Err(err) => eprintln!("agent-fleet host: statvfs: {err}"),
            }
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
            match read.flatten() {
                Some(quota) => {
                    q.missing.remove(kind);
                    let new = q
                        .published
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
            let pending = &view.state.msgs;
            tried.retain(|id, _| pending.contains_key(id));
            attempts.retain(|id, _| pending.contains_key(id));
            waiting.retain(|id| pending.contains_key(id));
            typed.retain(|id| pending.contains_key(id));
            for (id, msg) in pending {
                // Resolved per pass: a message to `coordinator` follows the role to its holder now.
                let Some(to) = view.state.recipient(&msg.to) else {
                    continue;
                };
                // A status report that waited out a reconnect is history: close it unrung.
                if let Some(why) = msg.stale(chrono::Utc::now()) {
                    let dropped = MsgDelivered {
                        to: to.to_string(),
                        observed: AgentState::Idle,
                        rung: None,
                        attempts: Some(0),
                    };
                    if self
                        .publish(self.presence(Known::MsgDelivered(dropped)).re(id))
                        .await
                    {
                        eprintln!(
                            "agent-fleet host: {id} to {} dropped, not rung: {why}",
                            msg.to
                        );
                    }
                    continue;
                }
                let target = {
                    let panes = self.panes.lock().unwrap();
                    panes
                        .iter()
                        .find(|(_, l)| l.agent == to)
                        .map(|(p, l)| (p.clone(), l.state, l.working))
                };
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
                if !queue && typed.contains(id) {
                    let shown = crate::stuck::screen(&self, to).await;
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
                    let shown = crate::stuck::screen(&self, to).await;
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
                    if shown.as_deref().is_some_and(|t| has_id(t, id)) {
                        self.ack_seen(id, to, msg, attempts.get(id).copied()).await;
                        continue;
                    }
                    if typed.contains(id) {
                        continue;
                    }
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
                    Ok(_) if queue && self.queued_visible(to, id).await != Some(true) => {
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
                        let delivered = MsgDelivered {
                            to: to.to_string(),
                            observed: AgentState::Working,
                            rung: Some(rung),
                            attempts: Some(attempt),
                        };
                        if self
                            .publish(self.presence(Known::MsgDelivered(delivered)).re(id))
                            .await
                        {
                            eprintln!("agent-fleet host: delivered {id} to {}", msg.to);
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

    async fn queued_visible(&self, to: &str, id: &str) -> Option<bool> {
        tokio::time::sleep(QUEUE_CHECK).await;
        crate::stuck::screen(self, to).await.map(|t| has_id(&t, id))
    }
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

/// The text typed into the agent. The `[agent-fleet …]` prefix marks it as machine-sent, never a human turn.
pub fn doorbell(from: &str, to: &str, kind: &str, reference: &str, text: &str, id: &str) -> String {
    format!(
        "[agent-fleet {}→{}] {kind} {reference}: {text} (event {id})",
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
            "[agent-fleet lead→impl] REVIEW task:p/7: please look (event 01ABC)"
        );
    }

    /// A herdr stand-in: one pane `w:1` with one agent `a`, speaking the same NDJSON socket API.
    #[derive(Default)]
    struct FakeHerdr {
        status: Mutex<String>,
        prompts: Mutex<Vec<String>>,
        /// What `agent.read` shows; a prompt is appended to it when `echo` is set.
        screen: Mutex<String>,
        echo: Mutex<bool>,
        subs: tokio::sync::Mutex<Vec<OwnedWriteHalf>>,
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
        let msg = Envelope::new(
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
            }),
        );
        let mut msg = msg.into_event();
        msg.ts -= chrono::Duration::hours(age_h);
        host.replica.lock().unwrap().append(&msg).unwrap();
        tokio::spawn(host.clone().delivery_loop());
        host.wake.notify_one();
        (host, fake, epoch)
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
    async fn a_status_report_that_waited_out_a_reconnect_is_dropped_not_rung() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake_msg(dir.path(), "idle", "MERGED", 20).await;
        for _ in 0..200 {
            if types(&host).contains(&"msg.delivered".to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            fake.prompts.lock().unwrap().is_empty(),
            "a 20 h old MERGED rang"
        );
        assert!(
            types(&host).contains(&"msg.delivered".to_string()),
            "dropped, so it leaves the undelivered list"
        );
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
