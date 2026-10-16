//! The event envelope and the v1 core types. Strict on write, tolerant on read.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::state::State;

/// Wire protocol. Bumped on a breaking change to the envelope or a core type.
pub const PROTOCOL: u32 = 1;
/// Version of the `agent-fleet api schema` document layout.
pub const SCHEMA_VERSION: u32 = 1;
/// Per-type schema version every v1 core type carries in `v`.
pub const TYPE_VERSION: u32 = 1;
/// Largest serialized event the hub accepts. Big content belongs in a file the event links to.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Agent,
    Human,
    Host,
    Hub,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub kind: ActorKind,
    /// `agent:<host>/<session>/<name>`, `host:<name>`, `human:<handle>`.
    pub id: String,
}

/// An agent's lifecycle state, as the terminal multiplexer reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

/// Task lifecycle, aligned with the A2A task states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// Every event: the envelope fields plus a `type` tag and its `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "Event")]
pub struct Envelope<B> {
    /// ULID chosen by the writer. Appending the same id twice is a no-op.
    pub id: String,
    /// Schema version of this event's type.
    pub v: u32,
    pub ts: DateTime<Utc>,
    pub actor: Actor,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The event this one answers or follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(flatten)]
    pub body: B,
}

/// An event as read from a log: a core type, or anything else kept verbatim.
pub type Event = Envelope<Body>;

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Known(Known),
    /// `{"type": …, "data": …}` that does not parse as a core type: an unknown type, or a known
    /// type whose `data` this binary cannot read. A newer `v` with readable `data` stays `Known`;
    /// `State::apply` skips it.
    Unknown(Value),
}

impl Serialize for Body {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Body::Known(k) => k.serialize(s),
            Body::Unknown(v) => v.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Body {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(match Known::deserialize(&v) {
            Ok(k) => Body::Known(k),
            Err(_) => Body::Unknown(v),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "data", deny_unknown_fields)]
pub enum Known {
    #[serde(rename = "agent.up")]
    AgentUp(AgentUp),
    #[serde(rename = "agent.state")]
    AgentState(AgentStateChanged),
    #[serde(rename = "agent.down")]
    AgentDown(AgentDown),
    #[serde(rename = "host.health")]
    HostHealth(HostHealth),
    #[serde(rename = "agent.usage")]
    AgentUsage(AgentUsage),
    #[serde(rename = "worktree.state")]
    WorktreeState(WorktreeState),
    #[serde(rename = "worktree.gone")]
    WorktreeGone(WorktreeGone),
    #[serde(rename = "worktree.procs")]
    WorktreeProcs(WorktreeProcs),
    #[serde(rename = "task.created")]
    TaskCreated(TaskCreated),
    #[serde(rename = "task.assigned")]
    TaskAssigned(TaskAssigned),
    #[serde(rename = "task.unassigned")]
    TaskUnassigned(TaskUnassigned),
    #[serde(rename = "task.state")]
    TaskState(TaskStateChanged),
    #[serde(rename = "task.dep.added")]
    DepAdded(Dep),
    #[serde(rename = "task.dep.removed")]
    DepRemoved(Dep),
    #[serde(rename = "blocker.raised")]
    BlockerRaised(BlockerRaised),
    #[serde(rename = "blocker.resolved")]
    BlockerResolved(BlockerResolved),
    #[serde(rename = "ask.raised")]
    AskRaised(AskRaised),
    #[serde(rename = "msg.sent")]
    MsgSent(MsgSent),
    #[serde(rename = "msg.delivered")]
    MsgDelivered(MsgDelivered),
    #[serde(rename = "handoff")]
    Handoff(Handoff),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentUp {
    pub agent: String,
    pub host: String,
    /// Agent program, e.g. `claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub state: AgentState,
    /// Multiplexer pane currently hosting the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStateChanged {
    pub agent: String,
    pub state: AgentState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDown {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostHealth {
    pub host: String,
    /// Bytes available to unprivileged users on the fleet data volume.
    pub disk_free: u64,
    pub disk_total: u64,
    pub agents: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<HostStats>,
}

/// The host agent's own vitals. Counters run from the host's start, so a drop in `uptime_s`
/// between two reports is a restart.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostStats {
    pub uptime_s: u64,
    /// Peak resident memory of the host process.
    pub max_rss_kb: u64,
    /// User + system CPU time of the host process.
    pub cpu_ms: u64,
    /// Doorbells typed into an agent.
    pub rings: u64,
    /// Rings that failed or were not followed by `working`.
    pub ring_failures: u64,
    /// Failed herdr calls and dropped herdr subscriptions.
    pub herdr_errors: u64,
    /// Events this host could neither send nor spool.
    pub write_failures: u64,
    /// Events this host sent to the spool instead of the hub.
    pub spooled: u64,
}

/// What an agent's processes cost: the pane's whole process tree, sampled by its host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentUsage {
    pub agent: String,
    /// Memory, CPU and process count are measured on Linux only; elsewhere just `cwd` is sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_kb: Option<u64>,
    /// CPU since the previous sample, in percent of one core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procs: Option<u32>,
    /// Where the agent works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// A git worktree on a host, as its last scan found it. Sent when it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeState {
    pub host: String,
    pub path: String,
    /// The main checkout the worktree belongs to.
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub head: String,
    /// Commits on HEAD that no remote has.
    pub unpushed: u32,
    /// Modified and untracked files.
    pub dirty: u32,
    /// Files changed against the default branch, plus dirty ones; at most `MAX_FILES`.
    pub files: Vec<String>,
    pub last_commit: DateTime<Utc>,
    /// Everything this branch changed is already in the default branch (merged, squashed or
    /// never started): it holds no work of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_main: bool,
}

/// Most files a `worktree.state` lists.
pub const MAX_FILES: usize = 200;

/// Processes running in a worktree that no agent's process tree holds: dev servers, databases,
/// test runs, shells. Sent each scan while there are any, then once with `procs: 0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeProcs {
    pub host: String,
    pub path: String,
    pub procs: u32,
    pub rss_kb: u64,
    pub cpu_pct: u32,
    /// Distinct process names, at most 8.
    pub names: Vec<String>,
    /// At most 20.
    pub pids: Vec<u32>,
    /// The directory is gone: nothing that could stop these processes knows they are there.
    pub orphaned: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeGone {
    pub host: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCreated {
    pub title: String,
    /// Where the spec lives (an issue, a doc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskAssigned {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskUnassigned {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskStateChanged {
    pub state: TaskState,
}

/// `task` is blocked by `on`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    pub on: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockerRaised {
    pub what: String,
    /// Who or what has to act: an agent, a resource, an ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unblocks_when: Option<String>,
}

/// Resolves the `blocker.raised` named by the envelope's `re`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockerResolved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskRaised {
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    /// A link to the context needed to answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// A doorbell: a short prompt pointing at an event or a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MsgSent {
    pub to: String,
    /// What the recipient should do, e.g. `REVIEW`, `FYI`, `UNBLOCKED`.
    pub kind: String,
    /// The event or task the message is about.
    #[serde(rename = "ref")]
    pub reference: String,
    pub text: String,
}

/// The `msg.sent` named by `re` reached the agent, which then turned `observed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MsgDelivered {
    pub to: String,
    pub observed: AgentState,
    /// When the ring that worked was typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rung: Option<DateTime<Utc>>,
    /// Rings it took, this one included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub to: String,
    /// The exact next step.
    pub next: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    /// Persistent paths or links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned: Option<String>,
}

impl Known {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::AgentUp(_) => "agent.up",
            Self::AgentState(_) => "agent.state",
            Self::AgentDown(_) => "agent.down",
            Self::HostHealth(_) => "host.health",
            Self::AgentUsage(_) => "agent.usage",
            Self::WorktreeState(_) => "worktree.state",
            Self::WorktreeGone(_) => "worktree.gone",
            Self::WorktreeProcs(_) => "worktree.procs",
            Self::TaskCreated(_) => "task.created",
            Self::TaskAssigned(_) => "task.assigned",
            Self::TaskUnassigned(_) => "task.unassigned",
            Self::TaskState(_) => "task.state",
            Self::DepAdded(_) => "task.dep.added",
            Self::DepRemoved(_) => "task.dep.removed",
            Self::BlockerRaised(_) => "blocker.raised",
            Self::BlockerResolved(_) => "blocker.resolved",
            Self::AskRaised(_) => "ask.raised",
            Self::MsgSent(_) => "msg.sent",
            Self::MsgDelivered(_) => "msg.delivered",
            Self::Handoff(_) => "handoff",
        }
    }

    /// Actor kinds allowed to write this type. `human` appears nowhere: v1 has no human channel.
    pub fn writers(&self) -> &'static [ActorKind] {
        use ActorKind::*;
        match self {
            Self::AgentUp(_)
            | Self::AgentState(_)
            | Self::AgentDown(_)
            | Self::HostHealth(_)
            | Self::AgentUsage(_)
            | Self::WorktreeState(_)
            | Self::WorktreeGone(_)
            | Self::WorktreeProcs(_) => &[Host],
            Self::MsgDelivered(_) => &[Host],
            Self::AskRaised(_) | Self::Handoff(_) => &[Agent],
            _ => &[Agent, Host],
        }
    }

    fn needs_task(&self) -> bool {
        matches!(
            self,
            Self::TaskCreated(_)
                | Self::TaskAssigned(_)
                | Self::TaskUnassigned(_)
                | Self::TaskState(_)
                | Self::DepAdded(_)
                | Self::DepRemoved(_)
                | Self::BlockerRaised(_)
                | Self::AskRaised(_)
                | Self::Handoff(_)
        )
    }

    fn needs_re(&self) -> bool {
        matches!(self, Self::BlockerResolved(_) | Self::MsgDelivered(_))
    }
}

impl Envelope<Known> {
    pub fn new(actor: Actor, project: impl Into<String>, body: Known) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            v: TYPE_VERSION,
            ts: Utc::now(),
            actor,
            project: project.into(),
            task: None,
            re: None,
            body,
        }
    }

    pub fn task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    pub fn re(mut self, re: impl Into<String>) -> Self {
        self.re = Some(re.into());
        self
    }

    pub fn into_event(self) -> Event {
        Envelope {
            id: self.id,
            v: self.v,
            ts: self.ts,
            actor: self.actor,
            project: self.project,
            task: self.task,
            re: self.re,
            body: Body::Known(self.body),
        }
    }
}

/// `agent:<host>/<session>/<name>`, each part non-empty.
pub fn is_agent_id(id: &str) -> bool {
    id.strip_prefix("agent:")
        .map(|rest| rest.splitn(3, '/').collect::<Vec<_>>())
        .is_some_and(|p| p.len() == 3 && p.iter().all(|x| !x.is_empty()))
}

/// Why the hub refuses an event. Maps 1:1 onto an HTTP status.
#[derive(Debug, Clone, PartialEq)]
pub enum Reject {
    BadRequest(String),
    Forbidden(String),
    Conflict(String),
    Internal(String),
}

/// Strict validation for writes: a known type at a known version, no unknown fields, an allowed
/// actor, and the envelope fields the type needs.
pub fn parse_strict(raw: &Value) -> Result<Envelope<Known>, Reject> {
    // Checked before the shape, so a human event is refused as such whatever else is wrong with it.
    if raw.pointer("/actor/kind").and_then(Value::as_str) == Some("human") {
        return Err(Reject::Forbidden(
            "actor.kind=human: v1 has no human channel".into(),
        ));
    }
    // serde cannot deny unknown fields through `flatten`, so the envelope keys are checked here.
    const KEYS: [&str; 9] = [
        "id", "v", "ts", "type", "actor", "project", "task", "re", "data",
    ];
    if let Some(k) = raw
        .as_object()
        .and_then(|o| o.keys().find(|k| !KEYS.contains(&k.as_str())))
    {
        return Err(Reject::BadRequest(format!("unknown envelope field `{k}`")));
    }
    let size = raw.to_string().len();
    if size > MAX_EVENT_BYTES {
        return Err(Reject::BadRequest(format!(
            "event is {size} bytes; the limit is {MAX_EVENT_BYTES} (put large content in a file and link it)"
        )));
    }
    let e: Envelope<Known> =
        serde_json::from_value(raw.clone()).map_err(|err| Reject::BadRequest(err.to_string()))?;
    if ulid::Ulid::from_string(&e.id).is_err() {
        return Err(Reject::BadRequest(format!("id {:?} is not a ULID", e.id)));
    }
    if e.v != TYPE_VERSION {
        return Err(Reject::BadRequest(format!(
            "{} v{} is not supported (this hub speaks v{TYPE_VERSION})",
            e.body.type_name(),
            e.v
        )));
    }
    if !e.body.writers().contains(&e.actor.kind) {
        return Err(Reject::Forbidden(format!(
            "actor.kind={:?} may not write {}",
            e.actor.kind,
            e.body.type_name()
        )));
    }
    let kind = serde_json::to_value(e.actor.kind).unwrap_or_default();
    if !e
        .actor
        .id
        .starts_with(&format!("{}:", kind.as_str().unwrap_or("?")))
    {
        return Err(Reject::BadRequest(format!(
            "actor.id {:?} does not match actor.kind {kind}",
            e.actor.id
        )));
    }
    if e.project.is_empty() || e.actor.id.is_empty() {
        return Err(Reject::BadRequest(
            "project and actor.id must be set".into(),
        ));
    }
    let recipient = match &e.body {
        Known::MsgSent(m) => Some(&m.to),
        Known::Handoff(h) => Some(&h.to),
        Known::TaskAssigned(a) => Some(&a.agent),
        _ => None,
    };
    if let Some(to) = recipient
        && !is_agent_id(to)
    {
        return Err(Reject::BadRequest(format!(
            "{to:?} is not an agent id (agent:<host>/<session>/<name>)"
        )));
    }
    if e.body.needs_task() && e.task.is_none() {
        return Err(Reject::BadRequest(format!(
            "{} needs `task`",
            e.body.type_name()
        )));
    }
    if e.body.needs_re() && e.re.is_none() {
        return Err(Reject::BadRequest(format!(
            "{} needs `re`",
            e.body.type_name()
        )));
    }
    Ok(e)
}

/// The `agent-fleet api schema` document, laid out like herdr's `api schema`.
pub fn api_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "agent-fleet api schema",
        "protocol": PROTOCOL,
        "schema_version": SCHEMA_VERSION,
        "schemas": {
            "event": schemars::schema_for!(Envelope<Known>),
            "state": schemars::schema_for!(State),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Actor {
        Actor {
            kind: ActorKind::Agent,
            id: "agent:h/s/a".into(),
        }
    }

    fn created() -> Value {
        let e = Envelope::new(
            agent(),
            "p",
            Known::TaskCreated(TaskCreated {
                title: "t".into(),
                url: None,
            }),
        )
        .task("task:p/1");
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn a_doorbell_to_a_pane_id_is_refused() {
        let tell = |to: &str| {
            let e = Envelope::new(
                agent(),
                "p",
                Known::MsgSent(MsgSent {
                    to: to.into(),
                    kind: "ANSWER".into(),
                    reference: "x".into(),
                    text: "t".into(),
                }),
            );
            parse_strict(&serde_json::to_value(e).unwrap())
        };
        assert!(
            matches!(tell("wDN:p1"), Err(Reject::BadRequest(ref m)) if m.contains("not an agent id"))
        );
        assert!(tell("agent:h/s").is_err());
        assert!(tell("agent:h/s/b").is_ok());
        assert!(
            tell("agent:h/s/w1:p2").is_ok(),
            "an unnamed agent is named by its pane"
        );
    }

    #[test]
    fn wire_shape_is_envelope_plus_type_and_data() {
        let v = created();
        assert_eq!(v["type"], "task.created");
        assert_eq!(v["data"]["title"], "t");
        assert_eq!(v["v"], 1);
        assert!(parse_strict(&v).is_ok());
    }

    #[test]
    fn unknown_type_is_kept_on_read_and_refused_on_write() {
        let mut v = created();
        v["type"] = "decision.recorded".into();
        v["data"] = json!({"scope": "x"});
        let e: Event = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(e.body, Body::Unknown(_)));
        assert_eq!(serde_json::to_value(&e).unwrap(), v, "round-trips verbatim");
        assert!(matches!(parse_strict(&v), Err(Reject::BadRequest(_))));
    }

    #[test]
    fn unknown_fields_are_refused_on_write() {
        let mut v = created();
        v["data"]["extra"] = 1.into();
        assert!(matches!(parse_strict(&v), Err(Reject::BadRequest(_))));
        let mut v = created();
        v["extra"] = 1.into();
        assert!(matches!(parse_strict(&v), Err(Reject::BadRequest(_))));
    }

    #[test]
    fn actor_rules() {
        let mut v = created();
        v["actor"]["kind"] = "human".into();
        assert!(matches!(parse_strict(&v), Err(Reject::Forbidden(_))));
        let e = Envelope::new(
            agent(),
            "p",
            Known::AgentDown(AgentDown {
                agent: "agent:h/s/a".into(),
                reason: None,
            }),
        );
        let v = serde_json::to_value(e).unwrap();
        assert!(
            matches!(parse_strict(&v), Err(Reject::Forbidden(_))),
            "presence is host-only"
        );
    }

    #[test]
    fn required_envelope_fields() {
        let mut v = created();
        v.as_object_mut().unwrap().remove("task");
        assert!(matches!(parse_strict(&v), Err(Reject::BadRequest(_))));
        let mut v = created();
        v["id"] = "not-a-ulid".into();
        assert!(matches!(parse_strict(&v), Err(Reject::BadRequest(_))));
        let mut v = created();
        v["v"] = 2.into();
        assert!(matches!(parse_strict(&v), Err(Reject::BadRequest(_))));
        let mut v = created();
        v["actor"]["id"] = "host:h".into();
        assert!(
            matches!(parse_strict(&v), Err(Reject::BadRequest(_))),
            "id must match kind"
        );
        let mut v = created();
        v["data"]["title"] = "x".repeat(MAX_EVENT_BYTES).into();
        assert!(
            matches!(parse_strict(&v), Err(Reject::BadRequest(_))),
            "oversized"
        );
    }

    #[test]
    fn committed_schema_is_current() {
        let committed = include_str!("../schema/agent-fleet.schema.json");
        let current = serde_json::to_string_pretty(&api_schema()).unwrap() + "\n";
        assert!(
            committed == current,
            "schema/agent-fleet.schema.json is stale: run `agent-fleet api schema > schema/agent-fleet.schema.json`"
        );
    }
}
