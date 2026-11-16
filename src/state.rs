//! `state = fold(events)`. Pure: no I/O, no clock.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::{
    AgentState, Body, Event, Known, Level, ProcessHung, QuotaWindow, ResourceHold, ResourceLimit,
    SpawnRequested, SpawnResult, StuckCause, TYPE_VERSION, TaskState, UpdateResult, WorktreeProcs,
    WorktreeState,
};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct State {
    /// Id of the last event folded in.
    pub last: Option<String>,
    pub hosts: BTreeMap<String, Host>,
    pub agents: BTreeMap<String, Agent>,
    /// The cwd an agent's own host first reported for its pane (`agent.usage`), by agent id; fixed
    /// until `agent.down`, so a later `cd` does not move it. The root of an agent nobody spawned.
    #[serde(default)]
    pub first_cwd: BTreeMap<String, String>,
    /// Declared roles by agent id; kept apart from `agents`, so a role survives `agent.down`.
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
    /// Usage quotas of the agent programs by `<host>:<kind>`.
    #[serde(default)]
    pub quotas: BTreeMap<String, Quota>,
    pub tasks: BTreeMap<String, Task>,
    /// Open blockers by the id of their `blocker.raised`.
    pub blockers: BTreeMap<String, Blocker>,
    /// Open asks by the id of their `ask.raised`.
    pub asks: BTreeMap<String, Ask>,
    /// Asks answered on the board, by the id of their `ask.raised`; kept for 24 h after the ack.
    #[serde(default)]
    pub answers: BTreeMap<String, Answered>,
    /// Undelivered doorbells by the id of their `msg.sent`.
    pub msgs: BTreeMap<String, Msg>,
    /// Git worktrees by `<host>:<path>`.
    pub worktrees: BTreeMap<String, Worktree>,
    /// Processes running in worktrees (or orphaned beside them) by `<host>:<path>`.
    pub worktree_procs: BTreeMap<String, Procs>,
    /// Held or queued-for resources by `<host>:<resource>`.
    #[serde(default)]
    pub resources: BTreeMap<String, Resource>,
    /// Capacity rules by `<host>:<name>`.
    #[serde(default)]
    pub limits: BTreeMap<String, ResourceLimit>,
    /// The latest kills `agent-fleet kill` refused, oldest first; at most `MAX_REFUSED`.
    #[serde(default)]
    pub refused_kills: Vec<RefusedKill>,
    /// Spawn requests by their `spawn.requested` id; answered ones are kept for 24 h.
    #[serde(default)]
    pub spawns: BTreeMap<String, Spawn>,
    /// Self-update requests by their `update.requested` id, kept for 24 h.
    #[serde(default)]
    pub updates: BTreeMap<String, Update>,
    /// Agent programs outside herdr on each host, keyed `<host>:<pid>`.
    #[serde(default)]
    pub unmanaged: BTreeMap<String, Unmanaged>,
    /// Processes each host proved orphaned, keyed `<host>:<pid>`.
    #[serde(default)]
    pub orphans: BTreeMap<String, Orphan>,
    /// Hung processes by `<host>:<pid>` of the topmost one, `since` always set.
    #[serde(default)]
    pub hung: BTreeMap<String, ProcessHung>,
    /// Why a stopped agent is stopped, as its host read it off the screen, keyed by agent id.
    #[serde(default)]
    pub stuck: BTreeMap<String, Stuck>,
    /// The latest `agent.signal` per agent, keyed by agent id; cleared when a `prompt` or
    /// `turn_end` signal opens a new episode, and when the agent turns `working` or goes down.
    #[serde(default)]
    pub signals: BTreeMap<String, Signal>,
    /// Events this binary cannot fold (an unknown type, or a known type at a newer `v`): kept in
    /// the log, skipped here.
    pub unknown: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Host {
    pub disk_free: u64,
    pub disk_total: u64,
    pub agents: u32,
    /// The commit its binary was built from, as its last `host.health` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub ts: DateTime<Utc>,
    /// When the host process started: that report's time less its `uptime_s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
}

/// An `update.requested` and each host's `update.result`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Update {
    /// Who asked (the request's actor id).
    pub by: String,
    /// The request's `ts`; the update is pruned `UPDATE_KEPT` after it, unless a host still runs
    /// the commit its failed answer reports.
    pub since: DateTime<Utc>,
    /// The one host it asks, or every host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Each host's answer, by host name.
    pub results: BTreeMap<String, UpdateResult>,
}

pub const UPDATE_KEPT: chrono::Duration = chrono::Duration::hours(24);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Agent {
    pub host: String,
    pub kind: Option<String>,
    pub pane: Option<String>,
    pub state: AgentState,
    pub since: DateTime<Utc>,
    /// When it was last seen `working`; a stall is an idle stretch after this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worked: Option<DateTime<Utc>>,
    /// When its state last became `done`; cleared on the next `working`. An agent that
    /// finished its turn and sits idle is parked awaiting work, not stalled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<DateTime<Utc>>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// An agent's declared level and the lead it reports to; `by` and `since` record the
/// `agent.role` event that set it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Role {
    pub level: Level,
    pub lead: Option<String>,
    pub domain: Option<String>,
    /// The actor that declared it.
    pub by: String,
    pub since: DateTime<Utc>,
}

/// A program's usage quota on one host, as its last `agent.quota` reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Quota {
    pub windows: Vec<QuotaWindow>,
    pub plan: Option<String>,
    pub exhausted: bool,
    /// When the numbers were read at the source: how stale they are.
    pub read_at: DateTime<Utc>,
    /// When the report was folded in.
    pub ts: DateTime<Utc>,
    /// When the reports first showed the account out; kept while it stays out, absent
    /// while it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted_since: Option<DateTime<Utc>>,
}

/// An `exhausted` read stays believed this long — claude/codex only refresh while used.
pub const QUOTA_STALE: chrono::Duration = chrono::Duration::minutes(30);

impl Quota {
    /// Out of quota at `now`: a 100% window whose reset is still ahead, or a fresh `exhausted`
    /// read. A stale exhausted flag does not block spawning forever.
    pub fn exhausted_at(&self, now: DateTime<Utc>) -> bool {
        self.windows
            .iter()
            .any(|w| w.used_pct >= 100.0 && w.resets_at.is_some_and(|r| r > now))
            || (self.exhausted && now - self.read_at <= QUOTA_STALE)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    pub rss_kb: Option<u64>,
    pub cpu_pct: Option<u32>,
    pub procs: Option<u32>,
    pub cwd: Option<String>,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Worktree {
    pub state: WorktreeState,
    /// When it last changed.
    pub since: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Procs {
    pub procs: WorktreeProcs,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Resource {
    pub host: String,
    pub resource: String,
    pub holder: Option<Hold>,
    /// Oldest first.
    pub waiting: Vec<Hold>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hold {
    pub pid: u32,
    pub cwd: Option<String>,
    pub scope: Option<String>,
    pub since: DateTime<Utc>,
}

impl From<&ResourceHold> for Hold {
    fn from(d: &ResourceHold) -> Self {
        Self {
            pid: d.pid,
            cwd: d.cwd.clone(),
            scope: d.scope.clone(),
            since: d.since,
        }
    }
}

pub const MAX_REFUSED: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RefusedKill {
    pub by: String,
    pub host: String,
    pub pid: u32,
    pub name: String,
    pub reason: String,
    pub ts: DateTime<Utc>,
}

pub fn worktree_key(host: &str, path: &str) -> String {
    format!("{host}:{path}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    pub project: String,
    pub title: String,
    pub url: Option<String>,
    pub state: TaskState,
    pub since: DateTime<Utc>,
    pub owner: Option<String>,
    pub lead: Option<String>,
    pub blocked_by: BTreeSet<String>,
    /// Ids of `handoff` events, oldest first.
    pub handoffs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Blocker {
    pub task: Option<String>,
    pub what: String,
    pub owner: Option<String>,
    pub unblocks_when: Option<String>,
    pub by: String,
    pub since: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Ask {
    #[serde(default)]
    pub project: String,
    pub task: Option<String>,
    pub by: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub context: Option<String>,
    pub since: DateTime<Utc>,
}

/// An ask the operator answered on the board, and how far the answer got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Answered {
    pub ask: Ask,
    pub answer: String,
    pub approve: bool,
    /// For an approval: the exact question text it applies to.
    pub approves: Option<String>,
    /// The `msg.sent` that carries the answer to the asker.
    pub msg: String,
    pub at: DateTime<Utc>,
    /// When the host saw the asker take the message; `None` while it is only sent.
    pub acked: Option<DateTime<Utc>>,
}

/// How long an acked answer stays in the state, measured against later events' `ts`.
pub const ANSWER_KEPT: chrono::Duration = chrono::Duration::hours(24);

/// One Inbox row: a question from a lead or the root coordinator, open or answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem {
    /// The `ask.raised` id.
    pub id: String,
    pub level: Level,
    pub ask: Ask,
    pub answer: Option<Answered>,
}

/// An agent's current stuck record: the latest `agent.stuck` with a cause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Stuck {
    pub cause: StuckCause,
    /// What the screen showed, or what was done about it.
    pub detail: String,
    /// When the condition lifts, if the cause names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
}

/// An agent's latest program-reported signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Signal {
    pub signal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The program that reported it.
    pub program: String,
    pub since: DateTime<Utc>,
}

/// A process the host's scan proved an orphan: reparented to init, its device gone or its
/// lock's recorded owner dead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Orphan {
    pub host: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub why: String,
    /// First proved.
    pub since: DateTime<Utc>,
}

/// An agent program the host's scan saw outside every herdr pane tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Unmanaged {
    pub host: String,
    pub pid: u32,
    pub program: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// First seen unmanaged.
    pub since: DateTime<Utc>,
}

/// A `spawn.requested` and the `spawn.result` that answered it. `by` and `project` come from the
/// request's envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Spawn {
    pub request: SpawnRequested,
    /// Who asked (the request's actor id).
    pub by: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<SpawnResult>,
    /// The request's `ts`.
    pub since: DateTime<Utc>,
    /// The answering event's `ts`; finished spawns are pruned 24 h after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered: Option<DateTime<Utc>>,
}

/// How long an answered spawn stays in the state, measured against later events' `ts` (the
/// reducer has no clock).
pub const SPAWN_KEPT: chrono::Duration = chrono::Duration::hours(24);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Msg {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub text: String,
    pub since: DateTime<Utc>,
}

impl State {
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut s = Self::default();
        for e in events {
            s.apply(e);
        }
        s
    }

    fn resource_mut(&mut self, host: &str, resource: &str) -> &mut Resource {
        self.resources
            .entry(worktree_key(host, resource))
            .or_insert_with(|| Resource {
                host: host.to_string(),
                resource: resource.to_string(),
                holder: None,
                waiting: Vec::new(),
            })
    }

    /// The agent holding `task`, if the task is assigned and still open.
    pub fn owner_of(&self, task: &str) -> Option<&str> {
        let t = self.tasks.get(task)?;
        if t.state.is_terminal() {
            return None;
        }
        t.owner.as_deref()
    }

    /// The leads above `id`, nearest first, following declared roles; a cycle ends the walk.
    pub fn chain(&self, id: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let mut at = self.roles.get(id).and_then(|r| r.lead.as_deref());
        while let Some(lead) = at {
            if out.contains(&lead) {
                break;
            }
            out.push(lead);
            at = self.roles.get(lead).and_then(|r| r.lead.as_deref());
        }
        out
    }

    /// The agents that report to `id`, by declared role.
    pub fn reports(&self, id: &str) -> Vec<&String> {
        self.roles
            .iter()
            .filter(|(_, r)| r.lead.as_deref() == Some(id))
            .map(|(k, _)| k)
            .collect()
    }

    /// The latest `spawn.requested` for `agent`, if the fleet spawned it.
    pub fn spawn_of(&self, agent: &str) -> Option<&Spawn> {
        self.spawns
            .values()
            .filter(|s| s.request.agent == agent)
            .max_by_key(|s| s.since)
    }

    /// The profile the latest `spawn.requested` for `agent` named, for cap counting.
    pub fn profile_of(&self, agent: &str) -> Option<&str> {
        self.spawn_of(agent).map(|s| s.request.profile.as_str())
    }

    pub fn apply(&mut self, e: &Event) {
        self.last = Some(e.id.clone());
        // A newer version of a known type may mean something else: skip it like an unknown type.
        let (Body::Known(k), TYPE_VERSION) = (&e.body, e.v) else {
            self.unknown += 1;
            return;
        };
        let ts = e.ts;
        match k {
            Known::AgentUp(d) => {
                // A re-up (host or herdr restart) keeps what the entry already knows: the last
                // working time, the current stretch when the state did not change, the usage.
                let old = self.agents.get(&d.agent);
                self.agents.insert(
                    d.agent.clone(),
                    Agent {
                        host: d.host.clone(),
                        kind: d.kind.clone().or_else(|| old.and_then(|a| a.kind.clone())),
                        pane: d.pane.clone(),
                        state: d.state,
                        since: match old {
                            Some(a) if a.state == d.state => a.since,
                            _ => ts,
                        },
                        worked: (d.state == AgentState::Working)
                            .then(|| {
                                self.signals.remove(&d.agent);
                                ts
                            })
                            .or_else(|| old.and_then(|a| a.worked)),
                        finished: if d.state == AgentState::Working {
                            None
                        } else {
                            (d.state == AgentState::Done)
                                .then_some(ts)
                                .or_else(|| old.and_then(|a| a.finished))
                        },
                        usage: old.and_then(|a| a.usage.clone()),
                    },
                );
            }
            Known::AgentUsage(d) => {
                if let Some(a) = self.agents.get_mut(&d.agent) {
                    // Only the agent's own host fixes where it was first seen.
                    if let Some(cwd) = d.cwd.clone().filter(|_| {
                        e.actor.kind == crate::event::ActorKind::Host
                            && e.actor.id == format!("host:{}", a.host)
                    }) {
                        self.first_cwd.entry(d.agent.clone()).or_insert(cwd);
                    }
                    a.usage = Some(Usage {
                        rss_kb: d.rss_kb,
                        cpu_pct: d.cpu_pct,
                        procs: d.procs,
                        cwd: d.cwd.clone(),
                        ts,
                    });
                }
            }
            Known::WorktreeState(d) => {
                self.worktrees.insert(
                    worktree_key(&d.host, &d.path),
                    Worktree {
                        state: d.clone(),
                        since: ts,
                    },
                );
            }
            Known::WorktreeProcs(d) => {
                let key = worktree_key(&d.host, &d.path);
                if d.procs == 0 {
                    self.worktree_procs.remove(&key);
                } else {
                    self.worktree_procs.insert(
                        key,
                        Procs {
                            procs: d.clone(),
                            ts,
                        },
                    );
                }
            }
            Known::ResourceTaken(d) => {
                let r = self.resource_mut(&d.host, &d.resource);
                r.waiting.retain(|w| w.pid != d.pid);
                r.holder = Some(d.into());
            }
            Known::ResourceWaiting(d) => {
                let r = self.resource_mut(&d.host, &d.resource);
                r.waiting.retain(|w| w.pid != d.pid);
                r.waiting.push(d.into());
                r.waiting.sort_by_key(|w| w.since);
            }
            Known::ResourceReleased(d) => {
                let key = worktree_key(&d.host, &d.resource);
                if let (Some(r), Some(pid)) = (self.resources.get_mut(&key), d.pid) {
                    r.waiting.retain(|w| w.pid != pid);
                    if r.holder.as_ref().is_some_and(|h| h.pid == pid) {
                        r.holder = None;
                    }
                    if r.holder.is_some() || !r.waiting.is_empty() {
                        return;
                    }
                }
                self.resources.remove(&key);
            }
            Known::ResourceLimit(d) => {
                let key = worktree_key(&d.host, &d.name);
                if d.resources.is_empty() {
                    self.limits.remove(&key);
                } else {
                    self.limits.insert(key, d.clone());
                }
            }
            Known::KillRefused(d) => {
                self.refused_kills.push(RefusedKill {
                    by: e.actor.id.clone(),
                    host: d.host.clone(),
                    pid: d.pid,
                    name: d.name.clone(),
                    reason: d.reason.clone(),
                    ts,
                });
                let over = self.refused_kills.len().saturating_sub(MAX_REFUSED);
                self.refused_kills.drain(..over);
            }
            Known::WorktreeGone(d) => {
                self.worktrees.remove(&worktree_key(&d.host, &d.path));
            }
            Known::AgentState(d) => {
                // Only `agent.up` brings an agent in: a late state change never revives a downed one.
                if let Some(a) = self.agents.get_mut(&d.agent) {
                    if d.state == AgentState::Working {
                        a.worked = Some(ts);
                        a.finished = None;
                        self.signals.remove(&d.agent);
                    }
                    if d.state == AgentState::Done {
                        a.finished = Some(ts);
                    }
                    if a.state != d.state {
                        a.state = d.state;
                        a.since = ts;
                    }
                }
            }
            Known::AgentDown(d) => {
                self.agents.remove(&d.agent);
                self.first_cwd.remove(&d.agent);
                self.stuck.remove(&d.agent);
                self.signals.remove(&d.agent);
            }
            Known::AgentSignal(d) => {
                // `prompt`/`turn_end` open a new episode: the stored signal is cleared, and
                // a stale event that arrives out of order never rewinds a newer one.
                let stale = self.signals.get(&d.agent).is_some_and(|s| s.since > ts);
                if matches!(d.signal.as_str(), "prompt" | "turn_end") {
                    if !stale {
                        self.signals.remove(&d.agent);
                    }
                } else if !stale {
                    self.signals.insert(
                        d.agent.clone(),
                        Signal {
                            signal: d.signal.clone(),
                            detail: d.detail.clone(),
                            program: d.program.clone(),
                            since: ts,
                        },
                    );
                }
            }
            Known::AgentStuck(d) => match &d.cause {
                Some(cause) => {
                    self.stuck.insert(
                        d.agent.clone(),
                        Stuck {
                            cause: *cause,
                            detail: d.detail.clone(),
                            until: d.until,
                            since: ts,
                        },
                    );
                }
                None => {
                    self.stuck.remove(&d.agent);
                }
            },
            Known::AgentQuota(d) => {
                let key = worktree_key(&d.host, &d.kind);
                let out = d.exhausted || d.windows.iter().any(|w| w.used_pct >= 100.0);
                self.quotas.insert(
                    key.clone(),
                    Quota {
                        windows: d.windows.clone(),
                        plan: d.plan.clone(),
                        exhausted: d.exhausted,
                        read_at: d.read_at,
                        ts,
                        exhausted_since: out.then(|| {
                            self.quotas
                                .get(&key)
                                .and_then(|q| q.exhausted_since)
                                .unwrap_or(ts)
                        }),
                    },
                );
            }
            Known::AgentRole(d) => {
                if let Some(level) = d.level {
                    self.roles.insert(
                        d.agent.clone(),
                        Role {
                            level,
                            lead: d.lead.clone(),
                            domain: d.domain.clone(),
                            by: e.actor.id.clone(),
                            since: ts,
                        },
                    );
                } else {
                    self.roles.remove(&d.agent);
                }
            }
            Known::HostHealth(d) => {
                self.hosts.insert(
                    d.host.clone(),
                    Host {
                        disk_free: d.disk_free,
                        disk_total: d.disk_total,
                        agents: d.agents,
                        commit: d.commit.clone(),
                        ts,
                        started: d
                            .stats
                            .as_ref()
                            .map(|s| ts - chrono::Duration::seconds(s.uptime_s as i64)),
                    },
                );
            }
            Known::TaskCreated(d) => {
                // A task still titled with its id takes the title from the next `task.created`;
                // once it has another title, a racing duplicate does not rename it.
                if let Some(id) = &e.task {
                    let t = self.task_mut(id, &e.project, ts);
                    if t.title == *id {
                        t.title = d.title.clone();
                    }
                    t.url = t.url.take().or(d.url.clone());
                }
            }
            Known::TaskAssigned(d) => {
                if let Some(t) = self.task_for(e) {
                    t.owner = Some(d.agent.clone());
                    if d.lead.is_some() {
                        t.lead = d.lead.clone();
                    }
                    // Claiming a new or finished task (re)opens it, so the next claim is a 409.
                    if t.state == TaskState::Submitted || t.state.is_terminal() {
                        t.state = TaskState::Working;
                        t.since = ts;
                    }
                }
            }
            Known::TaskUnassigned(_) => {
                if let Some(t) = self.task_for(e) {
                    t.owner = None;
                }
            }
            Known::TaskState(d) => {
                if let Some(t) = self.task_for(e)
                    && t.state != d.state
                {
                    // Reopening a finished task does not hand it back to whoever finished it.
                    if t.state.is_terminal() && !d.state.is_terminal() {
                        t.owner = None;
                    }
                    t.state = d.state;
                    t.since = ts;
                }
            }
            Known::DepAdded(d) => {
                if let Some(t) = self.task_for(e) {
                    t.blocked_by.insert(d.on.clone());
                }
            }
            Known::DepRemoved(d) => {
                if let Some(t) = self.task_for(e) {
                    t.blocked_by.remove(&d.on);
                }
            }
            Known::BlockerRaised(d) => {
                self.blockers.insert(
                    e.id.clone(),
                    Blocker {
                        task: e.task.clone(),
                        what: d.what.clone(),
                        owner: d.owner.clone(),
                        unblocks_when: d.unblocks_when.clone(),
                        by: e.actor.id.clone(),
                        since: ts,
                    },
                );
            }
            Known::BlockerResolved(_) => {
                if let Some(re) = &e.re {
                    self.blockers.remove(re);
                }
            }
            Known::AskRaised(d) => {
                self.asks.insert(
                    e.id.clone(),
                    Ask {
                        project: e.project.clone(),
                        task: e.task.clone(),
                        by: e.actor.id.clone(),
                        question: d.question.clone(),
                        options: d.options.clone(),
                        recommendation: d.recommendation.clone(),
                        context: d.context.clone(),
                        since: ts,
                    },
                );
            }
            Known::AskAnswered(d) => {
                if let Some(re) = &e.re
                    && let Some(ask) = self.asks.remove(re)
                {
                    self.answers.insert(
                        re.clone(),
                        Answered {
                            ask,
                            answer: d.answer.clone(),
                            approve: d.approve,
                            approves: d.approves.clone(),
                            msg: d.msg.clone(),
                            at: ts,
                            acked: None,
                        },
                    );
                }
            }
            Known::MsgSent(d) => {
                self.msgs.insert(
                    e.id.clone(),
                    Msg {
                        from: e.actor.id.clone(),
                        to: d.to.clone(),
                        kind: d.kind.clone(),
                        reference: d.reference.clone(),
                        text: d.text.clone(),
                        since: ts,
                    },
                );
            }
            Known::MsgDelivered(_) => {
                if let Some(re) = &e.re {
                    self.msgs.remove(re);
                    for a in self.answers.values_mut().filter(|a| &a.msg == re) {
                        a.acked.get_or_insert(ts);
                    }
                }
            }
            Known::Handoff(_) => {
                let id = e.id.clone();
                if let Some(t) = self.task_for(e) {
                    t.handoffs.push(id);
                }
            }
            Known::SpawnRequested(d) => {
                self.spawns.insert(
                    e.id.clone(),
                    Spawn {
                        request: *d.clone(),
                        by: e.actor.id.clone(),
                        project: e.project.clone(),
                        result: None,
                        since: ts,
                        answered: None,
                    },
                );
            }
            Known::AgentUnmanaged(d) => {
                let key = format!("{}:{}", d.host, d.pid);
                match &d.program {
                    Some(program) => {
                        self.unmanaged.insert(
                            key,
                            Unmanaged {
                                host: d.host.clone(),
                                pid: d.pid,
                                program: program.clone(),
                                cwd: d.cwd.clone(),
                                since: d.since.unwrap_or(ts),
                            },
                        );
                    }
                    None => {
                        self.unmanaged.remove(&key);
                    }
                }
            }
            Known::ProcessOrphaned(d) => {
                let key = format!("{}:{}", d.host, d.pid);
                match &d.why {
                    Some(why) => {
                        self.orphans.insert(
                            key,
                            Orphan {
                                host: d.host.clone(),
                                pid: d.pid,
                                cwd: d.cwd.clone(),
                                why: why.clone(),
                                since: d.since.unwrap_or(ts),
                            },
                        );
                    }
                    None => {
                        self.orphans.remove(&key);
                    }
                }
            }
            // Kill proof: only the host it names may report or clear it.
            Known::ProcessHung(d)
                if e.actor.kind != crate::event::ActorKind::Host
                    || e.actor.id != format!("host:{}", d.host) => {}
            Known::ProcessHung(d) => {
                let key = format!("{}:{}", d.host, d.pid);
                if d.evidence.is_some() {
                    // A reused pid's report starts its own episode.
                    let since = self
                        .hung
                        .get(&key)
                        .filter(|h| h.started == d.started)
                        .and_then(|h| h.since);
                    self.hung.insert(
                        key,
                        ProcessHung {
                            since: since.or(d.since).or(Some(ts)),
                            ..d.clone()
                        },
                    );
                } else {
                    self.hung.remove(&key);
                }
            }
            Known::SpawnResult(d) => {
                if let Some(re) = &e.re
                    && let Some(s) = self.spawns.get_mut(re)
                    && s.result.is_none()
                {
                    s.result = Some(d.clone());
                    s.answered = Some(ts);
                }
            }
            Known::UpdateRequested(d) => {
                self.updates.insert(
                    e.id.clone(),
                    Update {
                        by: e.actor.id.clone(),
                        since: ts,
                        host: d.host.clone(),
                        results: BTreeMap::new(),
                    },
                );
            }
            // Only the host it names may answer for it, and only once.
            Known::UpdateResult(d) => {
                if e.actor.id == format!("host:{}", d.host)
                    && let Some(re) = &e.re
                    && let Some(u) = self.updates.get_mut(re)
                {
                    u.results.entry(d.host.clone()).or_insert_with(|| d.clone());
                }
            }
        }
        // A failure is kept while its host still runs the commit it failed on: nothing will ask
        // that host again until `origin/main` moves, and `doctor` reports it until then. That is
        // the newest update the host answered, and only if that answer failed: a later success
        // (even "already current") clears it, as in `doctor`.
        let mut newest: BTreeMap<&str, (DateTime<Utc>, &str, &UpdateResult)> = BTreeMap::new();
        for (id, u) in &self.updates {
            for (h, r) in &u.results {
                if newest.get(h.as_str()).is_none_or(|(t, _, _)| u.since > *t) {
                    newest.insert(h, (u.since, id, r));
                }
            }
        }
        let keep: HashSet<String> = newest
            .iter()
            .filter(|(h, (_, _, r))| {
                !r.ok
                    && r.commit.is_some()
                    && self.hosts.get(**h).is_some_and(|x| x.commit == r.commit)
            })
            .map(|(_, (_, id, _))| id.to_string())
            .collect();
        self.updates
            .retain(|id, u| e.ts - u.since <= UPDATE_KEPT || keep.contains(id));
        self.answers
            .retain(|_, a| e.ts - a.acked.unwrap_or(a.at) <= ANSWER_KEPT);
        // The reducer has no clock: a spawn is pruned once a later event's `ts` is more than
        // SPAWN_KEPT past its answer (or its request, when it was never answered) — but never
        // while its agent is still up, so cap counting cannot lose sight of it.
        let agents = &self.agents;
        self.spawns.retain(|_, s| {
            agents.contains_key(&s.request.agent)
                || e.ts - s.answered.unwrap_or(s.since) <= SPAWN_KEPT
        });
    }

    /// The worktree the fleet assigned each agent at spawn on `host`, by agent id: the host's
    /// `spawn.result`, never where the agent's pane stands now (`usage.cwd` follows a `cd`).
    /// Exact rule: among the spawns of an agent that is up (in `agents`) on `host`, drop any the
    /// agent requested for itself, and any not answered `ok` with a worktree; the one requested
    /// LAST (`since`) gives the root. An agent re-spawned into a new worktree so moves with it.
    /// An up agent no one else spawned here (started by hand) has its `first_cwd`; one whose spawn
    /// failed or is unanswered, or with neither, has `None`.
    ///
    /// Threat model: `kill` guards against honest-but-mistaken agents, not deliberate
    /// impersonation. The hub's one shared bearer token already lets any holder post as any
    /// actor id, a fleet-wide limit out of scope here.
    pub fn assigned_worktrees(&self, host: &str) -> Vec<(String, Option<String>)> {
        let mut out: BTreeMap<&str, (DateTime<Utc>, Option<String>)> = BTreeMap::new();
        for s in self.spawns.values() {
            let agent = s.request.agent.as_str();
            if s.request.host != host || s.by == agent || !self.agents.contains_key(agent) {
                continue;
            }
            let Some(tree) = s
                .result
                .as_ref()
                .filter(|r| r.ok)
                .and_then(|r| r.worktree.clone())
            else {
                out.entry(agent).or_insert((s.since, None));
                continue;
            };
            let slot = out.entry(agent).or_insert((s.since, None));
            if slot.1.is_none() || s.since >= slot.0 {
                *slot = (s.since, Some(tree));
            }
        }
        for (id, a) in &self.agents {
            if let (true, Some(cwd)) = (a.host == host, self.first_cwd.get(id)) {
                out.entry(id).or_insert((a.since, Some(cwd.clone())));
            }
        }
        out.into_iter()
            .map(|(a, (_, t))| (a.to_string(), t))
            .collect()
    }

    /// The questions the board's Inbox shows: asks from a domain lead, implementation lead or root
    /// coordinator, open first, then the answered ones. Implementers, and agents with no declared
    /// role, go through their lead.
    pub fn inbox(&self) -> Vec<InboxItem> {
        let level = |by: &str| {
            self.roles
                .get(by)
                .map(|r| r.level)
                .filter(|l| *l >= Level::ImplementationLead)
        };
        let open = self.asks.iter().filter_map(|(id, ask)| {
            Some(InboxItem {
                id: id.clone(),
                level: level(&ask.by)?,
                ask: ask.clone(),
                answer: None,
            })
        });
        let answered = self.answers.iter().filter_map(|(id, a)| {
            Some(InboxItem {
                id: id.clone(),
                level: level(&a.ask.by)?,
                ask: a.ask.clone(),
                answer: Some(a.clone()),
            })
        });
        open.chain(answered).collect()
    }

    fn task_for(&mut self, e: &Event) -> Option<&mut Task> {
        let id = e.task.as_deref()?;
        Some(self.task_mut(id, &e.project, e.ts))
    }

    /// A task first seen through an event other than `task.created` is created on the spot.
    fn task_mut(&mut self, id: &str, project: &str, ts: DateTime<Utc>) -> &mut Task {
        self.tasks.entry(id.to_string()).or_insert_with(|| Task {
            project: project.to_string(),
            title: id.to_string(),
            url: None,
            state: TaskState::Submitted,
            since: ts,
            owner: None,
            lead: None,
            blocked_by: BTreeSet::new(),
            handoffs: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;

    fn ev(kind: ActorKind, body: Known) -> Envelope<Known> {
        let id = match kind {
            ActorKind::Host => "host:h",
            _ => "agent:h/s/a",
        };
        Envelope::new(
            Actor {
                kind,
                id: id.into(),
            },
            "p",
            body,
        )
    }

    fn agent_ev(body: Known) -> Envelope<Known> {
        ev(ActorKind::Agent, body).task("task:p/1")
    }

    #[test]
    fn an_agents_assigned_worktree_is_its_spawn_result_and_never_where_its_pane_stands() {
        let req: SpawnRequested = serde_json::from_value(serde_json::json!({
            "name": "a", "agent": "agent:h/s/a", "host": "h", "session": "s", "repo": "r",
            "branch": "b", "profile": "p", "brief": "x", "why": "t"
        }))
        .unwrap();
        let res = |ok, wt: Option<&str>| SpawnResult {
            agent: "agent:h/s/a".into(),
            ok,
            pane: None,
            worktree: wt.map(str::to_string),
            error: None,
        };
        let mut s = State::default();
        let now = Utc::now();
        s.agents.insert(
            "agent:h/s/a".into(),
            Agent {
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Idle,
                since: now,
                worked: None,
                finished: None,
                usage: Some(Usage {
                    rss_kb: None,
                    cpu_pct: None,
                    procs: None,
                    cwd: Some("/w/other".into()),
                    ts: now,
                }),
            },
        );
        s.spawns.insert(
            "e1".into(),
            Spawn {
                request: req,
                by: "x".into(),
                project: "p".into(),
                result: Some(res(true, Some("/w/mine"))),
                since: now,
                answered: Some(now),
            },
        );
        let want = vec![("agent:h/s/a".to_string(), Some("/w/mine".to_string()))];
        assert_eq!(
            s.assigned_worktrees("h"),
            want,
            "pane cwd /w/other is ignored"
        );
        assert!(s.assigned_worktrees("elsewhere").is_empty());
        // a failed or unanswered spawn assigns nothing
        s.spawns.get_mut("e1").unwrap().result = Some(res(false, Some("/w/mine")));
        assert_eq!(s.assigned_worktrees("h")[0].1, None);
        s.spawns.get_mut("e1").unwrap().result = None;
        assert_eq!(s.assigned_worktrees("h")[0].1, None);
        // a spawn the agent wrote for itself assigns nothing: no record, so fail closed
        s.spawns.get_mut("e1").unwrap().by = "agent:h/s/a".into();
        s.spawns.get_mut("e1").unwrap().result = Some(res(true, Some("/w/foreign")));
        assert!(s.assigned_worktrees("h").is_empty(), "self-spawn ignored");
        // a coordinator spawn stands; the agent's later self-spawn does not displace it
        let mut w1 = s.spawns["e1"].clone();
        w1.by = "agent:h/s/coord".into();
        w1.since = now - chrono::Duration::minutes(10);
        w1.result = Some(res(true, Some("/w/mine")));
        s.spawns.insert("e0".into(), w1.clone());
        assert_eq!(s.assigned_worktrees("h"), want);
        // re-spawned into a new worktree: the most recent successful spawn wins
        let mut w2 = w1.clone();
        w2.since = now - chrono::Duration::minutes(5);
        w2.result = Some(res(true, Some("/w/second")));
        s.spawns.insert("e00".into(), w2.clone());
        let second = vec![("agent:h/s/a".to_string(), Some("/w/second".to_string()))];
        assert_eq!(
            s.assigned_worktrees("h"),
            second,
            "W1 then W2 gives W2, whatever the key order"
        );
        // a later failed spawn does not displace it
        let mut w3 = w2;
        w3.since = now - chrono::Duration::minutes(1);
        w3.result = Some(res(false, None));
        s.spawns.insert("e000".into(), w3);
        assert_eq!(s.assigned_worktrees("h"), second);
        // an agent that is down has no root
        let gone = s.agents.remove("agent:h/s/a").unwrap();
        assert!(s.assigned_worktrees("h").is_empty());
        s.agents.insert("agent:h/s/a".into(), gone);
        // an agent nobody spawned is absent: its root fails closed
        s.spawns.clear();
        assert!(s.assigned_worktrees("h").is_empty());
    }

    #[test]
    fn a_hand_started_agents_root_is_where_its_host_first_saw_it() {
        let a = "agent:h/s/a".to_string();
        let use_at = |kind, cwd: &str| {
            ev(
                kind,
                Known::AgentUsage(AgentUsage {
                    agent: a.clone(),
                    rss_kb: None,
                    cpu_pct: None,
                    procs: None,
                    cwd: Some(cwd.into()),
                }),
            )
        };
        let mut s = State::default();
        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentUp(AgentUp {
                    agent: a.clone(),
                    host: "h".into(),
                    kind: Some("claude".into()),
                    state: AgentState::Idle,
                    pane: None,
                }),
            )
            .into_event(),
        );
        assert!(s.assigned_worktrees("h").is_empty(), "no record yet");
        s.apply(&use_at(ActorKind::Agent, "/w/foreign").into_event());
        let mut other_host = use_at(ActorKind::Host, "/w/x");
        other_host.actor.id = "host:x".into();
        s.apply(&other_host.into_event());
        assert!(s.first_cwd.is_empty(), "only the agent's own host sets it");
        s.apply(&use_at(ActorKind::Host, "/w/mine/web").into_event());
        s.apply(&use_at(ActorKind::Host, "/w/other").into_event());
        let mine = vec![(a.clone(), Some("/w/mine/web".to_string()))];
        assert_eq!(
            s.assigned_worktrees("h"),
            mine,
            "a later cd does not move it"
        );
        assert!(s.assigned_worktrees("x").is_empty());
        // a spawned agent keeps its spawn result, and a failed spawn still fails closed
        let req: SpawnRequested = serde_json::from_value(serde_json::json!({
            "name": "a", "agent": a, "host": "h", "session": "s", "repo": "r",
            "branch": "b", "profile": "p", "brief": "x", "why": "t"
        }))
        .unwrap();
        let spawn = |ok| Spawn {
            request: req.clone(),
            by: "agent:h/s/coord".into(),
            project: "p".into(),
            result: Some(SpawnResult {
                agent: a.clone(),
                ok,
                pane: None,
                worktree: Some("/w/spawned".into()),
                error: None,
            }),
            since: Utc::now(),
            answered: None,
        };
        s.spawns.insert("e1".into(), spawn(true));
        assert_eq!(
            s.assigned_worktrees("h")[0].1.as_deref(),
            Some("/w/spawned")
        );
        s.spawns.insert("e1".into(), spawn(false));
        assert_eq!(s.assigned_worktrees("h")[0].1, None);
        // the agent's own spawn request does not count: it falls back to where it was seen
        s.spawns.get_mut("e1").unwrap().by = a.clone();
        assert_eq!(s.assigned_worktrees("h"), mine);
        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentDown(AgentDown {
                    agent: a.clone(),
                    reason: None,
                }),
            )
            .into_event(),
        );
        assert!(s.first_cwd.is_empty(), "a new lifetime starts afresh");
    }

    #[test]
    fn a_signal_folds_latest_and_episode_marks_clear_it() {
        let sig = |signal: &str, detail: Option<&str>| {
            agent_ev(Known::AgentSignal(AgentSignal {
                agent: "agent:h/s/a".into(),
                program: "claude".into(),
                signal: signal.into(),
                detail: detail.map(str::to_string),
            }))
            .into_event()
        };
        let mut s = State::default();
        s.apply(&sig("permission", None));
        assert_eq!(s.signals["agent:h/s/a"].signal, "permission");
        // a newer signal replaces; an older one arriving late does not rewind
        s.apply(&sig("question", Some("which?")));
        assert_eq!(s.signals["agent:h/s/a"].signal, "question");
        assert_eq!(s.signals["agent:h/s/a"].detail.as_deref(), Some("which?"));
        let mut old = sig("error", Some("late"));
        old.ts = s.signals["agent:h/s/a"].since - chrono::Duration::minutes(1);
        s.apply(&old);
        assert_eq!(s.signals["agent:h/s/a"].signal, "question");
        // prompt and turn_end open a new episode: the stored signal clears
        s.apply(&sig("turn_end", None));
        assert!(s.signals.is_empty());
        s.apply(&sig("rate_limit", None));
        s.apply(&sig("prompt", None));
        assert!(s.signals.is_empty());
        // and turning working or going down clears it too
        let up = |state| {
            ev(
                ActorKind::Host,
                Known::AgentState(AgentStateChanged {
                    agent: "agent:h/s/a".into(),
                    state,
                }),
            )
            .into_event()
        };
        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentUp(AgentUp {
                    agent: "agent:h/s/a".into(),
                    host: "h".into(),
                    kind: Some("claude".into()),
                    state: AgentState::Idle,
                    pane: None,
                }),
            )
            .into_event(),
        );
        s.apply(&sig("error", Some("e")));
        s.apply(&up(AgentState::Working));
        assert!(s.signals.is_empty());
        s.apply(&sig("error", Some("e")));
        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentDown(AgentDown {
                    agent: "agent:h/s/a".into(),
                    reason: None,
                }),
            )
            .into_event(),
        );
        assert!(s.signals.is_empty());
    }

    #[test]
    fn exhausted_since_is_the_first_out_report_while_it_stays_out() {
        let quota = |exhausted: bool, pct: f64, ts: DateTime<Utc>| {
            let mut e = ev(
                ActorKind::Host,
                Known::AgentQuota(AgentQuota {
                    host: "h".into(),
                    kind: "devin".into(),
                    windows: vec![QuotaWindow {
                        name: "day".into(),
                        used_pct: pct,
                        resets_at: None,
                    }],
                    plan: None,
                    exhausted,
                    read_at: ts,
                }),
            )
            .into_event();
            e.ts = ts;
            e
        };
        let t0 = Utc::now();
        let mut s = State::default();
        s.apply(&quota(false, 50.0, t0));
        assert_eq!(s.quotas["h:devin"].exhausted_since, None);
        // The first report showing the account out stamps it; later out reports keep it.
        s.apply(&quota(true, 50.0, t0 + chrono::Duration::minutes(1)));
        let t_out = t0 + chrono::Duration::minutes(1);
        assert_eq!(
            s.quotas["h:devin"].exhausted_since,
            Some(t_out),
            "the event that set exhausted"
        );
        s.apply(&quota(true, 50.0, t0 + chrono::Duration::minutes(5)));
        assert_eq!(s.quotas["h:devin"].exhausted_since, Some(t_out));
        // A 100% window counts too; a non-out report clears it, a new outage re-stamps.
        s.apply(&quota(false, 100.0, t0 + chrono::Duration::minutes(6)));
        assert_eq!(s.quotas["h:devin"].exhausted_since, Some(t_out));
        s.apply(&quota(false, 50.0, t0 + chrono::Duration::minutes(7)));
        assert_eq!(s.quotas["h:devin"].exhausted_since, None);
        s.apply(&quota(true, 50.0, t0 + chrono::Duration::minutes(8)));
        assert_eq!(
            s.quotas["h:devin"].exhausted_since,
            Some(t0 + chrono::Duration::minutes(8))
        );
    }

    #[test]
    fn a_re_up_keeps_worked_and_an_unchanged_since() {
        let mut s = State::default();
        let up = |state, ts| {
            let mut e = ev(
                ActorKind::Host,
                Known::AgentUp(AgentUp {
                    agent: "agent:h/s/a".into(),
                    host: "h".into(),
                    kind: Some("devin".into()),
                    state,
                    pane: None,
                }),
            )
            .into_event();
            e.ts = ts;
            e
        };
        let t0 = Utc::now();
        s.apply(&up(AgentState::Working, t0));
        // a re-up while working: since stays (same state), worked moves to the newer sighting
        s.apply(&up(AgentState::Working, t0 + chrono::Duration::minutes(1)));
        let a = &s.agents["agent:h/s/a"];
        assert_eq!(a.since, t0);
        assert_eq!(a.worked, Some(t0 + chrono::Duration::minutes(1)));
        // a re-up now idle: worked survives, since moves (state changed)
        s.apply(&up(AgentState::Idle, t0 + chrono::Duration::minutes(30)));
        let a = &s.agents["agent:h/s/a"];
        assert_eq!(a.worked, Some(t0 + chrono::Duration::minutes(1)));
        assert_eq!(a.since, t0 + chrono::Duration::minutes(30));
    }

    #[test]
    fn an_unmanaged_report_folds_and_clears() {
        let mut s = State::default();
        let seen = ev(
            ActorKind::Host,
            Known::AgentUnmanaged(AgentUnmanaged {
                host: "h".into(),
                pid: 42,
                program: Some("claude".into()),
                cwd: Some("/tmp".into()),
                since: None,
            }),
        )
        .into_event();
        s.apply(&seen);
        let u = &s.unmanaged["h:42"];
        assert_eq!(u.program, "claude");
        assert_eq!(u.since, seen.ts);
        let clear = ev(
            ActorKind::Host,
            Known::AgentUnmanaged(AgentUnmanaged {
                host: "h".into(),
                pid: 42,
                program: None,
                cwd: None,
                since: None,
            }),
        )
        .into_event();
        s.apply(&clear);
        assert!(s.unmanaged.is_empty());
    }

    #[test]
    fn an_orphan_proof_folds_and_clears() {
        let mut s = State::default();
        let seen = ev(
            ActorKind::Host,
            Known::ProcessOrphaned(ProcessOrphaned {
                host: "h".into(),
                pid: 27634,
                cwd: Some("/w/shard".into()),
                why: Some("sim lock owner dead".into()),
                since: None,
            }),
        )
        .into_event();
        s.apply(&seen);
        let o = &s.orphans["h:27634"];
        assert_eq!(o.why, "sim lock owner dead");
        assert_eq!(o.since, seen.ts);
        let clear = ev(
            ActorKind::Host,
            Known::ProcessOrphaned(ProcessOrphaned {
                host: "h".into(),
                pid: 27634,
                cwd: None,
                why: None,
                since: None,
            }),
        )
        .into_event();
        s.apply(&clear);
        assert!(s.orphans.is_empty());
    }

    #[test]
    fn a_hung_report_folds_keeps_its_first_since_and_clears() {
        let mut s = State::default();
        let hung = |evidence: Option<&str>| {
            ev(
                ActorKind::Host,
                Known::ProcessHung(ProcessHung {
                    host: "h".into(),
                    pid: 30,
                    pids: vec![30, 31],
                    agent: Some("agent:h/s/o-1".into()),
                    command: Some("bash scripts/e2e.sh".into()),
                    started: None,
                    evidence: evidence.map(str::to_string),
                    since: None,
                    checked: None,
                }),
            )
            .into_event()
        };
        let first = hung(Some("0% CPU"));
        s.apply(&first);
        s.apply(&hung(Some("0% CPU, 6m")));
        let h = &s.hung["h:30"];
        assert_eq!(h.since, Some(first.ts), "the first report dates it");
        assert_eq!(h.evidence.as_deref(), Some("0% CPU, 6m"));
        assert_eq!(h.pids, vec![30, 31]);
        s.apply(&hung(None));
        assert!(s.hung.is_empty());
    }

    #[test]
    fn a_hung_report_counts_only_from_the_host_it_names() {
        let body = |host: &str| {
            Known::ProcessHung(ProcessHung {
                host: host.into(),
                pid: 30,
                pids: vec![30],
                agent: None,
                command: None,
                started: None,
                evidence: Some("0% CPU".into()),
                since: None,
                checked: None,
            })
        };
        let mut s = State::default();
        s.apply(&ev(ActorKind::Agent, body("h")).into_event());
        assert!(s.hung.is_empty(), "an agent cannot report a process hung");
        s.apply(&ev(ActorKind::Host, body("other")).into_event());
        assert!(s.hung.is_empty(), "host:h cannot report for another host");
        assert!(crate::doctor::hung_pids(&s, "h", Utc::now()).is_empty());
        s.apply(&ev(ActorKind::Host, body("h")).into_event());
        assert_eq!(s.hung.len(), 1);
        let mut clear = ev(ActorKind::Agent, body("h"));
        if let Known::ProcessHung(d) = &mut clear.body {
            d.evidence = None;
        }
        s.apply(&clear.into_event());
        assert_eq!(s.hung.len(), 1, "nor clear one");
    }

    #[test]
    fn presence() {
        let a = "agent:h/s/a".to_string();
        let up = ev(
            ActorKind::Host,
            Known::AgentUp(AgentUp {
                agent: a.clone(),
                host: "h".into(),
                kind: Some("claude".into()),
                state: AgentState::Idle,
                pane: None,
            }),
        );
        let busy = ev(
            ActorKind::Host,
            Known::AgentState(AgentStateChanged {
                agent: a.clone(),
                state: AgentState::Working,
            }),
        );
        let events = [up.into_event(), busy.into_event()];
        let s = State::fold(&events);
        assert_eq!(s.agents[&a].state, AgentState::Working);
        assert_eq!(s.agents[&a].since, events[1].ts);

        let down = ev(
            ActorKind::Host,
            Known::AgentDown(AgentDown {
                agent: a.clone(),
                reason: None,
            }),
        );
        let s = State::fold(events.iter().chain([&down.into_event()]));
        assert!(s.agents.is_empty());
    }

    #[test]
    fn a_role_is_declared_overwritten_and_removed() {
        let a = "agent:h/s/a".to_string();
        let role = |level: Option<Level>, by| {
            ev(
                by,
                Known::AgentRole(AgentRole {
                    agent: a.clone(),
                    level,
                    lead: Some("agent:h/s/lead".into()),
                    domain: Some("payments".into()),
                }),
            )
            .into_event()
        };
        let mut s = State::default();
        s.apply(&role(Some(Level::DomainLead), ActorKind::Host));
        let r = &s.roles[&a];
        assert_eq!(r.level, Level::DomainLead);
        assert_eq!(r.lead.as_deref(), Some("agent:h/s/lead"));
        assert_eq!(r.by, "host:h");

        s.apply(&role(Some(Level::Implementer), ActorKind::Agent));
        assert_eq!(s.roles[&a].level, Level::Implementer);
        assert_eq!(s.roles[&a].by, "agent:h/s/a");

        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentDown(AgentDown {
                    agent: a.clone(),
                    reason: None,
                }),
            )
            .into_event(),
        );
        assert!(s.roles.contains_key(&a), "a role survives agent.down");

        s.apply(&role(None, ActorKind::Agent));
        assert!(s.roles.is_empty());
    }

    #[test]
    fn the_chain_walks_leads_and_stops_at_a_cycle() {
        let a = |name| format!("agent:h/s/{name}");
        let role = |level, lead: Option<&str>| AgentRole {
            agent: a("x"),
            level,
            lead: lead.map(str::to_string),
            domain: None,
        };
        let set = |id: &str, level, lead: Option<&str>| {
            let mut d = role(Some(level), lead);
            d.agent = id.to_string();
            ev(ActorKind::Agent, Known::AgentRole(d)).into_event()
        };
        let events: Vec<Event> = [
            set(&a("impl"), Level::Implementer, Some(&a("lead"))),
            set(&a("lead"), Level::DomainLead, Some(&a("root"))),
            set(&a("root"), Level::RootCoordinator, None),
            set(&a("loop-a"), Level::Implementer, Some(&a("loop-b"))),
            set(&a("loop-b"), Level::Implementer, Some(&a("loop-a"))),
        ]
        .into_iter()
        .collect();
        let s = State::fold(&events);
        assert_eq!(s.chain(&a("impl")), [a("lead").as_str(), "agent:h/s/root"]);
        assert_eq!(
            s.chain(&a("loop-a")),
            ["agent:h/s/loop-b", "agent:h/s/loop-a"]
        );
        let reports: Vec<&str> = s.reports(&a("lead")).iter().map(|s| s.as_str()).collect();
        assert_eq!(reports, ["agent:h/s/impl"]);
    }

    #[test]
    fn resources_follow_their_holder_and_queue() {
        let since = Utc::now();
        let hold = |pid| ResourceHold {
            host: "h".into(),
            resource: "sim".into(),
            pid,
            cwd: None,
            scope: None,
            since,
        };
        let released = |pid| {
            Known::ResourceReleased(ResourceReleased {
                host: "h".into(),
                resource: "sim".into(),
                pid,
            })
        };
        let fold = |bodies: Vec<Known>| {
            let events: Vec<Event> = bodies
                .into_iter()
                .map(|b| ev(ActorKind::Host, b).into_event())
                .collect();
            State::fold(&events)
        };
        let queued = vec![
            Known::ResourceTaken(hold(1)),
            Known::ResourceWaiting(hold(2)),
            Known::ResourceWaiting(hold(3)),
            released(Some(1)),
            Known::ResourceTaken(hold(2)),
        ];
        let s = fold(queued.clone());
        let r = &s.resources["h:sim"];
        assert_eq!(r.holder.as_ref().map(|h| h.pid), Some(2));
        assert_eq!(r.waiting.iter().map(|w| w.pid).collect::<Vec<_>>(), [3]);
        let mut all_gone = queued.clone();
        all_gone.extend([released(Some(3)), released(Some(2))]);
        assert!(fold(all_gone).resources.is_empty());
        let mut reset = queued;
        reset.push(released(None));
        assert!(
            fold(reset).resources.is_empty(),
            "a release without pid clears it"
        );
    }

    #[test]
    fn refused_kills_keep_the_latest() {
        let events: Vec<Event> = (0..MAX_REFUSED as u32 + 3)
            .map(|pid| {
                agent_ev(Known::KillRefused(KillRefused {
                    host: "h".into(),
                    pid,
                    name: "zsh".into(),
                    reason: "your own ancestor".into(),
                }))
                .into_event()
            })
            .collect();
        let s = State::fold(&events);
        assert_eq!(s.refused_kills.len(), MAX_REFUSED);
        assert_eq!(s.refused_kills[0].pid, 3);
    }

    #[test]
    fn task_lifecycle_and_ownership() {
        let t = "task:p/1";
        let events: Vec<Event> = [
            agent_ev(Known::TaskCreated(TaskCreated {
                title: "one".into(),
                url: None,
            })),
            agent_ev(Known::TaskAssigned(TaskAssigned {
                agent: "agent:h/s/a".into(),
                lead: None,
            })),
            agent_ev(Known::DepAdded(Dep {
                on: "task:p/0".into(),
            })),
        ]
        .into_iter()
        .map(Envelope::into_event)
        .collect();
        let mut s = State::fold(&events);
        assert_eq!(s.tasks[t].title, "one");
        assert_eq!(s.tasks[t].state, TaskState::Working);
        assert_eq!(s.owner_of(t), Some("agent:h/s/a"));
        assert!(s.tasks[t].blocked_by.contains("task:p/0"));

        s.apply(
            &agent_ev(Known::DepRemoved(Dep {
                on: "task:p/0".into(),
            }))
            .into_event(),
        );
        assert!(s.tasks[t].blocked_by.is_empty());
        s.apply(
            &agent_ev(Known::TaskState(TaskStateChanged {
                state: TaskState::Completed,
            }))
            .into_event(),
        );
        assert_eq!(s.owner_of(t), None, "a finished task is free");
        assert_eq!(
            s.tasks[t].owner.as_deref(),
            Some("agent:h/s/a"),
            "history kept"
        );
    }

    #[test]
    fn unassign_frees_the_task() {
        let mut s = State::default();
        s.apply(
            &agent_ev(Known::TaskAssigned(TaskAssigned {
                agent: "agent:h/s/a".into(),
                lead: None,
            }))
            .into_event(),
        );
        assert_eq!(
            s.tasks["task:p/1"].title, "task:p/1",
            "created on first sight"
        );
        s.apply(&agent_ev(Known::TaskUnassigned(TaskUnassigned { reason: None })).into_event());
        assert_eq!(s.owner_of("task:p/1"), None);
    }

    #[test]
    fn blockers_asks_msgs_open_and_close() {
        let mut s = State::default();
        let b = agent_ev(Known::BlockerRaised(BlockerRaised {
            what: "sim".into(),
            owner: None,
            unblocks_when: None,
        }));
        let bid = b.id.clone();
        s.apply(&b.into_event());
        let ask = agent_ev(Known::AskRaised(AskRaised {
            question: "a or b?".into(),
            options: vec!["a".into(), "b".into()],
            recommendation: Some("a".into()),
            context: None,
        }));
        s.apply(&ask.into_event());
        let m = ev(
            ActorKind::Agent,
            Known::MsgSent(MsgSent {
                to: "agent:h/s/b".into(),
                kind: "FYI".into(),
                reference: "task:p/1".into(),
                text: "hi".into(),
            }),
        );
        let mid = m.id.clone();
        s.apply(&m.into_event());
        assert_eq!((s.blockers.len(), s.asks.len(), s.msgs.len()), (1, 1, 1));

        s.apply(
            &ev(
                ActorKind::Agent,
                Known::BlockerResolved(BlockerResolved { note: None }),
            )
            .re(&bid)
            .into_event(),
        );
        s.apply(
            &ev(
                ActorKind::Host,
                Known::MsgDelivered(MsgDelivered {
                    to: "agent:h/s/b".into(),
                    observed: AgentState::Working,
                    rung: None,
                    attempts: None,
                }),
            )
            .re(&mid)
            .into_event(),
        );
        assert_eq!((s.blockers.len(), s.msgs.len()), (0, 0));
    }

    fn role(agent: &str, level: Level) -> Event {
        ev(
            ActorKind::Agent,
            Known::AgentRole(AgentRole {
                agent: agent.into(),
                level: Some(level),
                lead: None,
                domain: None,
            }),
        )
        .into_event()
    }

    fn ask_by(agent: &str, q: &str) -> Event {
        let mut e = ev(
            ActorKind::Agent,
            Known::AskRaised(AskRaised {
                question: q.into(),
                options: vec!["a".into(), "b".into(), "c".into()],
                recommendation: None,
                context: None,
            }),
        )
        .task("task:p/1");
        e.actor.id = agent.into();
        e.into_event()
    }

    #[test]
    fn the_inbox_holds_only_lead_level_questions() {
        let mut s = State::default();
        for (agent, level) in [
            ("agent:h/s/impl", Level::Implementer),
            ("agent:h/s/il", Level::ImplementationLead),
            ("agent:h/s/dl", Level::DomainLead),
            ("agent:h/s/rc", Level::RootCoordinator),
        ] {
            s.apply(&role(agent, level));
            s.apply(&ask_by(agent, agent));
        }
        s.apply(&ask_by("agent:h/s/norole", "no role declared"));
        let by: Vec<_> = s.inbox().into_iter().map(|i| i.ask.by).collect();
        assert_eq!(
            by.len(),
            3,
            "implementer and role-less asks stay out: {by:?}"
        );
        assert!(
            !by.iter()
                .any(|b| b.ends_with("/impl") || b.ends_with("/norole"))
        );
        assert_eq!(
            s.asks.len(),
            5,
            "they are still open asks for everyone else"
        );
    }

    #[test]
    fn an_answer_is_sent_then_acked_and_leaves_the_open_asks() {
        let mut s = State::default();
        s.apply(&role("agent:h/s/dl", Level::DomainLead));
        let ask = ask_by("agent:h/s/dl", "ship it?");
        let aid = ask.id.clone();
        s.apply(&ask);
        let mut msg = ev(
            ActorKind::Human,
            Known::MsgSent(MsgSent {
                to: "agent:h/s/dl".into(),
                kind: "ANSWER".into(),
                reference: aid.clone(),
                text: "b".into(),
            }),
        );
        msg.actor.id = "human:board".into();
        let mid = msg.id.clone();
        s.apply(&msg.into_event());
        let mut ans = ev(
            ActorKind::Human,
            Known::AskAnswered(AskAnswered {
                answer: "b".into(),
                approve: false,
                approves: None,
                msg: mid.clone(),
            }),
        )
        .re(&aid);
        ans.actor.id = "human:board".into();
        s.apply(&ans.into_event());
        assert!(s.asks.is_empty(), "an answered ask is no longer open");
        let item = &s.inbox()[0];
        assert_eq!(item.answer.as_ref().map(|a| a.answer.as_str()), Some("b"));
        assert_eq!(item.answer.as_ref().and_then(|a| a.acked), None, "sent");

        s.apply(
            &ev(
                ActorKind::Host,
                Known::MsgDelivered(MsgDelivered {
                    to: "agent:h/s/dl".into(),
                    observed: AgentState::Working,
                    rung: None,
                    attempts: None,
                }),
            )
            .re(&mid)
            .into_event(),
        );
        assert!(
            s.inbox()[0].answer.as_ref().unwrap().acked.is_some(),
            "acked"
        );
    }

    #[test]
    fn unknown_events_are_counted_not_applied() {
        let mut v = serde_json::to_value(agent_ev(Known::TaskCreated(TaskCreated {
            title: "x".into(),
            url: None,
        })))
        .unwrap();
        v["type"] = "cost.spent".into();
        let e: Event = serde_json::from_value(v).unwrap();
        let s = State::fold([&e]);
        assert_eq!(s.unknown, 1);
        assert!(s.tasks.is_empty());
        assert_eq!(s.last.as_deref(), Some(e.id.as_str()));
    }

    #[test]
    fn newer_version_of_a_known_type_is_skipped() {
        let mut e = agent_ev(Known::TaskAssigned(TaskAssigned {
            agent: "agent:h/s/a".into(),
            lead: None,
        }))
        .into_event();
        e.v = 2;
        let s = State::fold([&e]);
        assert_eq!(s.unknown, 1);
        assert_eq!(s.owner_of("task:p/1"), None);
    }

    #[test]
    fn reclaiming_a_finished_task_reopens_it() {
        let mut s = State::default();
        s.apply(
            &agent_ev(Known::TaskAssigned(TaskAssigned {
                agent: "agent:h/s/a".into(),
                lead: None,
            }))
            .into_event(),
        );
        s.apply(
            &agent_ev(Known::TaskState(TaskStateChanged {
                state: TaskState::Completed,
            }))
            .into_event(),
        );
        s.apply(
            &agent_ev(Known::TaskAssigned(TaskAssigned {
                agent: "agent:h/s/b".into(),
                lead: None,
            }))
            .into_event(),
        );
        assert_eq!(s.tasks["task:p/1"].state, TaskState::Working);
        assert_eq!(s.owner_of("task:p/1"), Some("agent:h/s/b"));
    }

    #[test]
    fn late_state_change_does_not_revive_a_downed_agent() {
        let a = "agent:h/s/a".to_string();
        let mut s = State::default();
        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentDown(AgentDown {
                    agent: a.clone(),
                    reason: None,
                }),
            )
            .into_event(),
        );
        s.apply(
            &ev(
                ActorKind::Host,
                Known::AgentState(AgentStateChanged {
                    agent: a,
                    state: AgentState::Working,
                }),
            )
            .into_event(),
        );
        assert!(s.agents.is_empty());
    }

    #[test]
    fn first_task_created_names_the_task() {
        let mut s = State::default();
        let created = |title: &str| {
            agent_ev(Known::TaskCreated(TaskCreated {
                title: title.into(),
                url: None,
            }))
            .into_event()
        };
        s.apply(&created("winner"));
        s.apply(&created("loser"));
        assert_eq!(s.tasks["task:p/1"].title, "winner");
    }

    #[test]
    fn reopening_a_finished_task_clears_its_owner() {
        let mut s = State::default();
        s.apply(
            &agent_ev(Known::TaskAssigned(TaskAssigned {
                agent: "agent:h/s/a".into(),
                lead: None,
            }))
            .into_event(),
        );
        s.apply(
            &agent_ev(Known::TaskState(TaskStateChanged {
                state: TaskState::Completed,
            }))
            .into_event(),
        );
        s.apply(
            &agent_ev(Known::TaskState(TaskStateChanged {
                state: TaskState::Working,
            }))
            .into_event(),
        );
        assert_eq!(
            s.owner_of("task:p/1"),
            None,
            "the old owner is not resurrected"
        );
    }

    #[test]
    fn deps_accumulate_per_task() {
        let events: Vec<Event> = (0..20)
            .map(|i| {
                agent_ev(Known::DepAdded(Dep {
                    on: format!("task:p/{i}"),
                }))
                .into_event()
            })
            .collect();
        assert_eq!(State::fold(&events).tasks["task:p/1"].blocked_by.len(), 20);
    }

    fn by(id: &str, body: Known) -> Envelope<Known> {
        let kind = if id.starts_with("host:") {
            ActorKind::Host
        } else {
            ActorKind::Agent
        };
        let actor = Actor {
            kind,
            id: id.into(),
        };
        Envelope::new(actor, "p", body)
    }

    fn update_result(host: &str, commit: &str) -> Known {
        Known::UpdateResult(UpdateResult {
            host: host.into(),
            ok: true,
            commit: Some(commit.into()),
            error: None,
        })
    }

    #[test]
    fn a_host_reports_when_its_process_started() {
        let mut s = State::default();
        let health = |uptime_s: Option<u64>| {
            ev(
                ActorKind::Host,
                Known::HostHealth(HostHealth {
                    host: "h".into(),
                    disk_free: 1,
                    disk_total: 2,
                    agents: 0,
                    commit: None,
                    stats: uptime_s.map(|u| HostStats {
                        uptime_s: u,
                        ..HostStats::default()
                    }),
                }),
            )
            .into_event()
        };
        let e = health(Some(600));
        s.apply(&e);
        assert_eq!(
            s.hosts["h"].started,
            Some(e.ts - chrono::Duration::seconds(600))
        );
        s.apply(&health(None));
        assert_eq!(s.hosts["h"].started, None, "an old host says nothing");
    }

    #[test]
    fn an_update_request_remembers_the_one_host_it_asks() {
        let mut s = State::default();
        let ask = |host: Option<&str>| {
            ev(
                ActorKind::Host,
                Known::UpdateRequested(UpdateRequested {
                    host: host.map(String::from),
                }),
            )
            .into_event()
        };
        let (all, one) = (ask(None), ask(Some("h")));
        s.apply(&all);
        s.apply(&one);
        assert_eq!(s.updates[&all.id].host, None);
        assert_eq!(s.updates[&one.id].host.as_deref(), Some("h"));
    }

    #[test]
    fn each_host_answers_its_own_update_once_and_the_request_is_pruned_after_a_day() {
        let req = by(
            "agent:h/s/a",
            Known::UpdateRequested(UpdateRequested::default()),
        );
        let id = req.id.clone();
        let mut s = State::default();
        s.apply(&req.into_event());
        assert_eq!(s.updates[&id].by, "agent:h/s/a");
        assert!(s.updates[&id].results.is_empty());
        // another host may not answer for h1
        let forged = by("host:h2", update_result("h1", "bad")).re(&id);
        s.apply(&forged.into_event());
        assert!(s.updates[&id].results.is_empty());
        for (actor, host, commit) in [("h1", "h1", "abc"), ("h1", "h1", "x"), ("h2", "h2", "def")] {
            let e = by(&format!("host:{actor}"), update_result(host, commit)).re(&id);
            s.apply(&e.into_event());
        }
        let got: Vec<_> = s.updates[&id]
            .results
            .iter()
            .map(|(h, r)| (h.as_str(), r.commit.as_deref().unwrap()))
            .collect();
        assert_eq!(
            got,
            [("h1", "abc"), ("h2", "def")],
            "the first answer stands"
        );
        let mut late = by("host:h1", update_result("h1", "x")).re("other");
        late.ts += UPDATE_KEPT + chrono::Duration::seconds(1);
        s.apply(&late.into_event());
        assert!(s.updates.is_empty(), "pruned a day after the request");
    }

    #[test]
    fn a_failed_update_outlives_the_day_while_its_host_runs_the_commit_it_failed_on() {
        let mut s = State::default();
        let health = |commit: &str, plus: i64| {
            let mut e = ev(
                ActorKind::Host,
                Known::HostHealth(HostHealth {
                    host: "h".into(),
                    disk_free: 1,
                    disk_total: 2,
                    agents: 0,
                    commit: Some(commit.into()),
                    stats: None,
                }),
            );
            e.ts += chrono::Duration::seconds(plus);
            e.into_event()
        };
        let req = by("host:h", Known::UpdateRequested(UpdateRequested::default()));
        let id = req.id.clone();
        s.apply(&req.into_event());
        s.apply(&health("aaa", 1));
        let mut fail = ev(
            ActorKind::Host,
            Known::UpdateResult(UpdateResult {
                host: "h".into(),
                ok: false,
                commit: Some("aaa".into()),
                error: Some("boom".into()),
            }),
        )
        .re(&id);
        fail.ts += chrono::Duration::seconds(2);
        s.apply(&fail.into_event());
        // A day and a bit later the host still runs aaa: kept.
        s.apply(&health("aaa", UPDATE_KEPT.num_seconds() + 60));
        assert_eq!(s.updates.len(), 1, "kept while on aaa");
        // It moved on: pruned.
        s.apply(&health("bbb", UPDATE_KEPT.num_seconds() + 120));
        assert!(s.updates.is_empty(), "history once it runs another commit");
    }

    #[test]
    fn only_the_newest_failed_update_per_host_outlives_the_day() {
        let mut s = State::default();
        let health = |plus: i64| {
            let mut e = ev(
                ActorKind::Host,
                Known::HostHealth(HostHealth {
                    host: "h".into(),
                    disk_free: 1,
                    disk_total: 2,
                    agents: 0,
                    commit: Some("aaa".into()),
                    stats: None,
                }),
            );
            e.ts += chrono::Duration::seconds(plus);
            e.into_event()
        };
        s.apply(&health(1));
        let mut ids = Vec::new();
        for hour in [0, 2] {
            let mut req = by("host:h", Known::UpdateRequested(UpdateRequested::default()));
            req.ts += chrono::Duration::hours(hour);
            ids.push(req.id.clone());
            s.apply(&req.into_event());
            let mut fail = ev(
                ActorKind::Host,
                Known::UpdateResult(UpdateResult {
                    host: "h".into(),
                    ok: false,
                    commit: Some("aaa".into()),
                    error: None,
                }),
            )
            .re(&ids[ids.len() - 1]);
            fail.ts += chrono::Duration::hours(hour) + chrono::Duration::seconds(1);
            s.apply(&fail.into_event());
        }
        // 23 h after the first: both fit the day.
        s.apply(&health(23 * 3600));
        assert_eq!(s.updates.len(), 2);
        // 25 h after the first: past the day and not the newest, so gone; the second stays.
        s.apply(&health(25 * 3600));
        assert_eq!(s.updates.keys().collect::<Vec<_>>(), [&ids[1]]);
        // 30 h after the first, 28 h after the second: kept as the newest failure on aaa.
        s.apply(&health(30 * 3600));
        assert_eq!(s.updates.keys().collect::<Vec<_>>(), [&ids[1]]);
    }

    #[test]
    fn the_newest_failure_is_kept_per_host_and_only_one_on_the_hosts_current_commit() {
        let trigger = ev(
            ActorKind::Host,
            Known::HostHealth(HostHealth {
                host: "other".into(),
                disk_free: 1,
                disk_total: 2,
                agents: 0,
                commit: None,
                stats: None,
            }),
        )
        .into_event();
        let now = trigger.ts;
        let host = |commit: &str| Host {
            disk_free: 0,
            disk_total: 0,
            agents: 0,
            commit: Some(commit.into()),
            ts: now,
            started: None,
        };
        let res = |h: &str, ok: bool, commit: Option<&str>| {
            (
                h.to_string(),
                UpdateResult {
                    host: h.into(),
                    ok,
                    commit: commit.map(String::from),
                    error: None,
                },
            )
        };
        let up = |hours: i64, results: Vec<(String, UpdateResult)>| Update {
            by: "host:x".into(),
            since: now - chrono::Duration::hours(hours),
            host: None,
            results: results.into_iter().collect(),
        };
        let mut s = State::default();
        s.hosts.insert("a".into(), host("aaa"));
        s.hosts.insert("b".into(), host("bbb"));
        // Each host's newest failure on its own commit survives; each older one goes.
        s.updates
            .insert("a-old".into(), up(60, vec![res("a", false, Some("aaa"))]));
        s.updates
            .insert("a-new".into(), up(40, vec![res("a", false, Some("aaa"))]));
        s.updates
            .insert("b-old".into(), up(70, vec![res("b", false, Some("bbb"))]));
        s.updates
            .insert("b-new".into(), up(50, vec![res("b", false, Some("bbb"))]));
        // One update failing on both hosts stays while either needs it.
        s.updates.insert(
            "both".into(),
            up(
                30,
                vec![res("a", false, Some("aaa")), res("b", false, Some("bbb"))],
            ),
        );
        // A newer one that failed on another commit, or succeeded, does not displace the
        // newest failure on the commit the host runs.
        s.updates.insert(
            "a-elsewhere".into(),
            up(26, vec![res("a", false, Some("zzz"))]),
        );
        s.updates
            .insert("a-ok".into(), up(25, vec![res("a", true, Some("aaa"))]));
        s.updates.insert("fresh".into(), up(1, vec![]));
        s.apply(&trigger);
        let mut left: Vec<&str> = s.updates.keys().map(String::as_str).collect();
        left.sort();
        assert_eq!(left, ["both", "fresh"], "{left:?}");
    }

    #[test]
    fn a_later_success_ends_the_retention_of_an_older_failure() {
        let trigger = ev(
            ActorKind::Host,
            Known::HostHealth(HostHealth {
                host: "a".into(),
                disk_free: 1,
                disk_total: 2,
                agents: 0,
                commit: Some("aaa".into()),
                stats: None,
            }),
        )
        .into_event();
        let now = trigger.ts;
        let answer = |hours: i64, ok: bool| Update {
            by: "host:x".into(),
            since: now - chrono::Duration::hours(hours),
            host: None,
            results: [(
                "a".to_string(),
                UpdateResult {
                    host: "a".into(),
                    ok,
                    commit: Some("aaa".into()),
                    error: None,
                },
            )]
            .into(),
        };
        let mut s = State::default();
        s.updates.insert("failed".into(), answer(40, false));
        s.updates.insert("current".into(), answer(30, true));
        s.apply(&trigger);
        assert!(
            s.updates.is_empty(),
            "cleared by the later success: {:?}",
            s.updates.keys()
        );
        // The other way round, the failure is the newest answer and stays.
        s.updates.insert("current".into(), answer(40, true));
        s.updates.insert("failed".into(), answer(30, false));
        s.apply(&trigger);
        assert_eq!(s.updates.keys().collect::<Vec<_>>(), ["failed"]);
    }

    #[test]
    fn host_health_carries_the_commit() {
        let health = Known::HostHealth(HostHealth {
            host: "h1".into(),
            disk_free: 1,
            disk_total: 2,
            agents: 0,
            stats: None,
            commit: Some("abc".into()),
        });
        let mut s = State::default();
        s.apply(&by("host:h1", health).into_event());
        assert_eq!(s.hosts["h1"].commit.as_deref(), Some("abc"));
    }
}
