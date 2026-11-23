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
    /// every twice that while it stays so (`HELD` with unchanged text backs off, see `HELD_CAP`).
    pub stale_min: u64,
    /// Retire an agent that reported DONE/MERGED (or whose task ended) and idled `RETIRE_AFTER`.
    pub auto_retire: bool,
    /// Agents never rung or retired: a name, or the basename of the worktree they work in.
    pub exempt: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            stale_min: 20,
            auto_retire: false,
            exempt: Vec::new(),
        }
    }
}

/// A finished agent idle this long is retired when `auto_retire` is on.
pub const RETIRE_AFTER: Duration = Duration::minutes(10);

/// The longest gap between two identical `HELD` rings.
pub const HELD_CAP: Duration = Duration::hours(8);

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
        let wt = worktree_of(s, a)
            .map(|(_, w)| w.state.path.clone())
            .or_else(|| s.first_cwd.get(id).cloned());
        let base = wt
            .as_deref()
            .and_then(|p| p.trim_end_matches('/').rsplit('/').next());
        if c.exempt
            .iter()
            .any(|e| e == short(id) || Some(e.as_str()) == base)
        {
            continue;
        }
        let lead = escalate(s, id).map(str::to_string);
        let sp = crate::spawn::find_spawn(s, id);
        if sp.is_none()
            && let Some(to) = &lead
            && !s.idle_rung.contains_key(&format!("{id}{UNMANAGED_KEY}"))
        {
            let wt = wt.clone().unwrap_or_else(|| "?".into());
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
        let mine: Vec<_> = s
            .tasks
            .values()
            .filter(|t| t.owner.as_deref() == Some(id))
            .collect();
        // Every task it owns ended, or it reported DONE/MERGED since it last worked.
        let finished = report.is_some_and(|r| ["DONE", "MERGED"].contains(&r.kind.as_str()))
            || (!mine.is_empty() && mine.iter().all(|t| t.state.is_terminal()));
        // A retire is asked once per stretch since it last worked, not per state flip.
        let since = a.worked.or(report.map(|r| r.since)).unwrap_or(a.since);
        if c.auto_retire && finished && idle >= RETIRE_AFTER && sp.is_some() {
            let asked: Vec<_> = s
                .retires
                .values()
                .filter(|r| r.request.agent == *id && r.since >= since)
                .collect();
            // Asked once per idle stretch; while it still runs, ring nobody either.
            if asked.is_empty() {
                out.push((id.clone(), Act::Retire));
            }
            if asked.is_empty() || asked.iter().any(|r| r.result.is_none()) {
                continue;
            }
        }
        // ponytail: 0 would ring every tick; 1 min is the floor.
        let stale = Duration::minutes(c.stale_min.clamp(1, 1_000_000) as i64);
        let Some(to) = lead else { continue };
        if idle < stale {
            continue;
        }
        let name = short(id);
        let held = held(s, id);
        // A wait that does not change is not news: the same HELD text backs off, 2x, 4x, 8x the
        // base gap, up to `HELD_CAP`. New text, or the agent working, starts over.
        let gap = match (&held, s.held_streak.get(id)) {
            (Some(h), Some((t, n))) if *t == format!("{name}: {h}") => {
                (stale * 2 * 2i32.saturating_pow((*n - 1).min(16))).min(HELD_CAP)
            }
            _ => stale * 2,
        };
        if s.idle_rung.get(id).is_some_and(|t| now - *t < gap) {
            continue;
        }
        let (kind, text) = match held {
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
                what: "login for the operator".into(),
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
        assert!(text.contains("login for the operator"), "{text}");
    }

    #[test]
    fn an_unchanged_held_backs_off_to_the_cap_and_new_text_starts_over() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.tasks.insert(
            "t1".into(),
            crate::state::Task {
                project: "p".into(),
                title: "T".into(),
                url: None,
                state: crate::event::TaskState::Working,
                since: t0(),
                owner: Some("agent:h/s/a".into()),
                lead: None,
                blocked_by: Default::default(),
                handoffs: vec![],
            },
        );
        let blocker = |what: &str| crate::state::Blocker {
            task: Some("t1".into()),
            what: what.into(),
            owner: None,
            unblocks_when: None,
            pid: None,
            started: None,
            by: "x".into(),
            since: t0(),
        };
        s.blockers
            .insert("b1".into(), blocker("waits the operator"));
        let held_text = "a: held: waits the operator";
        // Ring n at `at`; the next is due `gap` later, not a minute before.
        let mut at = 20;
        for (n, gap) in [(1, 40), (2, 80), (3, 160), (4, 320), (5, 480), (6, 480)] {
            s.idle_rung
                .insert("agent:h/s/a".into(), t0() + Duration::minutes(at));
            s.held_streak
                .insert("agent:h/s/a".into(), (held_text.into(), n));
            assert!(
                acts(&s, at + gap - 1, Config::default()).is_empty(),
                "n={n}"
            );
            assert_eq!(acts(&s, at + gap, Config::default()).len(), 1, "n={n}");
            at += gap;
        }
        // The blocker text changes: back to the base gap.
        s.blockers.insert("b1".into(), blocker("waits for the key"));
        s.idle_rung
            .insert("agent:h/s/a".into(), t0() + Duration::minutes(at));
        assert_eq!(acts(&s, at + 40, Config::default()).len(), 1);
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
    fn a_terminal_task_finishes_an_agent_and_a_live_one_does_not() {
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
        let on = Config {
            auto_retire: true,
            ..Config::default()
        };
        s.tasks
            .insert("t".into(), task(crate::event::TaskState::Working));
        assert!(
            !acts(&s, 30, on.clone())
                .iter()
                .any(|(_, a)| *a == Act::Retire)
        );
        s.tasks
            .insert("t".into(), task(crate::event::TaskState::Completed));
        assert_eq!(acts(&s, 30, on)[0].1, Act::Retire);
    }

    #[test]
    fn an_exempt_name_or_worktree_is_never_rung_retired_or_called_unmanaged() {
        let mut s = state();
        s.first_cwd
            .insert("agent:h/s/a".into(), "/w/asa-ads".into());
        s.reports.insert(
            "agent:h/s/a".into(),
            Report {
                kind: "DONE".into(),
                text: "x".into(),
                since: t0(),
            },
        );
        for e in ["a", "asa-ads"] {
            let c = Config {
                auto_retire: true,
                exempt: vec![e.into()],
                ..Config::default()
            };
            assert!(acts(&s, 90, c).is_empty(), "{e}");
        }
        // hand-started, not exempt: still called out
        let c = Config {
            exempt: vec!["other".into()],
            ..Config::default()
        };
        assert!(!acts(&s, 90, c).is_empty());
        // spawned and exempt by name
        spawned(&mut s, "agent:h/s/a");
        let c = Config {
            auto_retire: true,
            exempt: vec!["a".into()],
            ..Config::default()
        };
        assert!(acts(&s, 90, c).is_empty());
    }

    #[test]
    fn stale_min_zero_is_one_minute_not_a_ring_per_tick() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.idle_rung
            .insert("agent:h/s/a".into(), t0() + Duration::minutes(5));
        let zero = Config {
            stale_min: 0,
            ..Config::default()
        };
        assert!(acts(&s, 5, zero.clone()).is_empty());
        assert_eq!(acts(&s, 7, zero).len(), 1);
    }

    #[test]
    fn a_state_flip_does_not_ask_for_another_retire() {
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
                result: Some(crate::event::RetireResult {
                    agent: "agent:h/s/a".into(),
                    ok: false,
                    text: "dirty".into(),
                }),
                since: t0() + Duration::minutes(10),
            },
        );
        // idle -> done at +15: a.since moves past the retire, the refusal still stands
        s.agents.get_mut("agent:h/s/a").unwrap().since = t0() + Duration::minutes(15);
        let on = Config {
            auto_retire: true,
            ..Config::default()
        };
        assert!(!acts(&s, 40, on).iter().any(|(_, a)| *a == Act::Retire));
    }

    #[test]
    fn with_no_lead_the_root_coordinator_is_rung() {
        let mut s = state();
        spawned(&mut s, "agent:h/s/a");
        s.roles.clear();
        s.roles.insert(
            "agent:h/s/lead".into(),
            crate::state::Role {
                level: crate::event::Level::RootCoordinator,
                lead: None,
                domain: None,
                by: "x".into(),
                since: t0(),
            },
        );
        let Act::Ring { to, .. } = &acts(&s, 30, Config::default())[0].1 else {
            panic!()
        };
        assert_eq!(to, "agent:h/s/lead");
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
