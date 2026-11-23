//! `state = fold(events)`. Pure: no I/O, no clock.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::{
    ActorKind, AgentState, Body, COORDINATOR, Event, Known, Level, MergeRequested, MergeResult,
    MsgSent, ProcessHung, QuotaWindow, ResourceHold, ResourceLimit, RetireRequested, RetireResult,
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
    /// Each agent's open `BLOCKED`/`QUESTION` to its lead, until answered (see `Waiting`).
    #[serde(default)]
    pub waiting: BTreeMap<String, Waiting>,
    /// Each up agent's last `msg.sent`, what `IDLE` quotes and `auto_retire` reads (`DONE`/`MERGED`).
    #[serde(default)]
    pub reports: BTreeMap<String, Report>,
    /// When each agent's lead last told it anything: new work after a `DONE`, so `auto_retire`
    /// waits for a fresh one. Gone when the agent goes down.
    #[serde(default)]
    pub told: BTreeMap<String, DateTime<Utc>>,
    /// When the host last rang an agent's lead `IDLE`/`HELD` (key: the agent id) or `UNMANAGED`
    /// (key: the id + `UNMANAGED_KEY`); gone once the agent works or goes down.
    #[serde(default)]
    pub idle_rung: BTreeMap<String, DateTime<Utc>>,
    /// The last `HELD` text rung per agent and how many times in a row: the host backs off an
    /// unchanged one. Gone once the agent works, changes state, goes down or is rung `IDLE`.
    #[serde(default)]
    pub held_streak: BTreeMap<String, (String, u32)>,
    /// Delivered doorbells their recipient has not read (`msg.read`) yet: retransmitted until it does.
    #[serde(default)]
    pub unread: BTreeMap<String, Unread>,
    /// When each agent that is not up went down (`agent.down`), kept 24 h: a tell to it goes to
    /// the coordinator `GONE_AFTER` later, never on a listing flap.
    #[serde(default)]
    pub gone: BTreeMap<String, DateTime<Utc>>,
    /// Read doorbells by the id of their `msg.sent`, kept for 24 h after the read.
    #[serde(default)]
    pub read: BTreeMap<String, Read>,
    /// Tells handed back to their sender as `UNREAD` (`escalations`), kept while they are queued.
    #[serde(default)]
    pub escalated: BTreeSet<String>,
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
    /// Retire requests by their `retire.requested` id, kept for 24 h.
    #[serde(default)]
    pub retires: BTreeMap<String, Retire>,
    /// Merge requests by their `merge.requested` id, kept for 24 h.
    #[serde(default)]
    pub merges: BTreeMap<String, Merge>,
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
    /// Since when that commit has been behind `origin/main`, as its last `host.health` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_since: Option<DateTime<Utc>>,
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
    /// The process it waits on, on the raiser's host, and when that process started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
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

/// A `retire.requested` and the `retire.result` that answered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Retire {
    pub request: RetireRequested,
    /// Who asked (the request's actor id).
    pub by: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RetireResult>,
    /// The request's `ts`.
    pub since: DateTime<Utc>,
}

/// A `merge.requested` and the `merge.result` that answered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Merge {
    pub request: MergeRequested,
    /// Who asked (the request's actor id).
    pub by: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<MergeResult>,
    /// The request's `ts`.
    pub since: DateTime<Utc>,
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

/// An agent's last `BLOCKED` or `QUESTION`, open until the lead tells it anything or it works.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Waiting {
    /// The id of the `msg.sent`.
    pub id: String,
    pub to: String,
    pub kind: String,
    pub text: String,
    pub since: DateTime<Utc>,
    /// The host already re-rang the lead (`STILL-WAITING`).
    #[serde(default)]
    pub rung: bool,
}

/// An agent's last `msg.sent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub kind: String,
    pub text: String,
    pub since: DateTime<Utc>,
}

/// Suffix of the `idle_rung` key of an `UNMANAGED` ring.
pub const UNMANAGED_KEY: &str = "#unmanaged";

/// An agent that works owes nobody an answer or an idle ring.
fn clear_asks(
    waiting: &mut BTreeMap<String, Waiting>,
    idle_rung: &mut BTreeMap<String, DateTime<Utc>>,
    held_streak: &mut BTreeMap<String, (String, u32)>,
    agent: &str,
) {
    waiting.remove(agent);
    idle_rung.remove(agent);
    held_streak.remove(agent);
}

/// A delivered doorbell not read yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Unread {
    #[serde(flatten)]
    pub msg: Msg,
    /// The last delivery's `ts`: the retransmit clock starts here.
    pub delivered: DateTime<Utc>,
    /// Deliveries so far, the first one included.
    pub rings: u32,
}

/// A doorbell its recipient read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Read {
    pub from: String,
    pub to: String,
    /// The reader: the agent the message reached.
    pub by: String,
    pub since: DateTime<Utc>,
    pub at: DateTime<Utc>,
}

/// Where a doorbell is, as its sender sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MailStatus {
    Sent,
    Delivered,
    Read,
}

/// A read doorbell is kept this long after the read, for its sender to see.
pub const READ_KEPT: chrono::Duration = chrono::Duration::hours(24);

/// A status report (`MERGED`, `DONE`, `RETIRE`, `OPEN`, `FIX`) still undelivered this long
/// after it was sent is history: its host was away, and what it announced has long been acted on.
pub const MSG_FRESH: chrono::Duration = chrono::Duration::hours(6);

/// Any doorbell still unread this long after it was sent has expired: it is no longer rung, and
/// its recipient's host hands it to the coordinator as a dead letter. Nothing is dropped.
pub const MSG_EXPIRE: chrono::Duration = chrono::Duration::hours(24);

/// Whether an agent of this program takes a prompt typed mid-turn: Claude Code queues it and
/// reads it at the next turn boundary. Any other program is never typed into while busy.
pub fn queues_input(kind: Option<&str>) -> bool {
    kind == Some("claude")
}

impl Msg {
    /// Why ringing this doorbell now would be a false alarm, if it would: a status report that
    /// waited out a host reconnect. Questions and instructions (`ASK`, `BLOCKED`, ...) never go
    /// stale; past `MSG_EXPIRE` every tell goes to the coordinator instead (`dead_letters`).
    pub fn stale(&self, now: DateTime<Utc>) -> Option<String> {
        (STATUS_KINDS.contains(&self.kind.as_str()) && now - self.since > MSG_FRESH).then(|| {
            format!(
                "{} sent {}h ago, before this host came back",
                self.kind,
                (now - self.since).num_hours()
            )
        })
    }
}

impl Msg {
    /// Unread `MSG_EXPIRE` after it was sent: no longer rung, it goes to the coordinator. A dead
    /// letter itself never expires.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.kind != DEADLETTER && now - self.since > MSG_EXPIRE
    }
}

/// The kind of the tell that hands an expired or orphaned tell to the coordinator.
pub const DEADLETTER: &str = "DEADLETTER";

/// The kind of the tell that tells a sender its tell to a working agent is still unread.
pub const UNREAD: &str = "UNREAD";

/// The kind of the tell that re-rings a lead whose agent's BLOCKED or QUESTION got no reply.
pub const STILL_WAITING: &str = "STILL-WAITING";

/// A tell to a working agent still unread this long after its send goes back to its sender.
pub const UNREAD_ESCALATE: chrono::Duration = chrono::Duration::minutes(10);

/// Status reports: nothing for the recipient to act on now, nor for their sender if unread.
const STATUS_KINDS: [&str; 5] = ["MERGED", "DONE", "RETIRE", "OPEN", "FIX"];

/// A tell whose recipient is not up this long after it was sent goes to the coordinator.
pub const GONE_AFTER: chrono::Duration = chrono::Duration::minutes(10);

/// The host part of `agent:<host>/<session>/<name>`.
fn agent_host(id: &str) -> Option<&str> {
    id.strip_prefix("agent:")?.split('/').next()
}

impl State {
    /// Tells `host` must hand to the coordinator now, as `(id, dead letter)`: unread (delivered or
    /// not) `MSG_EXPIRE` after the send, or to an agent down (`agent.down`) for `GONE_AFTER`. The
    /// recipient's host hands them over; an expired one also the coordinator's host, for a
    /// recipient whose host is gone (both write the same event). A tell to a role nobody holds waits.
    pub fn dead_letters(&self, host: &str, now: DateTime<Utc>) -> Vec<(String, MsgSent)> {
        self.msgs
            .iter()
            .chain(self.unread.iter().map(|(id, u)| (id, &u.msg)))
            .filter(|(_, m)| m.kind != DEADLETTER)
            .filter_map(|(id, m)| {
                // An undelivered stale report goes in the digest (`report_digest`) instead.
                if self.msgs.contains_key(id) && m.stale(now).is_some() {
                    return None;
                }
                let to = self.recipient(&m.to)?;
                let mine = agent_host(to) == Some(host);
                let why = if m.expired(now) && (mine || self.coordinator_on(host)) {
                    "expired"
                } else if mine
                    && !self.agents.contains_key(to)
                    && self.gone.get(to).is_some_and(|at| now - *at >= GONE_AFTER)
                {
                    "recipient gone"
                } else {
                    return None;
                };
                let age = now - m.since;
                let text = format!(
                    "[{why}, queued {}h{:02}m ago from {} to {}, sent {}] {}",
                    age.num_hours(),
                    age.num_minutes() % 60,
                    m.from,
                    m.to,
                    m.since.format("%Y-%m-%d %H:%MZ"),
                    m.text
                );
                Some((
                    id.clone(),
                    MsgSent {
                        to: COORDINATOR.into(),
                        kind: DEADLETTER.into(),
                        reference: m.reference.clone(),
                        text,
                        closes: Vec::new(),
                    },
                ))
            })
            .collect()
    }

    /// Agents on `host` that asked (`BLOCKED`/`QUESTION`) at least `min` ago, sit not working, and
    /// whose lead has told them nothing since: `(agent, ask)`. A host's `STILL-WAITING` marks `rung`.
    pub fn unanswered(
        &self,
        host: &str,
        now: DateTime<Utc>,
        min: chrono::Duration,
    ) -> Vec<(&String, &Waiting)> {
        self.waiting
            .iter()
            .filter(|(id, w)| {
                !w.rung
                    && now - w.since >= min
                    && self
                        .agents
                        .get(*id)
                        .is_some_and(|a| a.host == host && a.state != AgentState::Working)
            })
            .collect()
    }

    /// Tells to a `working` agent on `host` still unread (delivered or not) `UNREAD_ESCALATE` after
    /// the send, as `(id, letter)`: one `UNREAD` tell back to each sender that is up, once per tell
    /// (`escalated`). The tell stays queued. Status reports and the fleet's own letters never go back.
    pub fn escalations(&self, host: &str, now: DateTime<Utc>) -> Vec<(String, MsgSent)> {
        self.msgs
            .iter()
            .chain(self.unread.iter().map(|(id, u)| (id, &u.msg)))
            .filter(|(id, m)| {
                !self.escalated.contains(*id)
                    && ![DEADLETTER, UNREAD].contains(&m.kind.as_str())
                    && !STATUS_KINDS.contains(&m.kind.as_str())
                    && now - m.since >= UNREAD_ESCALATE
                    && !m.expired(now)
                    && self.agents.contains_key(&m.from)
            })
            .filter_map(|(id, m)| {
                let to = self.recipient(&m.to)?;
                let a = self.agents.get(to)?;
                if a.host != host || a.state != AgentState::Working || m.from == to {
                    return None;
                }
                let age = now - m.since;
                let text = format!(
                    "[unread {}h{:02}m: {to} is working and has not read it] {} {}: {}",
                    age.num_hours(),
                    age.num_minutes() % 60,
                    m.kind,
                    m.reference,
                    m.text
                );
                Some((
                    id.clone(),
                    MsgSent {
                        to: m.from.clone(),
                        kind: UNREAD.into(),
                        reference: m.reference.clone(),
                        text,
                        closes: Vec::new(),
                    },
                ))
            })
            .collect()
    }

    /// Whether the agent holding the coordinator role is up on `host`.
    fn coordinator_on(&self, host: &str) -> bool {
        self.coordinator()
            .and_then(|c| self.agents.get(c))
            .is_some_and(|c| c.host == host)
    }

    /// Status reports (`Msg::stale`) still undelivered to an agent on `host`, oldest first, as
    /// one `DEADLETTER` digest to the coordinator: a count, then one line each. `None` with none.
    /// A long absence costs the coordinator one message per sweep, never one per report.
    pub fn report_digest(&self, host: &str, now: DateTime<Utc>) -> Option<(Vec<String>, MsgSent)> {
        let mut stale: Vec<(&String, &Msg)> = self
            .msgs
            .iter()
            .filter(|(_, m)| m.stale(now).is_some())
            .filter(|(_, m)| {
                // As `dead_letters`: the recipient's host, and for an expired one the coordinator's.
                self.recipient(&m.to).is_some_and(|to| {
                    agent_host(to) == Some(host) || (m.expired(now) && self.coordinator_on(host))
                })
            })
            .collect();
        if stale.is_empty() {
            return None;
        }
        stale.sort_by_key(|(id, m)| (m.since, *id));
        let n = stale.len();
        let mut text = format!(
            "{n} status report{} unread over 6 h:",
            if n == 1 { "" } else { "s" }
        );
        for (_, m) in &stale {
            let age = now - m.since;
            text.push_str(&format!(
                "\n- {}h{:02}m ago {} → {} {} {}: {}",
                age.num_hours(),
                age.num_minutes() % 60,
                crate::host::short(&m.from),
                crate::host::short(&m.to),
                m.kind,
                m.reference,
                m.text
            ));
        }
        let ids: Vec<String> = stale.iter().map(|(id, _)| (*id).clone()).collect();
        Some((
            ids.clone(),
            MsgSent {
                to: COORDINATOR.into(),
                kind: DEADLETTER.into(),
                reference: "digest".into(),
                text,
                closes: ids,
            },
        ))
    }

    /// Tells to `me` (by id, or by a role it holds) not read yet and not expired at `now`,
    /// delivered or not, oldest first: what `agent-fleet read` shows and acks.
    pub fn unread_for(&self, me: &str, now: DateTime<Utc>) -> Vec<(&String, &Msg)> {
        let mine = |m: &Msg| m.to == me || self.recipient(&m.to) == Some(me);
        let mut v: Vec<_> = self
            .msgs
            .iter()
            .chain(self.unread.iter().map(|(id, u)| (id, &u.msg)))
            .filter(|(_, m)| mine(m) && m.stale(now).is_none() && !m.expired(now))
            .collect();
        v.sort_by_key(|(id, m)| (m.since, *id));
        v
    }

    /// Tells `me` sent that the state still holds, oldest first, with where each is.
    pub fn outbox(&self, me: &str) -> Vec<(&String, &str, DateTime<Utc>, MailStatus)> {
        let mut v: Vec<_> = self
            .msgs
            .iter()
            .filter(|(_, m)| m.from == me)
            .map(|(id, m)| (id, m.to.as_str(), m.since, MailStatus::Sent))
            .chain(
                self.unread
                    .iter()
                    .filter(|(_, u)| u.msg.from == me)
                    .map(|(id, u)| (id, u.msg.to.as_str(), u.msg.since, MailStatus::Delivered)),
            )
            .chain(
                self.read
                    .iter()
                    .filter(|(_, r)| r.from == me)
                    .map(|(id, r)| (id, r.to.as_str(), r.since, MailStatus::Read)),
            )
            .collect();
        v.sort_by_key(|(id, _, since, _)| (*since, *id));
        v
    }

    /// Tells still undelivered to `to` (`coordinator` follows its holder): how many, and the oldest.
    pub fn pending_for(&self, to: &str) -> (usize, Option<DateTime<Utc>>) {
        let who = self.recipient(to);
        let mine = self
            .msgs
            .values()
            .filter(|m| m.to == to || (who.is_some() && self.recipient(&m.to) == who));
        mine.fold((0, None), |(n, old), m| {
            (
                n + 1,
                Some(old.map_or(m.since, |o: DateTime<Utc>| o.min(m.since))),
            )
        })
    }

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

    /// The up agent holding the root-coordinator role; of several, the latest declared, so a
    /// handover is followed even before the old holder drops its role.
    pub fn coordinator(&self) -> Option<&str> {
        self.roles
            .iter()
            .filter(|(id, r)| r.level == Level::RootCoordinator && self.agents.contains_key(*id))
            .max_by_key(|(_, r)| r.since)
            .map(|(id, _)| id.as_str())
    }

    /// The agent a `msg.sent` to `to` goes to now: `coordinator` is the role's current holder.
    pub fn recipient<'a>(&'a self, to: &'a str) -> Option<&'a str> {
        match to {
            COORDINATOR => self.coordinator(),
            _ => Some(to),
        }
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
                self.gone.remove(&d.agent);
                // A re-up (host or herdr restart) keeps what the entry already knows: the last
                // working time, the current stretch when the state did not change, the usage.
                let old = self.agents.get(&d.agent);
                if old.is_none() {
                    // A new incarnation owes nothing to a previous one's ask.
                    clear_asks(
                        &mut self.waiting,
                        &mut self.idle_rung,
                        &mut self.held_streak,
                        &d.agent,
                    );
                }
                if old.is_some_and(|a| a.state != d.state) {
                    self.held_streak.remove(&d.agent);
                }
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
                                clear_asks(
                                    &mut self.waiting,
                                    &mut self.idle_rung,
                                    &mut self.held_streak,
                                    &d.agent,
                                );
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
                        clear_asks(
                            &mut self.waiting,
                            &mut self.idle_rung,
                            &mut self.held_streak,
                            &d.agent,
                        );
                        a.worked = Some(ts);
                        a.finished = None;
                        self.signals.remove(&d.agent);
                    }
                    if d.state == AgentState::Done {
                        a.finished = Some(ts);
                    }
                    if a.state != d.state {
                        self.held_streak.remove(&d.agent);
                        a.state = d.state;
                        a.since = ts;
                    }
                }
            }
            Known::AgentDown(d) => {
                clear_asks(
                    &mut self.waiting,
                    &mut self.idle_rung,
                    &mut self.held_streak,
                    &d.agent,
                );
                self.reports.remove(&d.agent);
                self.told.remove(&d.agent);
                self.idle_rung
                    .remove(&format!("{}{UNMANAGED_KEY}", d.agent));
                if self.agents.remove(&d.agent).is_some() {
                    self.gone.insert(d.agent.clone(), ts);
                }
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
                        behind_since: d.behind_since,
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
                        pid: d.pid,
                        started: d.started,
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
                match e.actor.kind {
                    ActorKind::Agent => {
                        if self.agents.contains_key(&e.actor.id) {
                            self.reports.insert(
                                e.actor.id.clone(),
                                Report {
                                    kind: d.kind.clone(),
                                    text: d.text.clone(),
                                    since: ts,
                                },
                            );
                        }
                        if ["BLOCKED", "QUESTION"].contains(&d.kind.as_str())
                            && self.agents.contains_key(&e.actor.id)
                        {
                            self.waiting.insert(
                                e.actor.id.clone(),
                                Waiting {
                                    id: e.id.clone(),
                                    to: d.to.clone(),
                                    kind: d.kind.clone(),
                                    text: d.text.clone(),
                                    since: ts,
                                    rung: false,
                                },
                            );
                        }
                        let from = e.actor.id.as_str();
                        let lead = self.roles.get(&d.to).and_then(|r| r.lead.as_deref());
                        if self.agents.contains_key(&d.to)
                            && (lead == Some(from)
                                || (lead.is_none() && self.coordinator() == Some(from)))
                        {
                            self.told.insert(d.to.clone(), ts);
                        }
                        // Any tell from the lead to the waiting agent is the answer.
                        if self
                            .waiting
                            .get(&d.to)
                            .is_some_and(|w| w.to == from || self.recipient(&w.to) == Some(from))
                        {
                            self.waiting.remove(&d.to);
                        }
                    }
                    ActorKind::Host if d.kind == STILL_WAITING => {
                        if let Some(w) = self
                            .waiting
                            .values_mut()
                            .find(|w| Some(&w.id) == e.re.as_ref())
                        {
                            w.rung = true;
                        }
                    }
                    ActorKind::Host if ["IDLE", "HELD", "UNMANAGED"].contains(&d.kind.as_str()) => {
                        let key = match d.kind.as_str() {
                            "UNMANAGED" => format!("{}{UNMANAGED_KEY}", d.reference),
                            _ => d.reference.clone(),
                        };
                        match d.kind.as_str() {
                            "HELD" => {
                                let n = match self.held_streak.get(&key) {
                                    Some((t, n)) if *t == d.text => n + 1,
                                    _ => 1,
                                };
                                self.held_streak.insert(key.clone(), (d.text.clone(), n));
                            }
                            "IDLE" => {
                                self.held_streak.remove(&key);
                            }
                            _ => {}
                        }
                        self.idle_rung.insert(key, ts);
                    }
                    _ => {}
                }
                // A host's dead letter takes the tell it names (`re`) off the recipient's queue.
                // One for a tell already read or handed over (a host's view lagging) is dropped.
                // A digest hands over every tell in `closes`.
                if d.kind == DEADLETTER && e.actor.kind == ActorKind::Host {
                    let mut open = false;
                    for id in e.re.iter().chain(&d.closes) {
                        open |= self.msgs.remove(id).is_some() | self.unread.remove(id).is_some();
                    }
                    if !open {
                        return;
                    }
                }
                // A host's `UNREAD` marks the tell it names as handed back, which stays queued. One
                // for a tell no longer queued (a host's view lagging) is dropped.
                if d.kind == UNREAD && e.actor.kind == ActorKind::Host {
                    let Some(re) = e
                        .re
                        .as_ref()
                        .filter(|re| self.msgs.contains_key(*re) || self.unread.contains_key(*re))
                    else {
                        return;
                    };
                    self.escalated.insert(re.clone());
                }
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
            Known::MsgDelivered(d) => {
                if let Some(re) = &e.re {
                    // Only a ring that asked for an ack leaves the tell unread: one older than the ack
                    // (an upgrade folding its log) never comes back. `attempts: 0` closes a stale
                    // report unrung: nothing was shown, nothing to read.
                    let shown = d.ack && d.attempts != Some(0);
                    if let Some(msg) = self.msgs.remove(re) {
                        if shown {
                            let delivered = ts;
                            self.unread.insert(
                                re.clone(),
                                Unread {
                                    msg,
                                    delivered,
                                    rings: 1,
                                },
                            );
                        }
                    } else if shown && let Some(u) = self.unread.get_mut(re) {
                        u.delivered = ts;
                        u.rings += 1;
                    }
                    for a in self.answers.values_mut().filter(|a| &a.msg == re) {
                        a.acked.get_or_insert(ts);
                    }
                }
            }
            // Only the recipient's own read counts: the agent `to` names, or holds as a role.
            Known::MsgRead(_) => {
                if let Some(re) = &e.re {
                    let msg = self
                        .msgs
                        .get(re)
                        .or_else(|| self.unread.get(re).map(|u| &u.msg));
                    if let Some(m) = msg
                        && (m.to == e.actor.id
                            || self.recipient(&m.to) == Some(e.actor.id.as_str()))
                    {
                        let read = Read {
                            from: m.from.clone(),
                            to: m.to.clone(),
                            by: e.actor.id.clone(),
                            since: m.since,
                            at: ts,
                        };
                        self.msgs.remove(re);
                        self.unread.remove(re);
                        self.read.insert(re.clone(), read);
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
            Known::RetireRequested(d) => {
                self.retires.insert(
                    e.id.clone(),
                    Retire {
                        request: d.clone(),
                        by: e.actor.id.clone(),
                        project: e.project.clone(),
                        result: None,
                        since: ts,
                    },
                );
            }
            // Only the host it names may answer, and only once.
            Known::RetireResult(d) => {
                if let Some(re) = &e.re
                    && let Some(r) = self.retires.get_mut(re)
                    && r.result.is_none()
                    && e.actor.id == format!("host:{}", r.request.host)
                {
                    r.result = Some(d.clone());
                }
            }
            Known::MergeRequested(d) => {
                self.merges.insert(
                    e.id.clone(),
                    Merge {
                        request: d.clone(),
                        by: e.actor.id.clone(),
                        project: e.project.clone(),
                        result: None,
                        since: ts,
                    },
                );
            }
            // Only the host it names may answer, and only once.
            Known::MergeResult(d) => {
                if let Some(re) = &e.re
                    && let Some(m) = self.merges.get_mut(re)
                    && m.result.is_none()
                    && e.actor.id == format!("host:{}", m.request.host)
                {
                    m.result = Some(d.clone());
                }
            }
            // A record in the log for the views and `stats`; nothing in the state depends on it.
            Known::MergeDone(_) => {}
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
        self.read.retain(|_, r| e.ts - r.at <= READ_KEPT);
        let (msgs, unread) = (&self.msgs, &self.unread);
        self.escalated
            .retain(|id| msgs.contains_key(id) || unread.contains_key(id));
        self.gone.retain(|_, at| e.ts - *at <= MSG_EXPIRE);
        // The reducer has no clock: a spawn is pruned once a later event's `ts` is more than
        // SPAWN_KEPT past its answer (or its request, when it was never answered) — but never
        // while its agent is still up, so cap counting cannot lose sight of it.
        self.retires.retain(|_, r| e.ts - r.since <= SPAWN_KEPT);
        self.merges.retain(|_, m| e.ts - m.since <= SPAWN_KEPT);
        let agents = &self.agents;
        self.spawns.retain(|_, s| {
            agents.contains_key(&s.request.agent)
                || e.ts - s.answered.unwrap_or(s.since) <= SPAWN_KEPT
        });
    }

    /// The (repo, worktree, issue) to retire when `agent`'s last pane on `host` closes: the worktree of
    /// its latest answered-ok spawn, if the fleet MADE that tree (some spawn with no `worktree`
    /// in its request answered with it) and no newer spawn handed it to another agent. A handoff
    /// therefore retires nothing until its last agent closes; a spawn into an existing
    /// directory (a main checkout) never retires anything.
    pub fn retire_target(&self, agent: &str, host: &str) -> Option<(String, String, Option<u64>)> {
        let ok = |s: &Spawn| {
            s.result
                .as_ref()
                .filter(|r| r.ok)
                .and_then(|r| r.worktree.clone())
        };
        let (mine, tree) = self
            .spawns
            .values()
            .filter(|s| s.request.host == host && s.request.agent == agent)
            .filter_map(|s| ok(s).map(|t| (s, t)))
            .max_by_key(|(s, _)| s.since)?;
        let made = self
            .spawns
            .values()
            .any(|s| s.request.worktree.is_none() && ok(s).as_deref() == Some(&tree));
        // Another agent holds the tree: up in it now, or handed it in a newer spawn that is
        // answered ok or not answered yet (the window before its pane exists).
        let held = self.spawns.values().any(|s| {
            s.request.agent != agent
                && (ok(s).as_deref() == Some(&tree) && self.agents.contains_key(&s.request.agent)
                    || s.since > mine.since
                        && (ok(s).as_deref() == Some(&tree)
                            || s.result.is_none() && s.request.worktree.as_deref() == Some(&tree)))
        });
        (made && !held).then(|| (mine.request.repo.clone(), tree, mine.request.issue))
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

    impl State {
        /// Where the doorbell `id` is: sent (waiting for a ring), delivered (not read), or read.
        fn mail_status(&self, id: &str) -> Option<MailStatus> {
            if self.msgs.contains_key(id) {
                Some(MailStatus::Sent)
            } else if self.unread.contains_key(id) {
                Some(MailStatus::Delivered)
            } else {
                self.read.contains_key(id).then_some(MailStatus::Read)
            }
        }
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
    fn held_rings_count_while_the_text_repeats_and_reset_on_new_text_idle_or_a_state_change() {
        let t0 = Utc::now();
        let a = "agent:h/s/a";
        let mut s = State::default();
        let mut n = 0;
        let mut ev = |s: &mut State, kind: ActorKind, actor: &str, body| {
            n += 1;
            let mut e = Envelope::new(
                Actor {
                    kind,
                    id: actor.into(),
                },
                "p",
                body,
            )
            .into_event();
            e.ts = t0 + chrono::Duration::minutes(n);
            s.apply(&e);
        };
        let ring = |kind: &str, text: &str| {
            Known::MsgSent(MsgSent {
                to: "agent:h/s/lead".into(),
                kind: kind.into(),
                reference: a.into(),
                text: text.into(),
                closes: Vec::new(),
            })
        };
        let state = |st| {
            Known::AgentState(AgentStateChanged {
                agent: a.into(),
                state: st,
            })
        };
        ev(
            &mut s,
            ActorKind::Agent,
            a,
            Known::AgentUp(AgentUp {
                agent: a.into(),
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Idle,
            }),
        );
        for want in 1..=3 {
            ev(&mut s, ActorKind::Host, "host:h", ring("HELD", "x"));
            assert_eq!(s.held_streak[a], ("x".to_string(), want));
        }
        ev(&mut s, ActorKind::Host, "host:h", ring("HELD", "y"));
        assert_eq!(s.held_streak[a], ("y".to_string(), 1));
        // another ring kind for the same agent leaves the HELD streak alone, IDLE ends it
        ev(&mut s, ActorKind::Host, "host:h", ring("UNMANAGED", "u"));
        assert_eq!(s.held_streak[a].1, 1);
        ev(&mut s, ActorKind::Host, "host:h", ring("IDLE", "i"));
        assert!(!s.held_streak.contains_key(a));
        // a same-state report keeps it; a real state change, working and down clear it
        ev(&mut s, ActorKind::Host, "host:h", ring("HELD", "y"));
        ev(&mut s, ActorKind::Agent, a, state(AgentState::Idle));
        assert!(s.held_streak.contains_key(a));
        ev(&mut s, ActorKind::Agent, a, state(AgentState::Blocked));
        assert!(!s.held_streak.contains_key(a));
        ev(&mut s, ActorKind::Host, "host:h", ring("HELD", "y"));
        ev(&mut s, ActorKind::Agent, a, state(AgentState::Working));
        assert!(!s.held_streak.contains_key(a));
        ev(&mut s, ActorKind::Agent, a, state(AgentState::Idle));
        ev(&mut s, ActorKind::Host, "host:h", ring("HELD", "y"));
        let up = |st| {
            Known::AgentUp(AgentUp {
                agent: a.into(),
                host: "h".into(),
                kind: None,
                pane: None,
                state: st,
            })
        };
        ev(&mut s, ActorKind::Agent, a, up(AgentState::Idle));
        assert!(s.held_streak.contains_key(a));
        ev(&mut s, ActorKind::Agent, a, up(AgentState::Blocked));
        assert!(!s.held_streak.contains_key(a));
        ev(&mut s, ActorKind::Host, "host:h", ring("HELD", "y"));
        ev(
            &mut s,
            ActorKind::Agent,
            a,
            Known::AgentDown(AgentDown {
                agent: a.into(),
                reason: None,
            }),
        );
        assert!(s.held_streak.is_empty());
    }

    #[test]
    fn a_report_is_kept_and_an_idle_ring_marked_until_the_agent_works_or_goes_down() {
        let t0 = Utc::now();
        let at = |mins: i64, kind: ActorKind, actor: &str, body| {
            let mut e = Envelope::new(
                Actor {
                    kind,
                    id: actor.into(),
                },
                "p",
                body,
            )
            .into_event();
            e.ts = t0 + chrono::Duration::minutes(mins);
            e
        };
        let a = "agent:h/s/a";
        let msg = |kind: &str, reference: &str| {
            Known::MsgSent(MsgSent {
                to: "agent:h/s/lead".into(),
                kind: kind.into(),
                reference: reference.into(),
                text: "t".into(),
                closes: Vec::new(),
            })
        };
        let state = |st| {
            Known::AgentState(AgentStateChanged {
                agent: a.into(),
                state: st,
            })
        };
        let mut s = State::default();
        s.apply(&at(
            0,
            ActorKind::Agent,
            a,
            Known::AgentUp(AgentUp {
                agent: a.into(),
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Idle,
            }),
        ));
        s.apply(&at(1, ActorKind::Agent, a, msg("DONE", "r")));
        assert_eq!(s.reports[a].kind, "DONE");
        s.apply(&at(2, ActorKind::Host, "host:h", msg("IDLE", a)));
        s.apply(&at(3, ActorKind::Host, "host:h", msg("UNMANAGED", a)));
        assert!(s.idle_rung.contains_key(a));
        assert!(s.idle_rung.contains_key(&format!("{a}{UNMANAGED_KEY}")));
        // working clears the idle ring, not the unmanaged one
        s.apply(&at(4, ActorKind::Agent, a, state(AgentState::Working)));
        assert!(!s.idle_rung.contains_key(a));
        assert!(s.idle_rung.contains_key(&format!("{a}{UNMANAGED_KEY}")));
        s.apply(&at(
            5,
            ActorKind::Host,
            "host:h",
            Known::AgentDown(AgentDown {
                agent: a.into(),
                reason: None,
            }),
        ));
        assert!(s.reports.is_empty() && s.idle_rung.is_empty());
    }

    #[test]
    fn a_tell_from_the_lead_is_remembered_and_one_from_anyone_else_is_not() {
        let t0 = Utc::now();
        let at = |mins: i64, actor: &str, body| {
            let mut e = Envelope::new(
                Actor {
                    kind: ActorKind::Agent,
                    id: actor.into(),
                },
                "p",
                body,
            )
            .into_event();
            e.ts = t0 + chrono::Duration::minutes(mins);
            e
        };
        let (a, lead, peer) = ("agent:h/s/a", "agent:h/s/lead", "agent:h/s/peer");
        let tell = |to: &str| {
            Known::MsgSent(MsgSent {
                to: to.into(),
                kind: "FIX".into(),
                reference: "r".into(),
                text: "t".into(),
                closes: Vec::new(),
            })
        };
        let mut s = State::default();
        for id in [a, lead, peer] {
            s.apply(&at(
                0,
                id,
                Known::AgentUp(AgentUp {
                    agent: id.into(),
                    host: "h".into(),
                    kind: None,
                    pane: None,
                    state: AgentState::Idle,
                }),
            ));
        }
        s.roles.insert(
            a.into(),
            Role {
                level: Level::Implementer,
                lead: Some(lead.into()),
                domain: None,
                by: "x".into(),
                since: t0,
            },
        );
        s.apply(&at(1, peer, tell(a)));
        assert!(s.told.is_empty(), "a peer is not the lead");
        s.apply(&at(2, lead, tell(a)));
        assert_eq!(s.told[a], t0 + chrono::Duration::minutes(2));
        s.apply(&at(
            3,
            "host:h",
            Known::AgentDown(AgentDown {
                agent: a.into(),
                reason: None,
            }),
        ));
        assert!(s.told.is_empty());
    }

    #[test]
    fn with_no_declared_lead_the_coordinator_counts_as_the_lead() {
        let t0 = Utc::now();
        let at = |actor: &str, body| {
            let mut e = Envelope::new(
                Actor {
                    kind: ActorKind::Agent,
                    id: actor.into(),
                },
                "p",
                body,
            )
            .into_event();
            e.ts = t0;
            e
        };
        let (a, co, peer) = ("agent:h/s/a", "agent:h/s/co", "agent:h/s/peer");
        let tell = |to: &str| {
            Known::MsgSent(MsgSent {
                to: to.into(),
                kind: "FIX".into(),
                reference: "r".into(),
                text: "t".into(),
                closes: Vec::new(),
            })
        };
        let mut s = State::default();
        for id in [a, co, peer] {
            s.apply(&at(
                id,
                Known::AgentUp(AgentUp {
                    agent: id.into(),
                    host: "h".into(),
                    kind: None,
                    pane: None,
                    state: AgentState::Idle,
                }),
            ));
        }
        s.roles.insert(
            co.into(),
            Role {
                level: Level::RootCoordinator,
                lead: None,
                domain: None,
                by: "x".into(),
                since: t0,
            },
        );
        s.apply(&at(peer, tell(a)));
        assert!(s.told.is_empty());
        // a tell to an agent that is not up, or to the `coordinator` alias, is not recorded
        s.apply(&at(co, tell("agent:h/s/ghost")));
        s.apply(&at(co, tell("coordinator")));
        assert!(s.told.is_empty());
        s.apply(&at(co, tell(a)));
        assert_eq!(s.told[a], t0);
    }

    #[test]
    fn a_blocked_or_question_tell_stays_unanswered_until_the_lead_tells_the_agent_or_it_works() {
        let t0 = Utc::now();
        let at = |mins: i64, kind: ActorKind, actor: &str, id: &str, re: Option<&str>, body| {
            let mut e = Envelope::new(
                Actor {
                    kind,
                    id: actor.into(),
                },
                "p",
                body,
            );
            e.id = id.into();
            e.re = re.map(str::to_string);
            let mut e = e.into_event();
            e.ts = t0 + chrono::Duration::minutes(mins);
            e
        };
        let (a, lead) = ("agent:h/s/a", "agent:h/s/lead");
        let msg = |to: &str, kind: &str| {
            Known::MsgSent(MsgSent {
                to: to.into(),
                kind: kind.into(),
                reference: "r".into(),
                text: "which way?".into(),
                closes: Vec::new(),
            })
        };
        let up = |state| {
            Known::AgentUp(AgentUp {
                agent: a.into(),
                host: "h".into(),
                kind: None,
                pane: None,
                state,
            })
        };
        let min = chrono::Duration::minutes(10);
        let at20 = t0 + chrono::Duration::minutes(20);
        let fresh = || {
            let mut s = State::default();
            s.apply(&at(0, ActorKind::Agent, a, "u", None, up(AgentState::Idle)));
            s.apply(&at(1, ActorKind::Agent, a, "m", None, msg(lead, "BLOCKED")));
            s
        };
        // idle, nobody answered, past the limit: unanswered; before it: not yet
        let s = fresh();
        assert_eq!(s.unanswered("h", at20, min).len(), 1);
        assert!(
            s.unanswered("h", t0 + chrono::Duration::minutes(5), min)
                .is_empty()
        );
        assert!(s.unanswered("other", at20, min).is_empty());
        // an ask from an agent that is not up is not tracked
        let mut s = State::default();
        s.apply(&at(1, ActorKind::Agent, a, "m", None, msg(lead, "BLOCKED")));
        assert!(s.waiting.is_empty());
        // a status kind is no ask
        let mut s = State::default();
        s.apply(&at(0, ActorKind::Agent, a, "u", None, up(AgentState::Idle)));
        s.apply(&at(1, ActorKind::Agent, a, "m", None, msg(lead, "DONE")));
        assert!(s.unanswered("h", at20, min).is_empty());
        // the lead tells the agent anything: answered
        let mut s = fresh();
        s.apply(&at(2, ActorKind::Agent, lead, "r", None, msg(a, "ANSWER")));
        assert!(s.unanswered("h", at20, min).is_empty());
        // somebody else telling it is no answer
        let mut s = fresh();
        s.apply(&at(
            2,
            ActorKind::Agent,
            "agent:h/s/z",
            "r",
            None,
            msg(a, "HI"),
        ));
        assert_eq!(s.unanswered("h", at20, min).len(), 1);
        // the agent resumes working: answered
        let mut s = fresh();
        s.apply(&at(
            2,
            ActorKind::Host,
            "host:h",
            "w",
            None,
            Known::AgentState(AgentStateChanged {
                agent: a.into(),
                state: AgentState::Working,
            }),
        ));
        assert!(s.unanswered("h", at20, min).is_empty());
        // the host's STILL-WAITING (re = the ask) is rung once
        let mut s = fresh();
        s.apply(&at(
            12,
            ActorKind::Host,
            "host:h",
            "sw",
            Some("m"),
            msg(lead, STILL_WAITING),
        ));
        assert!(s.unanswered("h", at20, min).is_empty());
        // a role-addressed ask is answered by whoever holds the role
        let mut s = State::default();
        s.apply(&at(0, ActorKind::Agent, a, "u", None, up(AgentState::Idle)));
        s.apply(&at(
            1,
            ActorKind::Agent,
            a,
            "m",
            None,
            msg(COORDINATOR, "QUESTION"),
        ));
        assert_eq!(s.unanswered("h", at20, min).len(), 1);
    }

    #[test]
    fn a_tell_goes_sent_then_delivered_then_read_and_only_its_recipient_reads_it() {
        let t0 = Utc::now();
        let at = |mins, actor: &str, re: Option<&str>, body| {
            let kind = if actor.starts_with("host:") {
                ActorKind::Host
            } else {
                ActorKind::Agent
            };
            let mut e = Envelope::new(
                Actor {
                    kind,
                    id: actor.into(),
                },
                "p",
                body,
            );
            e.id = format!("e{mins}");
            e.re = re.map(str::to_string);
            let mut e = e.into_event();
            e.ts = t0 + chrono::Duration::minutes(mins);
            e
        };
        let to = "agent:h/s/b";
        let sent = Known::MsgSent(MsgSent {
            to: to.into(),
            kind: "ASK".into(),
            reference: "r".into(),
            text: "t".into(),
            closes: Vec::new(),
        });
        let delivered = || {
            Known::MsgDelivered(MsgDelivered {
                to: to.into(),
                observed: AgentState::Working,
                rung: None,
                attempts: Some(1),
                ack: true,
            })
        };
        let mut s = State::default();
        s.apply(&at(0, "agent:h/s/a", None, sent));
        assert_eq!(s.mail_status("e0"), Some(MailStatus::Sent));
        s.apply(&at(1, "host:h", Some("e0"), delivered()));
        assert!(s.msgs.is_empty(), "delivered: no longer waiting for a ring");
        assert_eq!(s.mail_status("e0"), Some(MailStatus::Delivered));
        assert_eq!(
            s.unread_for(to, t0 + chrono::Duration::minutes(9)).len(),
            1,
            "delivered and unread: still in the inbox"
        );
        assert!(s.unread_for("agent:h/s/a", t0).is_empty());
        assert_eq!(s.outbox("agent:h/s/a")[0].3, MailStatus::Delivered);
        let u = &s.unread["e0"];
        assert_eq!(
            (u.rings, u.delivered, u.msg.since),
            (1, t0 + chrono::Duration::minutes(1), t0)
        );
        // A retransmit: delivered again, the ring counted, the clock restarted.
        s.apply(&at(6, "host:h", Some("e0"), delivered()));
        assert_eq!(s.unread["e0"].rings, 2);
        assert_eq!(s.unread["e0"].delivered, t0 + chrono::Duration::minutes(6));
        // Someone else's ack is not an ack.
        s.apply(&at(
            7,
            "agent:h/s/c",
            Some("e0"),
            Known::MsgRead(MsgRead {}),
        ));
        assert_eq!(s.mail_status("e0"), Some(MailStatus::Delivered));
        s.apply(&at(8, to, Some("e0"), Known::MsgRead(MsgRead {})));
        assert!(s.unread.is_empty());
        assert_eq!(s.mail_status("e0"), Some(MailStatus::Read));
        assert_eq!(s.read["e0"].at, t0 + chrono::Duration::minutes(8));
        assert!(
            s.unread_for(to, t0 + chrono::Duration::minutes(9))
                .is_empty()
        );
        assert_eq!(s.outbox("agent:h/s/a")[0].3, MailStatus::Read);
        // An expired tell is never shown, even before its host closes it.
        s.apply(&at(
            10,
            "agent:h/s/a",
            None,
            Known::MsgSent(MsgSent {
                to: to.into(),
                kind: "FYI".into(),
                reference: "r".into(),
                text: "old".into(),
                closes: Vec::new(),
            }),
        ));
        assert_eq!(s.unread_for(to, t0 + chrono::Duration::hours(23)).len(), 1);
        assert!(
            s.unread_for(to, t0 + chrono::Duration::hours(25))
                .is_empty()
        );
        // Read is kept 24 h, then pruned by a later event.
        s.apply(&at(
            8 + 24 * 60 + 1,
            "agent:h/s/a",
            None,
            Known::MsgSent(MsgSent {
                to: "agent:h/s/c".into(),
                kind: "FYI".into(),
                reference: "r".into(),
                text: "later".into(),
                closes: Vec::new(),
            }),
        ));
        assert_eq!(s.mail_status("e0"), None);
    }

    #[test]
    fn a_tell_pulled_before_its_ring_is_never_rung_and_a_dropped_report_is_not_unread() {
        let t0 = Utc::now();
        let at = |mins, actor: &str, re: Option<&str>, body| {
            let kind = if actor.starts_with("host:") {
                ActorKind::Host
            } else {
                ActorKind::Agent
            };
            let mut e = Envelope::new(
                Actor {
                    kind,
                    id: actor.into(),
                },
                "p",
                body,
            );
            e.id = format!("e{mins}");
            e.re = re.map(str::to_string);
            let mut e = e.into_event();
            e.ts = t0 + chrono::Duration::minutes(mins);
            e
        };
        let sent = |to: &str| {
            Known::MsgSent(MsgSent {
                to: to.into(),
                kind: "DONE".into(),
                reference: "r".into(),
                text: "t".into(),
                closes: Vec::new(),
            })
        };
        let mut s = State::default();
        s.apply(&at(0, "agent:h/s/a", None, sent("agent:h/s/b")));
        s.apply(&at(
            1,
            "agent:h/s/b",
            Some("e0"),
            Known::MsgRead(MsgRead {}),
        ));
        assert!(s.msgs.is_empty() && s.unread.is_empty());
        assert_eq!(s.mail_status("e0"), Some(MailStatus::Read));
        // A stale report closed unrung (`attempts: 0`) was never shown: it is not unread.
        s.apply(&at(2, "agent:h/s/a", None, sent("agent:h/s/b")));
        s.apply(&at(
            3,
            "host:h",
            Some("e2"),
            Known::MsgDelivered(MsgDelivered {
                to: "agent:h/s/b".into(),
                observed: AgentState::Idle,
                rung: None,
                attempts: Some(0),
                ack: false,
            }),
        ));
        assert!(s.unread.is_empty());
        assert_eq!(s.mail_status("e2"), None);
        // A ring older than the ack (no `ack`): delivered for good, never unread, never rung again.
        s.apply(&at(4, "agent:h/s/a", None, sent("agent:h/s/b")));
        s.apply(&at(
            5,
            "host:h",
            Some("e4"),
            Known::MsgDelivered(MsgDelivered {
                to: "agent:h/s/b".into(),
                observed: AgentState::Working,
                rung: None,
                attempts: Some(1),
                ack: false,
            }),
        ));
        assert!(s.msgs.is_empty() && s.unread.is_empty());
    }

    #[test]
    fn expired_and_orphaned_tells_go_to_the_coordinator_from_the_recipients_host_once() {
        let now = Utc::now();
        let mut s = State::default();
        let msg = |to: &str, kind: &str, mins: i64| Msg {
            from: "agent:h/s/lead".into(),
            to: to.into(),
            kind: kind.into(),
            reference: "x#1".into(),
            text: "which?".into(),
            since: now - chrono::Duration::minutes(mins),
        };
        let up = |s: &mut State, id: &str| {
            s.agents.insert(
                id.into(),
                serde_json::from_value(serde_json::json!({
                    "host": agent_host(id), "state": "idle", "since": now
                }))
                .unwrap(),
            );
        };
        up(&mut s, "agent:h/s/a");
        up(&mut s, "agent:k/s/far");
        s.msgs.insert("fresh".into(), msg("agent:h/s/a", "ASK", 60));
        s.msgs
            .insert("old".into(), msg("agent:h/s/a", "ASK", 25 * 60));
        s.unread.insert(
            "unread-old".into(),
            Unread {
                msg: msg("agent:h/s/a", "FYI", 25 * 60),
                delivered: now - chrono::Duration::hours(24),
                rings: 6,
            },
        );
        // Down 11 min: its tells go. Down 2 min (a listing flap) with a 30 min old tell: it waits.
        s.gone
            .insert("agent:h/s/gone".into(), now - chrono::Duration::minutes(11));
        s.gone
            .insert("agent:h/s/flap".into(), now - chrono::Duration::minutes(2));
        s.msgs
            .insert("gone".into(), msg("agent:h/s/gone", "ASK", 12));
        s.msgs
            .insert("flap".into(), msg("agent:h/s/flap", "ASK", 30));
        s.msgs
            .insert("never-seen".into(), msg("agent:h/s/nobody", "ASK", 30));
        s.msgs
            .insert("elsewhere".into(), msg("agent:k/s/far", "ASK", 25 * 60));
        s.msgs
            .insert("dl".into(), msg(COORDINATOR, DEADLETTER, 50 * 60));
        let got = s.dead_letters("h", now);
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["gone", "old", "unread-old"], "{got:?}");
        let (_, dl) = &got[1];
        assert_eq!(
            (dl.to.as_str(), dl.kind.as_str()),
            (COORDINATOR, DEADLETTER)
        );
        assert_eq!(dl.reference, "x#1");
        assert!(
            dl.text.starts_with(
                "[expired, queued 25h00m ago from agent:h/s/lead to agent:h/s/a, sent "
            ) && dl.text.ends_with("] which?"),
            "{}",
            dl.text
        );
        assert!(got[0].1.text.starts_with("[recipient gone, "));
        assert_eq!(s.dead_letters("k", now).len(), 1, "the far host's own");
        // The coordinator's host also hands over expired tells, for a recipient's host that is gone:
        // the same events, by the same ids.
        s.apply(
            &ev(
                ActorKind::Agent,
                Known::AgentRole(AgentRole {
                    agent: "agent:k/s/far".into(),
                    level: Some(Level::RootCoordinator),
                    lead: None,
                    domain: None,
                }),
            )
            .into_event(),
        );
        let ids: Vec<String> = s
            .dead_letters("k", now)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            ids,
            vec!["elsewhere", "old", "unread-old"],
            "never a gone one"
        );

        // The host's dead letter closes the original in the same event; an agent's does not.
        let dead = |actor: ActorKind, re: &str| {
            let mut e = ev(actor, Known::MsgSent(got[1].1.clone()))
                .re(re)
                .into_event();
            e.ts = now;
            e
        };
        s.apply(&dead(ActorKind::Agent, "old"));
        assert!(s.msgs.contains_key("old"));
        s.apply(&dead(ActorKind::Host, "old"));
        s.apply(&dead(ActorKind::Host, "unread-old"));
        assert!(!s.msgs.contains_key("old") && s.unread.is_empty());
        let ids: Vec<String> = s
            .dead_letters("h", now)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec!["gone".to_string()], "each goes once");
        // A second hand-over of a tell already closed (a lagging view) adds nothing.
        let n = s.msgs.len();
        s.apply(&dead(ActorKind::Host, "old"));
        assert_eq!(s.msgs.len(), n);
    }

    #[test]
    fn a_tell_a_working_agent_leaves_unread_goes_back_to_its_sender_once_and_stays_queued() {
        let now = Utc::now();
        let mut s = State::default();
        let msg = |from: &str, kind: &str, mins: i64| Msg {
            from: from.into(),
            to: "agent:h/s/a".into(),
            kind: kind.into(),
            reference: "x#1".into(),
            text: "stop and rebase".into(),
            since: now - chrono::Duration::minutes(mins),
        };
        let up = |s: &mut State, id: &str, state: &str| {
            s.agents.insert(
                id.into(),
                serde_json::from_value(serde_json::json!({
                    "host": agent_host(id), "state": state, "since": now
                }))
                .unwrap(),
            );
        };
        up(&mut s, "agent:h/s/a", "working");
        up(&mut s, "agent:h/s/lead", "idle");
        s.msgs
            .insert("old".into(), msg("agent:h/s/lead", "ASK", 11));
        s.msgs
            .insert("young".into(), msg("agent:h/s/lead", "ASK", 9));
        // Nobody to tell: a sender that is not up.
        s.msgs
            .insert("orphan".into(), msg("agent:h/s/gone", "ASK", 30));
        // Nothing for the sender to act on: a status report, or the fleet's own letters.
        s.msgs
            .insert("report".into(), msg("agent:h/s/lead", "DONE", 30));
        s.msgs
            .insert("dl".into(), msg("agent:h/s/lead", DEADLETTER, 30));
        s.unread.insert(
            "rung".into(),
            Unread {
                msg: msg("agent:h/s/lead", "FYI", 20),
                delivered: now - chrono::Duration::minutes(15),
                rings: 2,
            },
        );
        let got = s.escalations("h", now);
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["old", "rung"], "{got:?}");
        let (_, back) = &got[0];
        assert_eq!(
            (
                back.to.as_str(),
                back.kind.as_str(),
                back.reference.as_str()
            ),
            ("agent:h/s/lead", UNREAD, "x#1")
        );
        assert!(
            back.text
                .starts_with("[unread 0h11m: agent:h/s/a is working and has not read it")
                && back.text.ends_with("] ASK x#1: stop and rebase"),
            "{}",
            back.text
        );
        assert!(
            s.escalations("k", now).is_empty(),
            "only the recipient's host"
        );
        // Idle, the agent is rung as usual: no escalation.
        up(&mut s, "agent:h/s/a", "idle");
        assert!(s.escalations("h", now).is_empty());
        up(&mut s, "agent:h/s/a", "working");

        // The host's letter marks the tell, which stays queued; an agent's does not.
        let letter = |actor: ActorKind, re: &str| {
            ev(actor, Known::MsgSent(got[0].1.clone()))
                .re(re)
                .into_event()
        };
        s.apply(&letter(ActorKind::Agent, "old"));
        assert_eq!(s.escalations("h", now).len(), 2);
        s.apply(&letter(ActorKind::Host, "old"));
        s.apply(&letter(ActorKind::Host, "rung"));
        assert!(s.escalations("h", now).is_empty(), "each goes back once");
        assert!(s.msgs.contains_key("old") && s.unread.contains_key("rung"));
        // Read: the mark goes with the tell, and a letter for it (a lagging view) adds nothing.
        s.apply(
            &ev(ActorKind::Agent, Known::MsgRead(MsgRead {}))
                .re("old")
                .into_event(),
        );
        assert!(!s.escalated.contains("old"), "pruned with its tell");
        let n = s.msgs.len();
        s.apply(&letter(ActorKind::Host, "old"));
        assert_eq!(s.msgs.len(), n);
    }

    #[test]
    fn stale_reports_go_to_the_coordinator_as_one_digest_that_closes_them_all() {
        let now = Utc::now();
        let mut s = State::default();
        let msg = |kind: &str, hours: i64, text: &str| Msg {
            from: "agent:h/s/o-1".into(),
            to: "agent:h/s/lead".into(),
            kind: kind.into(),
            reference: "r#1".into(),
            text: text.into(),
            since: now - chrono::Duration::hours(hours),
        };
        s.msgs.insert("01B".into(), msg("MERGED", 7, "merged abc"));
        s.msgs.insert("01A".into(), msg("DONE", 30, "done"));
        s.msgs.insert("01C".into(), msg("MERGED", 5, "fresh"));
        s.msgs.insert("01D".into(), msg("ASK", 7, "a question"));
        let (ids, d) = s.report_digest("h", now).unwrap();
        assert_eq!(ids, vec!["01A", "01B"], "oldest first, reports only");
        assert_eq!((d.to.as_str(), d.kind.as_str()), (COORDINATOR, DEADLETTER));
        let lines: Vec<&str> = d.text.lines().collect();
        assert_eq!(lines[0], "2 status reports unread over 6 h:");
        assert_eq!(lines[1], "- 30h00m ago o-1 → lead DONE r#1: done");
        assert_eq!(lines.len(), 3);
        assert!(s.report_digest("k", now).is_none(), "another host's");
        let dl: Vec<String> = s
            .dead_letters("h", now)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            !dl.contains(&"01A".to_string()),
            "an expired report goes in the digest only"
        );
        let e = ev(ActorKind::Host, Known::MsgSent(d)).into_event();
        s.apply(&e);
        assert!(!s.msgs.contains_key("01A") && !s.msgs.contains_key("01B"));
        assert!(s.msgs.contains_key("01C") && s.msgs.contains_key("01D"));
        assert!(s.report_digest("h", now).is_none(), "handed over once");
    }

    #[test]
    fn an_agent_down_is_gone_from_then_and_back_up_is_not() {
        let mut s = State::default();
        let up = Known::AgentUp(AgentUp {
            agent: "agent:h/s/a".into(),
            host: "h".into(),
            kind: None,
            state: AgentState::Idle,
            pane: None,
        });
        let down = || {
            Known::AgentDown(AgentDown {
                agent: "agent:h/s/a".into(),
                reason: None,
            })
        };
        s.apply(&ev(ActorKind::Host, down()).into_event());
        assert!(s.gone.is_empty(), "never up: no down time");
        s.apply(&ev(ActorKind::Host, up.clone()).into_event());
        let e = ev(ActorKind::Host, down()).into_event();
        s.apply(&e);
        assert_eq!(s.gone.get("agent:h/s/a"), Some(&e.ts));
        s.apply(&ev(ActorKind::Host, up).into_event());
        assert!(s.gone.is_empty());
    }

    #[test]
    fn a_message_to_coordinator_follows_the_role_to_its_current_holder() {
        let t0 = Utc::now();
        let at = |mins, body| {
            let mut e = ev(ActorKind::Host, body).into_event();
            e.ts = t0 + chrono::Duration::minutes(mins);
            e
        };
        let up = |name: &str| {
            Known::AgentUp(AgentUp {
                agent: format!("agent:h/s/{name}"),
                host: "h".into(),
                kind: None,
                state: AgentState::Idle,
                pane: None,
            })
        };
        let root = |name: &str| {
            Known::AgentRole(AgentRole {
                agent: format!("agent:h/s/{name}"),
                level: Some(Level::RootCoordinator),
                lead: None,
                domain: None,
            })
        };
        let down = |name: &str| {
            Known::AgentDown(AgentDown {
                agent: format!("agent:h/s/{name}"),
                reason: None,
            })
        };
        let sent = Known::MsgSent(MsgSent {
            to: COORDINATOR.into(),
            kind: "DONE".into(),
            reference: "x".into(),
            text: "t".into(),
            closes: Vec::new(),
        });
        let mut s = State::default();
        s.apply(&at(0, sent));
        assert_eq!(s.recipient(COORDINATOR), None, "nobody holds the role yet");
        s.apply(&at(1, root("w1:p1")));
        assert_eq!(
            s.coordinator(),
            None,
            "a holder that is not up is no holder"
        );
        s.apply(&at(2, up("w1:p1")));
        assert_eq!(s.recipient(COORDINATOR), Some("agent:h/s/w1:p1"));
        // A handover: the new holder wins while the old one is still up with its role.
        s.apply(&at(3, up("w2:p1")));
        s.apply(&at(4, root("w2:p1")));
        assert_eq!(s.recipient(COORDINATOR), Some("agent:h/s/w2:p1"));
        // The new holder gone: the old one, still up and still declared, holds it again.
        s.apply(&at(5, down("w2:p1")));
        assert_eq!(s.recipient(COORDINATOR), Some("agent:h/s/w1:p1"));
        // A restart in another pane: the old pane is gone, the role is declared again.
        s.apply(&at(6, down("w1:p1")));
        assert_eq!(s.recipient(COORDINATOR), None);
        s.apply(&at(7, up("w3:p1")));
        s.apply(&at(8, root("w3:p1")));
        assert_eq!(s.recipient(COORDINATOR), Some("agent:h/s/w3:p1"));
        // The queued message still names the role, not a pane.
        assert_eq!(s.msgs.values().next().unwrap().to, COORDINATOR);
        assert_eq!(s.recipient("agent:h/s/b"), Some("agent:h/s/b"));
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
            pid: None,
            started: None,
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
                closes: Vec::new(),
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
                    ack: false,
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
                closes: Vec::new(),
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
                    ack: false,
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

    #[test]
    fn a_retire_is_answered_once_and_only_by_the_host_it_names() {
        use crate::event::{RetireRequested, RetireResult};
        let mut s = State::default();
        let req = by(
            "agent:mac/s/me",
            Known::RetireRequested(RetireRequested {
                agent: "agent:kir/s/dv-1".into(),
                host: "kir".into(),
                decide: None,
            }),
        );
        let id = req.id.clone();
        s.apply(&req.into_event());
        assert!(s.retires[&id].result.is_none());
        let answer = |host: &str, ok: bool| {
            by(
                host,
                Known::RetireResult(RetireResult {
                    agent: "agent:kir/s/dv-1".into(),
                    ok,
                    text: format!("from {host}"),
                }),
            )
            .re(&id)
            .into_event()
        };
        s.apply(&answer("host:mac", true));
        assert!(
            s.retires[&id].result.is_none(),
            "another host cannot answer"
        );
        s.apply(&answer("host:kir", false));
        s.apply(&answer("host:kir", true));
        let r = s.retires[&id].result.as_ref().unwrap();
        assert!(
            !r.ok && r.text == "from host:kir",
            "the first answer stands"
        );
    }

    #[test]
    fn a_merge_is_answered_once_and_only_by_the_host_it_names() {
        use crate::event::{MergeDone, MergeRequested, MergeResult};
        let mut s = State::default();
        let req = by(
            "agent:mac/s/me",
            Known::MergeRequested(MergeRequested {
                repo: "o/r".into(),
                pr: 7,
                sha: "abcdef1".into(),
                host: "box".into(),
                allow_pin_rewind: None,
            }),
        );
        let id = req.id.clone();
        s.apply(&req.into_event());
        assert_eq!(s.merges[&id].by, "agent:mac/s/me");
        let answer = |host: &str, ok: bool| {
            by(
                host,
                Known::MergeResult(MergeResult {
                    ok,
                    text: format!("from {host}"),
                }),
            )
            .re(&id)
            .into_event()
        };
        s.apply(&answer("host:mac", true));
        assert!(s.merges[&id].result.is_none(), "another host cannot answer");
        s.apply(&answer("host:box", false));
        s.apply(&answer("host:box", true));
        let r = s.merges[&id].result.as_ref().unwrap();
        assert!(
            !r.ok && r.text == "from host:box",
            "the first answer stands"
        );
        let before = s.clone();
        s.apply(
            &by(
                "host:box",
                Known::MergeDone(MergeDone {
                    repo: "o/r".into(),
                    pr: 7,
                    sha: "a".repeat(40),
                    commit: None,
                }),
            )
            .into_event(),
        );
        assert_eq!(
            s.merges, before.merges,
            "merge.done is a record in the log only"
        );
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
                    behind_since: None,
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
                    behind_since: None,
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
                    behind_since: None,
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
                behind_since: None,
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
            behind_since: None,
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
                behind_since: None,
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
            behind_since: None,
        });
        let mut s = State::default();
        s.apply(&by("host:h1", health).into_event());
        assert_eq!(s.hosts["h1"].commit.as_deref(), Some("abc"));
    }

    #[test]
    fn a_status_report_older_than_six_hours_is_stale_and_any_tell_but_a_dead_letter_expires_at_a_day()
     {
        let now = Utc::now();
        let msg = |kind: &str, ago: i64| Msg {
            from: "agent:h/s/a".into(),
            to: "coordinator".into(),
            kind: kind.into(),
            reference: "x#1".into(),
            text: "t".into(),
            since: now - chrono::Duration::hours(ago),
        };
        for k in ["MERGED", "DONE", "RETIRE", "OPEN", "FIX"] {
            assert!(msg(k, 5).stale(now).is_none(), "{k} at 5h is still news");
            assert!(msg(k, 7).stale(now).unwrap().contains("7h ago"), "{k}");
        }
        assert!(
            !msg(DEADLETTER, 100).expired(now),
            "a dead letter waits for the coordinator"
        );
        for k in ["BLOCKED", "ASK", "FYI"] {
            assert!(msg(k, 30).stale(now).is_none(), "{k} is never stale");
            assert!(!msg(k, 23).expired(now), "{k} at 23h still rings");
            assert!(msg(k, 25).expired(now), "{k}");
        }
    }
}
