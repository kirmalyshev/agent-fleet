//! Idle agents are rung, not just listed: `[idle]` in fleet.toml. The decisions are pure
//! (`due`); `spawn::idle` runs them against the log.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use crate::doctor::{escalate, held, worktree_of};
use crate::event::AgentState;
use crate::spawn::short;
use crate::state::{State, UNMANAGED_KEY};
use crate::ui::ago;

/// `[idle]` in fleet.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Minutes an agent may sit not working before its lead is rung `IDLE`/`HELD`; rung again
    /// every twice that while it stays so.
    pub stale_min: u64,
    /// Retire an agent that reported DONE/MERGED (or whose task ended) and idled `RETIRE_AFTER`.
    pub auto_retire: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            stale_min: 20,
            auto_retire: false,
        }
    }
}

/// A finished agent idle this long is retired when `auto_retire` is on.
pub const RETIRE_AFTER: Duration = Duration::minutes(10);

#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// A tell to `to`: `IDLE`, `HELD` or `UNMANAGED`.
    Ring {
        to: String,
        kind: &'static str,
        text: String,
    },
    /// `retire.requested` for the agent.
    Retire,
}

/// What this host owes now, as `(agent, act)`. `cfg` gives an agent's config (its repo's
/// `[idle]`, else the default).
pub fn due(
    s: &State,
    host: &str,
    now: DateTime<Utc>,
    cfg: &dyn Fn(&str) -> Config,
) -> Vec<(String, Act)> {
    let mut out = Vec::new();
    for (id, a) in &s.agents {
        if a.host != host || a.state == AgentState::Working {
            continue;
        }
        let c = cfg(id);
        let lead = escalate(s, id).map(str::to_string);
        let sp = crate::spawn::find_spawn(s, id);
        if sp.is_none()
            && let Some(to) = &lead
            && !s.idle_rung.contains_key(&format!("{id}{UNMANAGED_KEY}"))
        {
            let wt = worktree_of(s, a)
                .map(|(_, w)| w.state.path.clone())
                .or_else(|| s.first_cwd.get(id).cloned())
                .unwrap_or_else(|| "?".into());
            out.push((
                id.clone(),
                Act::Ring {
                    to: to.clone(),
                    kind: "UNMANAGED",
                    text: format!("{} runs with no spawn record, in {wt}", short(id)),
                },
            ));
        }
        let idle = now - a.since;
        let report = s
            .reports
            .get(id)
            .filter(|r| a.worked.is_none_or(|w| r.since >= w));
        let finished = report.is_some_and(|r| ["DONE", "MERGED"].contains(&r.kind.as_str()))
            || s.tasks
                .values()
                .any(|t| t.owner.as_deref() == Some(id) && t.state.is_terminal());
        if c.auto_retire
            && finished
            && idle >= RETIRE_AFTER
            && sp.is_some()
            && !s
                .retires
                .values()
                .any(|r| r.request.agent == *id && r.since >= a.since)
        {
            out.push((id.clone(), Act::Retire));
            continue;
        }
        let stale = Duration::minutes(c.stale_min as i64);
        let again = s.idle_rung.get(id).is_none_or(|t| now - *t >= stale * 2);
        let Some(to) = lead else { continue };
        if idle < stale || !again {
            continue;
        }
        let name = short(id);
        let (kind, text) = match held(s, id) {
            Some(h) => ("HELD", format!("{name}: {h}")),
            None => {
                let what = report.map_or_else(
                    || {
                        s.tasks
                            .values()
                            .find(|t| t.owner.as_deref() == Some(id) && !t.state.is_terminal())
                            .map_or("no report".to_string(), |t| t.title.clone())
                    },
                    |r| format!("{} {}", r.kind, r.text),
                );
                (
                    "IDLE",
                    format!("{name} {}: {what} — retire?", ago(now, a.since)),
                )
            }
        };
        out.push((id.clone(), Act::Ring { to, kind, text }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Agent, Report};

    fn t0() -> DateTime<Utc> {
        "2026-11-22T10:00:00Z".parse().unwrap()
    }

    fn state() -> State {
        let mut s = State::default();
        for id in ["agent:h/s/a", "agent:h/s/lead"] {
            s.agents.insert(
                id.into(),
                Agent {
                    host: "h".into(),
                    kind: None,
                    pane: None,
                    state: AgentState::Idle,
                    since: t0(),
                    worked: None,
                    finished: None,
                    usage: None,
                },
            );
        }
        s.roles.insert(
            "agent:h/s/a".into(),
            crate::state::Role {
                level: crate::event::Level::Implementer,
                lead: Some("agent:h/s/lead".into()),
                domain: None,
                by: "x".into(),
                since: t0(),
            },
        );
        s
    }

    fn spawned(s: &mut State, id: &str) {
        let req: crate::event::SpawnRequested = serde_json::from_value(serde_json::json!({
            "name": "a", "agent": id, "host": "h", "session": "s", "repo": "o/r",
            "branch": "b", "profile": "p", "brief": "", "why": "t"
        }))
        .unwrap();
        s.spawns.insert(
            "sp".into(),
            crate::state::Spawn {
                request: req,
                by: "agent:h/s/lead".into(),
                project: "p".into(),
                result: None,
                since: t0(),
                answered: None,
            },
        );
    }

    fn acts(s: &State, mins: i64, c: Config) -> Vec<(String, Act)> {
        due(s, "h", t0() + Duration::minutes(mins), &|_| c.clone())
            .into_iter()
            .filter(|(id, _)| id == "agent:h/s/a")
            .collect()
    }

    #[test]
    fn an_agent_idle_past_stale_min_rings_its_lead_then_every_twice_that() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        assert!(acts(&s, 19, Config::default()).is_empty());
        let got = acts(&s, 20, Config::default());
        let Act::Ring { to, kind, text } = &got[0].1 else {
            panic!("{got:?}")
        };
        assert_eq!((to.as_str(), *kind), ("agent:h/s/lead", "IDLE"));
        assert!(text.contains("retire?") && text.starts_with("a "), "{text}");
        // rung at +20: not again until +60
        s.idle_rung
            .insert("agent:h/s/a".into(), t0() + Duration::minutes(20));
        assert!(acts(&s, 59, Config::default()).is_empty());
        assert_eq!(acts(&s, 60, Config::default()).len(), 1);
    }

    #[test]
    fn the_ring_quotes_the_last_report() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.reports.insert(
            "agent:h/s/a".into(),
            Report {
                kind: "OPEN".into(),
                text: "PR 7".into(),
                since: t0(),
            },
        );
        let Act::Ring { text, .. } = &acts(&s, 30, Config::default())[0].1 else {
            panic!()
        };
        assert!(text.contains("OPEN PR 7"), "{text}");
    }

    #[test]
    fn a_working_agent_or_one_with_nobody_to_tell_is_not_rung() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.agents.get_mut("agent:h/s/a").unwrap().state = AgentState::Working;
        assert!(acts(&s, 90, Config::default()).is_empty());
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.roles.clear();
        assert!(acts(&s, 90, Config::default()).is_empty());
    }

    #[test]
    fn a_held_agent_is_rung_as_held_with_the_reason() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        let task = |state| crate::state::Task {
            project: "p".into(),
            title: "T".into(),
            url: None,
            state,
            since: t0(),
            owner: Some("agent:h/s/a".into()),
            lead: None,
            blocked_by: Default::default(),
            handoffs: vec![],
        };
        s.tasks
            .insert("t1".into(), task(crate::event::TaskState::Working));
        s.blockers.insert(
            "b1".into(),
            crate::state::Blocker {
                task: Some("t1".into()),
                what: "login for Kirill".into(),
                owner: None,
                unblocks_when: None,
                pid: None,
                started: None,
                by: "x".into(),
                since: t0(),
            },
        );
        let Act::Ring { kind, text, .. } = &acts(&s, 20, Config::default())[0].1 else {
            panic!()
        };
        assert_eq!(*kind, "HELD");
        assert!(text.contains("login for Kirill"), "{text}");
    }

    #[test]
    fn a_finished_agent_idle_ten_minutes_is_retired_only_when_auto_retire_is_on() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.reports.insert(
            "agent:h/s/a".into(),
            Report {
                kind: "MERGED".into(),
                text: "#5".into(),
                since: t0(),
            },
        );
        let on = Config {
            auto_retire: true,
            ..Config::default()
        };
        assert!(acts(&s, 9, on.clone()).is_empty());
        assert_eq!(acts(&s, 10, on.clone())[0].1, Act::Retire);
        assert!(!matches!(
            acts(&s, 10, Config::default())[..],
            [(_, Act::Retire)]
        ));
        // it worked again after the report: not finished
        s.agents.get_mut("agent:h/s/a").unwrap().worked = Some(t0() + Duration::minutes(1));
        assert!(
            !acts(&s, 30, on.clone())
                .iter()
                .any(|(_, a)| *a == Act::Retire)
        );
    }

    #[test]
    fn a_retire_already_asked_for_this_idle_stretch_is_not_asked_again() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.reports.insert(
            "agent:h/s/a".into(),
            Report {
                kind: "DONE".into(),
                text: "x".into(),
                since: t0(),
            },
        );
        s.retires.insert(
            "r1".into(),
            crate::state::Retire {
                request: crate::event::RetireRequested {
                    agent: "agent:h/s/a".into(),
                    host: "h".into(),
                    decide: None,
                },
                by: "host:h".into(),
                project: "p".into(),
                result: None,
                since: t0() + Duration::minutes(10),
            },
        );
        let on = Config {
            auto_retire: true,
            ..Config::default()
        };
        assert!(!acts(&s, 30, on).iter().any(|(_, a)| *a == Act::Retire));
    }

    #[test]
    fn an_agent_without_a_spawn_record_rings_unmanaged_once_and_is_never_retired() {
        let mut s = state();
        s.first_cwd.insert("agent:h/s/a".into(), "/w/x".into());
        s.reports.insert(
            "agent:h/s/a".into(),
            Report {
                kind: "DONE".into(),
                text: "x".into(),
                since: t0(),
            },
        );
        let on = Config {
            auto_retire: true,
            ..Config::default()
        };
        let got = acts(&s, 12, on.clone());
        assert_eq!(got.len(), 1, "{got:?}");
        let Act::Ring { kind, text, .. } = &got[0].1 else {
            panic!()
        };
        assert_eq!(*kind, "UNMANAGED");
        assert!(text.contains("/w/x"), "{text}");
        s.idle_rung
            .insert(format!("agent:h/s/a{UNMANAGED_KEY}"), t0());
        assert!(acts(&s, 12, on).is_empty());
    }
}
