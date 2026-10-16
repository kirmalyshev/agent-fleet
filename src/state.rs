//! `state = fold(events)`. Pure: no I/O, no clock.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::{
    AgentState, Body, Event, Known, TYPE_VERSION, TaskState, WorktreeProcs, WorktreeState,
};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct State {
    /// Id of the last event folded in.
    pub last: Option<String>,
    pub hosts: BTreeMap<String, Host>,
    pub agents: BTreeMap<String, Agent>,
    pub tasks: BTreeMap<String, Task>,
    /// Open blockers by the id of their `blocker.raised`.
    pub blockers: BTreeMap<String, Blocker>,
    /// Open asks by the id of their `ask.raised`.
    pub asks: BTreeMap<String, Ask>,
    /// Undelivered doorbells by the id of their `msg.sent`.
    pub msgs: BTreeMap<String, Msg>,
    /// Git worktrees by `<host>:<path>`.
    pub worktrees: BTreeMap<String, Worktree>,
    /// Processes running in worktrees (or orphaned beside them) by `<host>:<path>`.
    pub worktree_procs: BTreeMap<String, Procs>,
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
    #[serde(default)]
    pub usage: Option<Usage>,
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
    pub task: Option<String>,
    pub by: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub context: Option<String>,
    pub since: DateTime<Utc>,
}

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

    /// The agent holding `task`, if the task is assigned and still open.
    pub fn owner_of(&self, task: &str) -> Option<&str> {
        let t = self.tasks.get(task)?;
        if t.state.is_terminal() {
            return None;
        }
        t.owner.as_deref()
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
                self.agents.insert(
                    d.agent.clone(),
                    Agent {
                        host: d.host.clone(),
                        kind: d.kind.clone(),
                        pane: d.pane.clone(),
                        state: d.state,
                        since: ts,
                        usage: None,
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
            Known::WorktreeGone(d) => {
                self.worktrees.remove(&worktree_key(&d.host, &d.path));
            }
            Known::AgentState(d) => {
                // Only `agent.up` brings an agent in: a late state change never revives a downed one.
                if let Some(a) = self.agents.get_mut(&d.agent)
                    && a.state != d.state
                {
                    a.state = d.state;
                    a.since = ts;
                }
            }
            Known::AgentDown(d) => {
                self.agents.remove(&d.agent);
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
                }
            }
            Known::Handoff(_) => {
                let id = e.id.clone();
                if let Some(t) = self.task_for(e) {
                    t.handoffs.push(id);
                }
            }
        }
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
