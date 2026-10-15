//! `fleet host`: one per machine. Presence from herdr, disk health, a local replica of the log,
//! and doorbell delivery into herdr agents.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Notify;

use crate::client::{Client, Sent};
use crate::event::*;
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
}

#[derive(Clone)]
struct Local {
    agent: String,
    state: AgentState,
    /// How many times this host has published the agent turning `working`.
    working: u64,
}

struct Host {
    cfg: Config,
    client: Client,
    replica: Mutex<Store>,
    /// herdr pane id → the agent in it.
    panes: Mutex<HashMap<String, Local>>,
    /// Wakes the delivery loop.
    wake: Notify,
    /// Fires after an `agent.state` is published.
    seen: Notify,
    /// Panes with a live status watcher, so a pane is never watched twice.
    watched: Mutex<HashSet<String>>,
}

impl Host {
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
    /// An event the hub refused stays out of the replica.
    async fn publish(&self, e: Envelope<Known>) {
        let ty = e.body.type_name();
        match self.client.write(&e).await {
            Ok(Sent::Spooled(why)) => eprintln!("fleet host: {ty} spooled ({why})"),
            Ok(Sent::Accepted { .. }) => {}
            Err(err) => return eprintln!("fleet host: {ty} refused: {err}"),
        }
        if let Err(err) = self.replica.lock().unwrap().append(&e.into_event()) {
            eprintln!("fleet host: replica append: {err}");
        }
    }

    fn presence(&self, body: Known) -> Envelope<Known> {
        Envelope::new(self.actor(), self.cfg.session.clone(), body)
    }

    async fn agent_up(&self, pane: &str, info: &Value) {
        let name = info["name"].as_str().unwrap_or(pane);
        let local = Local {
            agent: self.agent_id(name),
            state: agent_state(&info["agent_status"]),
            working: 0,
        };
        self.panes
            .lock()
            .unwrap()
            .insert(pane.to_string(), local.clone());
        self.publish(self.presence(Known::AgentUp(AgentUp {
            agent: local.agent,
            host: self.cfg.host.clone(),
            kind: info["agent"].as_str().map(str::to_string),
            state: local.state,
            pane: Some(pane.to_string()),
        })))
        .await;
        self.wake.notify_one();
    }

    async fn agent_down(&self, pane: &str, reason: &str) {
        let gone = self.panes.lock().unwrap().remove(pane);
        if let Some(l) = gone {
            self.publish(self.presence(Known::AgentDown(AgentDown {
                agent: l.agent,
                reason: Some(reason.into()),
            })))
            .await;
        }
    }

    /// Initial inventory, then reconcile: agents the log thinks are up on this session but herdr
    /// no longer has are marked down.
    async fn inventory(self: &Arc<Self>) -> Result<Vec<String>> {
        let list = herdr_call(&self.cfg.herdr_socket, "agent.list", json!({})).await?;
        let mut panes = Vec::new();
        for a in list["agents"].as_array().into_iter().flatten() {
            let pane = a["pane_id"].as_str().unwrap_or_default().to_string();
            self.agent_up(&pane, a).await;
            panes.push(pane);
        }
        let live: Vec<String> = self
            .panes
            .lock()
            .unwrap()
            .values()
            .map(|l| l.agent.clone())
            .collect();
        let prefix = self.agent_id("");
        let stale: Vec<String> = {
            let replica = self.replica.lock().unwrap();
            State::fold(&replica.all()?)
                .agents
                .into_keys()
                .filter(|a| a.starts_with(&prefix) && !live.contains(a))
                .collect()
        };
        for agent in stale {
            self.publish(self.presence(Known::AgentDown(AgentDown {
                agent,
                reason: Some("gone at host start".into()),
            })))
            .await;
        }
        Ok(panes)
    }

    /// One subscription per agent pane: herdr scopes `pane.agent_status_changed` to a pane.
    fn watch_pane(self: &Arc<Self>, pane: String) {
        if !self.watched.lock().unwrap().insert(pane.clone()) {
            return;
        }
        let host = self.clone();
        tokio::spawn(async move {
            host.watch(&pane).await;
            host.watched.lock().unwrap().remove(&pane);
        });
    }

    async fn watch(&self, pane: &str) {
        let sub = json!([{ "type": "pane.agent_status_changed", "pane_id": pane }]);
        let mut sub = match herdr_subscribe(&self.cfg.herdr_socket, sub).await {
            Ok(s) => s,
            Err(err) => return eprintln!("fleet host: watch {pane}: {err}"),
        };
        while let Ok(Some(line)) = sub.lines.next_line().await {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let data = &ev["data"];
            if data["agent"].is_null() {
                return self.agent_down(pane, "agent left the pane").await;
            }
            let state = agent_state(&data["agent_status"]);
            let agent = match self.panes.lock().unwrap().get(pane) {
                Some(l) if l.state != state => l.agent.clone(),
                _ => continue,
            };
            // Publish first, then record: whatever waits on the record sees the event in the log.
            self.publish(self.presence(Known::AgentState(AgentStateChanged { agent, state })))
                .await;
            if let Some(l) = self.panes.lock().unwrap().get_mut(pane) {
                l.state = state;
                l.working += u64::from(state == AgentState::Working);
            }
            self.seen.notify_waiters();
            self.wake.notify_one();
        }
    }

    /// Presence is push: lifecycle events from herdr, plus a status watcher per agent pane.
    async fn presence_loop(self: Arc<Self>) -> Result<()> {
        let pong = herdr_call(&self.cfg.herdr_socket, "ping", json!({})).await?;
        if pong["protocol"].as_u64() != Some(HERDR_PROTOCOL) {
            bail!(
                "herdr speaks protocol {} but this fleet build knows {HERDR_PROTOCOL}: presence disabled",
                pong["protocol"]
            );
        }
        let subs = json!([
            { "type": "workspace.created" }, { "type": "workspace.closed" },
            { "type": "pane.agent_detected" }, { "type": "pane.closed" }, { "type": "pane.exited" }
        ]);
        let mut sub = herdr_subscribe(&self.cfg.herdr_socket, subs).await?;
        for pane in self.inventory().await? {
            self.watch_pane(pane);
        }
        // One bad line or one failed lookup is logged and skipped; it never stops the host.
        while let Some(line) = sub.lines.next_line().await? {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                eprintln!("fleet host: unreadable herdr event: {line}");
                continue;
            };
            let data = &ev["data"];
            let pane = data["pane_id"].as_str().unwrap_or_default().to_string();
            match ev["event"].as_str().unwrap_or_default() {
                "pane_agent_detected" => {
                    let target = json!({ "target": pane });
                    match herdr_call(&self.cfg.herdr_socket, "agent.get", target).await {
                        Ok(info) => {
                            self.agent_up(&pane, &info["agent"]).await;
                            self.watch_pane(pane);
                        }
                        Err(err) => eprintln!("fleet host: agent in {pane}: {err}"),
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
        // First report once the inventory has had a moment, then every 5 minutes.
        let start = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut tick = tokio::time::interval_at(start, Duration::from_secs(300));
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
                    })))
                    .await;
                }
                Err(err) => eprintln!("fleet host: statvfs: {err}"),
            }
        }
    }

    /// Keeps the replica current: pull from the cursor, then follow the SSE feed. Reconnects forever.
    async fn sync_loop(self: Arc<Self>) {
        loop {
            if let Err(err) = self.sync_once().await {
                eprintln!("fleet host: sync: {err:#}");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn sync_once(&self) -> Result<()> {
        self.client.flush_spool().await?;
        let mut cursor = self.replica.lock().unwrap().meta("cursor")?;
        loop {
            let page = self.client.events_after(cursor.as_deref()).await?;
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

    /// Rings doorbells for this host's agents. `msg.delivered` is written only after herdr reports
    /// the agent `working` following the prompt: a submitted prompt alone proves nothing.
    async fn delivery_loop(self: Arc<Self>) {
        let mut tried: HashMap<String, Instant> = HashMap::new();
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
            // ponytail: refolds the whole replica per wake; keep a live State here once the log is large.
            let pending = match self.replica.lock().unwrap().all() {
                Ok(events) => State::fold(&events).msgs,
                Err(err) => {
                    eprintln!("fleet host: replica: {err}");
                    continue;
                }
            };
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
                match herdr_call(&self.cfg.herdr_socket, "agent.prompt", params).await {
                    Ok(_) if self.saw_working(&pane, before).await => {
                        self.publish(
                            self.presence(Known::MsgDelivered(MsgDelivered {
                                to: msg.to.clone(),
                                observed: AgentState::Working,
                            }))
                            .re(&id),
                        )
                        .await;
                        tried.remove(&id);
                        eprintln!("fleet host: delivered {id} to {}", msg.to);
                    }
                    Ok(_) => {
                        eprintln!("fleet host: {id}: prompted, but no `working` seen; will retry")
                    }
                    Err(err) => eprintln!("fleet host: {id}: {err}; will retry"),
                }
            }
        }
    }

    /// Waits up to 20 s for this host to publish the agent in `pane` turning `working` again.
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

/// The text typed into the agent. The `[fleet …]` prefix marks it as machine-sent, never a human turn.
pub fn doorbell(from: &str, to: &str, kind: &str, reference: &str, text: &str, id: &str) -> String {
    format!(
        "[fleet {}→{}] {kind} {reference}: {text} (event {id})",
        short(from),
        short(to)
    )
}

pub async fn run(cfg: Config, client: Client) -> Result<()> {
    let replica = Store::open(&cfg.replica)?;
    eprintln!(
        "fleet host: host={} session={} herdr={} replica={} ({} events)",
        cfg.host,
        cfg.session,
        cfg.herdr_socket.display(),
        cfg.replica.display(),
        replica.len()?
    );
    let host = Arc::new(Host {
        cfg,
        client,
        replica: Mutex::new(replica),
        panes: Mutex::default(),
        wake: Notify::new(),
        seen: Notify::new(),
        watched: Mutex::default(),
    });
    tokio::spawn(host.clone().sync_loop());
    tokio::spawn(host.clone().health_loop());
    tokio::spawn(host.clone().delivery_loop());
    host.presence_loop().await
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "[fleet lead→impl] REVIEW task:p/7: please look (event 01ABC)"
        );
    }
}
