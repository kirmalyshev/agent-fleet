//! `agent-fleet host`: one per machine and herdr session. Presence from herdr, disk health, a local replica of the log,
//! and doorbell delivery into herdr agents.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::state::State;
use crate::store::Store;

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

struct Host {
    cfg: Config,
    client: Client,
    replica: Mutex<Store>,
    /// herdr pane id → the agent in it.
    panes: Mutex<HashMap<String, Local>>,
    next_epoch: AtomicU64,
    /// Wakes the delivery loop.
    wake: Notify,
    /// Fires after an `agent.state` is recorded.
    seen: Notify,
    started: Instant,
    n: Counters,
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

/// How often the host samples agents' processes and the worktrees.
const SCAN_EVERY: Duration = Duration::from_secs(300);

fn backoff(d: Duration) -> Duration {
    (d * 2).min(Duration::from_secs(60))
}

impl Host {
    fn new(cfg: Config, client: Client, replica: Store) -> Self {
        Self {
            cfg,
            client,
            replica: Mutex::new(replica),
            panes: Mutex::default(),
            next_epoch: AtomicU64::new(1),
            wake: Notify::new(),
            seen: Notify::new(),
            started: Instant::now(),
            n: Counters::default(),
        }
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

    fn actor(&self) -> Actor {
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
    async fn publish(&self, e: Envelope<Known>) -> bool {
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

    fn presence(&self, body: Known) -> Envelope<Known> {
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
    async fn agent_up(&self, pane: &str, info: &Value) -> u64 {
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
    async fn agent_down(&self, pane: &str, reason: &str) {
        let gone = {
            let mut panes = self.panes.lock().unwrap();
            let gone = panes.remove(pane);
            gone.filter(|l| !panes.values().any(|o| o.agent == l.agent))
        };
        if let Some(l) = gone {
            self.publish(self.presence(Known::AgentDown(AgentDown {
                agent: l.agent,
                reason: Some(reason.into()),
            })))
            .await;
        }
    }

    /// herdr's current list, reconciled against what the hub believes about this session: agents
    /// it has but herdr does not are marked down. Returns the panes to watch.
    async fn inventory(&self) -> Result<Vec<(String, u64)>> {
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
            Err(_) => State::fold(&self.replica.lock().unwrap().all()?),
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
    async fn watch(&self, pane: &str, epoch: u64) -> Result<()> {
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
                    self.publish(self.presence(Known::HostHealth(HostHealth {
                        host: self.cfg.host.clone(),
                        disk_free: s.f_bavail * s.f_frsize,
                        disk_total: s.f_blocks * s.f_frsize,
                        agents,
                        stats: Some(self.stats()),
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
        let start = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut tick = tokio::time::interval_at(start, SCAN_EVERY);
        loop {
            tick.tick().await;
            if let Err(err) = self.scan_once(&mut prev).await {
                bump(&self.n.herdr_errors);
                eprintln!("agent-fleet host: scan: {err:#}");
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
        let mut agent_pids: HashSet<u32> = HashSet::new();
        let mut sampled = HashSet::new();
        for a in list["agents"].as_array().into_iter().flatten() {
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
            let cwd = a["foreground_cwd"]
                .as_str()
                .or(a["cwd"].as_str())
                .map(str::to_string);
            if let Some(repo) = match &cwd {
                Some(c) => scan::repo_of(c).await,
                None => None,
            } {
                repos.insert(repo);
            }
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
            if tree.is_empty() {
                continue;
            }
            agent_pids.extend(&tree);
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
                rss_kb,
                cpu_pct,
                procs: tree.len() as u32,
                cwd,
            })))
            .await;
        }
        prev.retain(|a, _| sampled.contains(a));
        // ponytail: refolds the replica per scan, like delivery; share one live State if it grows.
        let state = State::fold(&self.replica.lock().unwrap().all()?);
        let known = &state.worktrees;
        let mine = |w: &&crate::state::Worktree| w.state.host == self.cfg.host;
        repos.extend(known.values().filter(mine).map(|w| w.state.repo.clone()));
        let mut all = Vec::new();
        for repo in repos {
            let found = scan::worktrees(&self.cfg.host, &repo).await;
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
        Ok(())
    }

    /// Keeps the replica current: pull from the cursor, then follow the SSE feed. Reconnects forever.
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
    /// has itself logged the agent turning `working` after the prompt; herdr accepting the
    /// prompt proves nothing.
    async fn delivery_loop(self: Arc<Self>) {
        let mut tried: HashMap<String, Instant> = HashMap::new();
        let mut attempts: HashMap<String, u32> = HashMap::new();
        let mut waiting: HashSet<String> = HashSet::new();
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
            // ponytail: refolds the whole replica per wake; keep a live State here once the log is large.
            let pending = match self.replica.lock().unwrap().all() {
                Ok(events) => State::fold(&events).msgs,
                Err(err) => {
                    eprintln!("agent-fleet host: replica: {err}");
                    continue;
                }
            };
            tried.retain(|id, _| pending.contains_key(id));
            attempts.retain(|id, _| pending.contains_key(id));
            waiting.retain(|id| pending.contains_key(id));
            for (id, msg) in pending {
                let target = {
                    let panes = self.panes.lock().unwrap();
                    panes
                        .iter()
                        .find(|(_, l)| l.agent == msg.to)
                        .map(|(p, l)| (p.clone(), l.state, l.working))
                };
                let Some((pane, state, before)) = target else {
                    continue;
                };
                // Never type into a busy or blocked agent; retry a failed ring at most once a minute.
                if !matches!(state, AgentState::Idle | AgentState::Done) {
                    if waiting.insert(id.clone()) {
                        eprintln!("agent-fleet host: {id} waits: {} is {state:?}", msg.to);
                    }
                    continue;
                }
                if tried
                    .get(&id)
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
                {
                    continue;
                }
                tried.insert(id.clone(), Instant::now());
                let text = doorbell(
                    &msg.from,
                    &msg.to,
                    &msg.kind,
                    &msg.reference,
                    &msg.text,
                    &id,
                );
                let params = json!({ "target": pane, "text": text, "wait": { "until": ["working"], "timeout_ms": 20000 } });
                let attempt = attempts.entry(id.clone()).or_default();
                *attempt += 1;
                let attempt = *attempt;
                bump(&self.n.rings);
                let rung = chrono::Utc::now();
                match herdr_call(&self.cfg.herdr_socket, "agent.prompt", params).await {
                    Ok(_) if self.saw_working(&pane, before).await => {
                        let delivered = MsgDelivered {
                            to: msg.to.clone(),
                            observed: AgentState::Working,
                            rung: Some(rung),
                            attempts: Some(attempt),
                        };
                        if self
                            .publish(self.presence(Known::MsgDelivered(delivered)).re(&id))
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

fn short(id: &str) -> &str {
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
                        "agent.prompt" => {
                            fake.prompts
                                .lock()
                                .unwrap()
                                .push(req["params"]["text"].as_str().unwrap().into());
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
        let sock = dir.join("herdr.sock");
        let fake = Arc::new(FakeHerdr::default());
        *fake.status.lock().unwrap() = status.into();
        tokio::spawn(fake.clone().serve(UnixListener::bind(&sock).unwrap()));
        let cfg = Config {
            host: "h".into(),
            session: "s".into(),
            herdr_socket: sock,
            replica: dir.join("replica.db"),
            data_dir: dir.to_path_buf(),
            repos: Vec::new(),
        };
        let client = Client::new("http://127.0.0.1:1", "t".into(), dir.join("spool.jsonl"));
        let host = Arc::new(Host::new(
            cfg,
            client,
            Store::open(&dir.join("replica.db")).unwrap(),
        ));
        let info = json!({ "name": "a", "agent": "claude", "agent_status": status });
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
                kind: "FYI".into(),
                reference: "x".into(),
                text: "hi".into(),
            }),
        );
        host.replica
            .lock()
            .unwrap()
            .append(&msg.into_event())
            .unwrap();
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
    async fn a_busy_agent_is_not_rung() {
        let dir = tempfile::tempdir().unwrap();
        let (host, fake, _) = host_on_fake(dir.path(), "working").await;
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
        };
        let replica = Store::open(&cfg.replica).unwrap();
        replica
            .set_meta("cursor", "01J00000000000000000000000")
            .unwrap();
        let host = Host::new(cfg, client, replica);
        let _ = tokio::time::timeout(Duration::from_secs(1), host.sync_once()).await;
        assert_eq!(host.replica.lock().unwrap().len().unwrap(), 2);
    }
}
