//! `state = fold(events)`. Pure: no I/O, no clock.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::{
    AgentState, Body, Event, Known, Level, QuotaWindow, ResourceHold, ResourceLimit,
    SpawnRequested, SpawnResult, StuckCause, TYPE_VERSION, TaskState, WorktreeProcs, WorktreeState,
};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct State {
    /// Id of the last event folded in.
    pub last: Option<String>,
    pub hosts: BTreeMap<String, Host>,
    pub agents: BTreeMap<String, Agent>,
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
    /// Agent programs outside herdr on each host, keyed `<host>:<pid>`.
    #[serde(default)]
    pub unmanaged: BTreeMap<String, Unmanaged>,
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
    pub ts: DateTime<Utc>,
}

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
                        usage: old.and_then(|a| a.usage.clone()),
                    },
                );
            }
            Known::AgentUsage(d) => {
                if let Some(a) = self.agents.get_mut(&d.agent) {
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
                        self.signals.remove(&d.agent);
                    }
                    if a.state != d.state {
                        a.state = d.state;
                        a.since = ts;
                    }
                }
            }
            Known::AgentDown(d) => {
                self.agents.remove(&d.agent);
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
                        ts,
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
            Known::SpawnResult(d) => {
                if let Some(re) = &e.re
                    && let Some(s) = self.spawns.get_mut(re)
                    && s.result.is_none()
                {
                    s.result = Some(d.clone());
                    s.answered = Some(ts);
                }
            }
        }
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
}
