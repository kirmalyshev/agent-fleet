//! The per-profile scorecard: speed, quality and predictability of each routing profile (or agent
//! program), read off the event log. Pure, like the reducer: no I/O, no clock.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::event::{ActorKind, Body, Event, Known};
use crate::stats::Dist;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum By {
    Profile,
    Kind,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Row {
    pub key: String,
    /// Spawns that started.
    pub n: usize,
    pub spawn_failures: usize,
    /// Spawns with neither a result nor an `agent.up`: left out of every figure below.
    pub unconfirmed: usize,
    /// Minutes from spawn to the first `OPEN` report.
    pub to_pr_min: Dist,
    /// Minutes from spawn to the first `MERGED` or `DONE` report.
    pub to_done_min: Dist,
    /// `FIX` reports per spawn: review rounds after the first push.
    pub fix_rounds: f64,
    /// Failed merge requests per finished spawn.
    pub gate_reds: Option<f64>,
    /// A newer spawn on the same issue within 7 days of a finish.
    pub fixups: usize,
    /// `BLOCKED` reports.
    pub blocks: usize,
    /// Percent of spawns not handed off to another agent.
    pub no_handoff_pct: f64,
    /// IDLE, STALLED, HUNG, stuck and UNREAD events per agent-hour; `None` with no hours.
    pub interventions_per_hour: Option<f64>,
    /// Standard deviation of the minutes to done; `None` under two finished spawns.
    pub done_sd_min: Option<f64>,
    /// Mean quota points the program's busiest window rose over the agent's life; `None` unknown.
    pub quota_pp: Option<f64>,
    /// Agents the quota figure is a mean over.
    pub quota_n: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Scorecard {
    pub window_s: i64,
    pub by: String,
    pub rows: Vec<Row>,
}

/// A newer spawn on the same issue within this long of a finish counts as its fix-up.
const FIXUP_WINDOW: Duration = Duration::days(7);

/// One spawn, as the log tells it.
struct Run {
    agent: String,
    profile: String,
    kind: Option<String>,
    host: String,
    repo: String,
    issue: Option<u64>,
    spawned: DateTime<Utc>,
    failed: bool,
    confirmed: bool,
    up: Option<DateTime<Utc>>,
    down: Option<DateTime<Utc>>,
    first_pr: Option<DateTime<Utc>>,
    done: Option<DateTime<Utc>>,
    fixes: usize,
    blocks: usize,
    gate_reds: usize,
    handed_off: bool,
    interventions: usize,
}

/// Time-ordered `(read at, used percent)` samples.
type Series = Vec<(DateTime<Utc>, f64)>;

/// PR numbers a tell names: `#12`, `PR 12`, `pull/12`.
fn pr_numbers(text: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for marker in ["#", "PR ", "pull/"] {
        for part in text.split(marker).skip(1) {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse() {
                out.push(n);
            }
        }
    }
    out
}

fn mins(a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    (b - a).num_seconds().max(0) as f64 / 60.0
}

pub fn compute(events: &[Event], now: DateTime<Utc>, window: Duration, by: By) -> Scorecard {
    let mut runs: Vec<Run> = Vec::new();
    let mut by_agent: HashMap<String, usize> = HashMap::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut tell_to: HashMap<&str, &str> = HashMap::new();
    let mut merges: HashMap<&str, usize> = HashMap::new();
    // (repo, PR number) -> the run whose tells name that PR, last one wins.
    let mut pr_runs: HashMap<(String, u64), usize> = HashMap::new();
    // (host, program) -> (time, busiest window's used percent), in log order.
    let mut quota: HashMap<(&str, &str), Series> = HashMap::new();

    for e in events.iter().filter(|e| e.ts <= now) {
        let Body::Known(k) = &e.body else { continue };
        let from = e.actor.id.as_str();
        let own = |m: &HashMap<String, usize>| m.get(from).copied();
        let resolve = |id: &str| {
            by_agent
                .get(id)
                .or_else(|| by_name.get(id.rsplit('/').next().unwrap_or(id)))
                .copied()
        };
        match k {
            Known::SpawnRequested(s) => {
                if let Some(i) = s.replaces.as_deref().and_then(resolve) {
                    runs[i].handed_off = true;
                }
                by_agent.insert(s.agent.clone(), runs.len());
                by_name.insert(s.name.clone(), runs.len());
                runs.push(Run {
                    agent: s.agent.clone(),
                    profile: s.profile.clone(),
                    kind: None,
                    host: s.host.clone(),
                    repo: s.repo.clone(),
                    issue: s.issue,
                    spawned: e.ts,
                    failed: false,
                    confirmed: false,
                    up: None,
                    down: None,
                    first_pr: None,
                    done: None,
                    fixes: 0,
                    blocks: 0,
                    gate_reds: 0,
                    handed_off: false,
                    interventions: 0,
                });
            }
            Known::SpawnResult(r) => {
                if let Some(i) = resolve(&r.agent) {
                    runs[i].failed = !r.ok;
                    runs[i].confirmed = true;
                }
            }
            Known::AgentUp(u) => {
                if let Some(i) = resolve(&u.agent) {
                    runs[i].kind = u.kind.clone().or(runs[i].kind.take());
                    runs[i].up.get_or_insert(e.ts);
                    runs[i].down = None;
                    runs[i].confirmed = true;
                }
            }
            Known::AgentDown(d) => {
                if let Some(i) = resolve(&d.agent) {
                    runs[i].down = Some(e.ts);
                }
            }
            Known::AgentStuck(s) if s.cause.is_some() => {
                if let Some(i) = resolve(&s.agent) {
                    runs[i].interventions += 1;
                }
            }
            Known::ProcessHung(h) => {
                if let Some(i) = h.agent.as_deref().and_then(resolve) {
                    runs[i].interventions += 1;
                }
            }
            Known::Handoff(_) => {
                if let Some(i) = own(&by_agent) {
                    runs[i].handed_off = true;
                }
            }
            // The lead usually merges, so the PR is traced to the agent whose tells name it.
            Known::MergeRequested(m) => {
                if let Some(i) = pr_runs
                    .get(&(m.repo.clone(), m.pr))
                    .copied()
                    .or(own(&by_agent))
                {
                    merges.insert(&e.id, i);
                }
            }
            Known::MergeResult(r) if !r.ok => {
                if let Some(i) = e.re.as_deref().and_then(|re| merges.get(re)) {
                    runs[*i].gate_reds += 1;
                }
            }
            Known::AgentQuota(q) => {
                if let Some(used) = q.windows.iter().map(|w| w.used_pct).reduce(f64::max) {
                    quota
                        .entry((&q.host, &q.kind))
                        .or_default()
                        .push((e.ts, used));
                }
            }
            Known::MsgSent(m) => {
                tell_to.insert(&e.id, &m.to);
                if e.actor.kind == ActorKind::Agent {
                    if let Some(i) = own(&by_agent) {
                        let r = &mut runs[i];
                        for n in pr_numbers(&format!("{} {}", m.reference, m.text)) {
                            pr_runs.insert((r.repo.clone(), n), i);
                        }
                        match m.kind.as_str() {
                            "OPEN" => {
                                r.first_pr.get_or_insert(e.ts);
                            }
                            "MERGED" | "DONE" => {
                                r.done.get_or_insert(e.ts);
                            }
                            "FIX" => r.fixes += 1,
                            "BLOCKED" => r.blocks += 1,
                            _ => {}
                        }
                    }
                } else if e.actor.kind == ActorKind::Host {
                    let target = match m.kind.as_str() {
                        "IDLE" | "STALLED" => resolve(&m.reference),
                        "UNREAD" => {
                            e.re.as_deref()
                                .and_then(|re| tell_to.get(re))
                                .and_then(|to| resolve(to))
                        }
                        _ => None,
                    };
                    if let Some(i) = target {
                        runs[i].interventions += 1;
                    }
                }
            }
            _ => {}
        }
    }

    let since = now - window;
    let fixups: Vec<usize> = runs
        .iter()
        .map(|r| {
            let Some(done) = r.done else { return 0 };
            runs.iter()
                .filter(|s| {
                    s.agent != r.agent
                        && !s.failed
                        && s.repo == r.repo
                        && s.issue.is_some()
                        && s.issue == r.issue
                        && s.spawned > done
                        && s.spawned - done <= FIXUP_WINDOW
                })
                .count()
        })
        .collect();

    let quota_at = |host: &str, kind: &str, t: DateTime<Utc>| {
        quota
            .get(&(host, kind))?
            .iter()
            .rev()
            .find(|(at, _)| *at <= t)
            .map(|(_, v)| *v)
    };

    let mut groups: BTreeMap<String, Vec<(usize, &Run)>> = BTreeMap::new();
    for (i, r) in runs.iter().enumerate().filter(|(_, r)| r.spawned >= since) {
        let key = match by {
            By::Profile => r.profile.clone(),
            By::Kind => r
                .kind
                .clone()
                .unwrap_or_else(|| format!("{} (never up)", r.profile)),
        };
        groups.entry(key).or_default().push((i, r));
    }

    let rows = groups
        .into_iter()
        .map(|(key, g)| {
            let started: Vec<_> = g.iter().filter(|(_, r)| r.confirmed && !r.failed).collect();
            let n = started.len();
            let to_done: Vec<f64> = started
                .iter()
                .filter_map(|(_, r)| r.done.map(|d| mins(r.spawned, d)))
                .collect();
            let dist = |f: fn(&Run) -> Option<DateTime<Utc>>| {
                Dist::of(
                    started
                        .iter()
                        .filter_map(|(_, r)| f(r).map(|t| mins(r.spawned, t).round() as u64))
                        .collect(),
                )
            };
            let per = |x: usize, d: usize| if d == 0 { 0.0 } else { x as f64 / d as f64 };
            let hours: f64 = started
                .iter()
                .map(|(_, r)| {
                    let end = r.down.or(r.done).unwrap_or(now);
                    mins(r.up.unwrap_or(r.spawned), end) / 60.0
                })
                .sum();
            let mean = per_f(&to_done);
            let rises: Vec<f64> = started
                .iter()
                .filter_map(|(_, r)| {
                    let kind = r.kind.as_deref()?;
                    let end = r.down.or(r.done).unwrap_or(now);
                    let d = quota_at(&r.host, kind, end)? - quota_at(&r.host, kind, r.spawned)?;
                    (d >= 0.0).then_some(d)
                })
                .collect();
            Row {
                key,
                n,
                spawn_failures: g.iter().filter(|(_, r)| r.failed).count(),
                unconfirmed: g.iter().filter(|(_, r)| !r.confirmed).count(),
                to_pr_min: dist(|r| r.first_pr),
                to_done_min: dist(|r| r.done),
                fix_rounds: per(started.iter().map(|(_, r)| r.fixes).sum(), n),
                gate_reds: (!to_done.is_empty()).then(|| {
                    per(
                        started.iter().map(|(_, r)| r.gate_reds).sum(),
                        to_done.len(),
                    )
                }),
                fixups: started.iter().map(|(i, _)| fixups[*i]).sum(),
                blocks: started.iter().map(|(_, r)| r.blocks).sum(),
                no_handoff_pct: 100.0
                    * per(started.iter().filter(|(_, r)| !r.handed_off).count(), n),
                interventions_per_hour: (hours > 0.0).then(|| {
                    started.iter().map(|(_, r)| r.interventions).sum::<usize>() as f64 / hours
                }),
                done_sd_min: (to_done.len() >= 2).then(|| {
                    (to_done.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / to_done.len() as f64)
                        .sqrt()
                }),
                quota_pp: (!rises.is_empty()).then(|| per_f(&rises)),
                quota_n: rises.len(),
            }
        })
        .collect();

    Scorecard {
        window_s: window.num_seconds(),
        by: match by {
            By::Profile => "profile",
            By::Kind => "kind",
        }
        .into(),
        rows,
    }
}

fn per_f(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// The table `agent-fleet stats` prints: one row per key, then what each column means.
pub fn render(s: &Scorecard) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let days = s.window_s as f64 / 86400.0;
    let _ = writeln!(out, "scorecard by {}, last {days:.0}d", s.by);
    if s.rows.is_empty() {
        out.push_str("no spawns in the window\n");
    }
    for r in &s.rows {
        let opt = |v: Option<f64>, f: fn(f64) -> String| v.map_or("?".into(), f);
        let q = r
            .quota_pp
            .map_or("?".into(), |q| format!("{q:.0}pp (n={})", r.quota_n));
        let _ = writeln!(
            out,
            "\n{}  n={}  spawn failures {}  unconfirmed {}",
            r.key, r.n, r.spawn_failures, r.unconfirmed
        );
        let _ = writeln!(
            out,
            "  speed        to PR p50 {}m p90 {}m (n={}) | to done p50 {}m p90 {}m (n={})",
            r.to_pr_min.p50,
            r.to_pr_min.p90,
            r.to_pr_min.n,
            r.to_done_min.p50,
            r.to_done_min.p90,
            r.to_done_min.n
        );
        let _ = writeln!(
            out,
            "  quality      fix rounds {:.1} | gate reds/merge {} | fix-up PRs {} | BLOCKED {}",
            r.fix_rounds,
            opt(r.gate_reds, |v| format!("{v:.2}")),
            r.fixups,
            r.blocks
        );
        let _ = writeln!(
            out,
            "  predictable  no handoff {:.0}% | interventions/h {} | done sd {}m | quota {q}",
            r.no_handoff_pct,
            opt(r.interventions_per_hour, |v| format!("{v:.2}")),
            opt(r.done_sd_min, |v| format!("{v:.0}"))
        );
    }
    out.push_str(
        "\nlegend: speed = minutes from spawn to the first OPEN / MERGED|DONE tell. fix rounds = FIX tells per\n\
         spawn (a proxy: PR verdicts are not in the log). BLOCKED = BLOCKED tells. gate reds = failed merge requests per finished spawn. fix-up = a newer spawn on the same issue\n\
         within 7d of a finish. interventions = IDLE, STALLED, UNREAD, HUNG and stuck events per agent-hour.\n\
         quota = points the program's busiest quota window rose over the agent's life (shared by concurrent\n\
         agents, a rough proxy; ? = no data). Failed and unconfirmed spawns (no result, never up) count only in their own columns.\n\
         Under --by kind a spawn that never came up has no program: it shows as `<profile> (never up)`. Tells name\n\
         an agent by short name when a host sends them, so a reused name takes the newest spawn.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const T0: &str = "2026-11-20T10:00:00Z";

    fn at(mins: i64) -> DateTime<Utc> {
        T0.parse::<DateTime<Utc>>().unwrap() + Duration::minutes(mins)
    }

    fn ev(id: &str, mins: i64, actor: &str, ty: &str, data: Value) -> Event {
        let kind = actor.split(':').next().unwrap();
        serde_json::from_value(json!({
            "id": id, "v": 1, "ts": at(mins), "project": "p",
            "actor": {"kind": kind, "id": actor},
            "type": ty, "data": data,
        }))
        .unwrap()
    }

    fn evre(id: &str, re: &str, mins: i64, actor: &str, ty: &str, data: Value) -> Event {
        let mut e = ev(id, mins, actor, ty, data);
        e.re = Some(re.into());
        e
    }

    fn spawn(id: &str, mins: i64, name: &str, profile: &str, issue: u64) -> Event {
        ev(
            id,
            mins,
            "agent:h/s/lead",
            "spawn.requested",
            json!({"name": name, "agent": format!("agent:h/s/{name}"), "host": "h", "session": "s",
                   "repo": "o/r", "issue": issue, "branch": name, "profile": profile,
                   "brief": "b", "why": "w"}),
        )
    }

    fn tell(id: &str, mins: i64, from: &str, kind: &str) -> Event {
        ev(
            id,
            mins,
            from,
            "msg.sent",
            json!({"to": "coordinator", "kind": kind, "ref": "r", "text": "t"}),
        )
    }

    fn up(id: &str, mins: i64, name: &str, kind: &str) -> Event {
        ev(
            id,
            mins,
            "host:h",
            "agent.up",
            json!({"agent": format!("agent:h/s/{name}"), "host": "h", "kind": kind, "state": "working"}),
        )
    }

    fn down(id: &str, mins: i64, name: &str) -> Event {
        ev(
            id,
            mins,
            "host:h",
            "agent.down",
            json!({"agent": format!("agent:h/s/{name}")}),
        )
    }

    fn run(events: &[Event], by: By) -> Scorecard {
        compute(events, at(24 * 60), Duration::days(14), by)
    }

    fn row<'a>(s: &'a Scorecard, key: &str) -> &'a Row {
        s.rows
            .iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("no row {key}: {s:?}"))
    }

    /// Two sonnet spawns and one devin: the fixture every test reads.
    fn fixture() -> Vec<Event> {
        let a = "agent:h/s/a";
        let b = "agent:h/s/b";
        vec![
            spawn("01", 0, "a", "sonnet", 1),
            ev(
                "02",
                0,
                "host:h",
                "spawn.result",
                json!({"agent": a, "ok": true}),
            ),
            up("03", 1, "a", "claude"),
            tell("04", 30, a, "OPEN"),
            tell("05", 50, a, "FIX"),
            tell("06", 60, a, "FIX"),
            ev(
                "07",
                61,
                a,
                "merge.requested",
                json!({"repo": "o/r", "pr": 9, "sha": "abcdef1", "host": "h"}),
            ),
            evre(
                "08",
                "07",
                62,
                "host:h",
                "merge.result",
                json!({"ok": false, "text": "red"}),
            ),
            tell("09", 90, a, "MERGED"),
            down("10", 95, "a"),
            spawn("11", 10, "b", "sonnet", 2),
            ev(
                "12",
                10,
                "host:h",
                "spawn.result",
                json!({"agent": b, "ok": true}),
            ),
            up("13", 11, "b", "claude"),
            tell("14", 20, b, "OPEN"),
            tell("15", 40, b, "DONE"),
            ev(
                "16",
                41,
                b,
                "handoff",
                json!({"to": "agent:h/s/z", "next": "n"}),
            ),
            down("17", 71, "b"),
            spawn("18", 100, "c", "devin", 1),
            ev(
                "19",
                100,
                "host:h",
                "spawn.result",
                json!({"agent": "agent:h/s/c", "ok": false, "error": "boom"}),
            ),
        ]
    }

    #[test]
    fn rows_split_by_profile_and_count_spawns_and_failures() {
        let s = run(&fixture(), By::Profile);
        assert_eq!(s.rows.len(), 2);
        assert_eq!(row(&s, "sonnet").n, 2);
        let d = row(&s, "devin");
        assert_eq!((d.n, d.spawn_failures), (0, 1));
    }

    #[test]
    fn speed_is_minutes_from_spawn_to_first_pr_and_to_done() {
        let r = row(&run(&fixture(), By::Profile), "sonnet").clone();
        // a: pr at 30, done at 90; b: pr at 10 (20-10), done at 30 (40-10)
        assert_eq!(
            (r.to_pr_min.n, r.to_pr_min.p50, r.to_pr_min.max),
            (2, 30, 30)
        ); // upper median
        assert_eq!(
            (r.to_done_min.n, r.to_done_min.p50, r.to_done_min.max),
            (2, 90, 90)
        );
    }

    #[test]
    fn quality_counts_fix_rounds_gate_reds_and_blocks() {
        let mut ev = fixture();
        ev.push(tell("20", 70, "agent:h/s/a", "BLOCKED"));
        let r = row(&run(&ev, By::Profile), "sonnet").clone();
        assert_eq!(r.fix_rounds, 1.0); // 2 FIX over 2 spawns
        assert_eq!(r.gate_reds, Some(0.5)); // 1 red over 2 finished
        assert_eq!(r.blocks, 1);
    }

    #[test]
    fn a_new_spawn_on_the_same_issue_within_a_week_is_a_fixup() {
        let mut ev = fixture();
        // `a` (issue 1) finished at minute 90; a spawn on issue 1 two days later is its fix-up
        ev.push(spawn("21", 90 + 2 * 24 * 60, "d", "devin", 1));
        let s = compute(&ev, at(10 * 24 * 60), Duration::days(14), By::Profile);
        assert_eq!(row(&s, "sonnet").fixups, 1);
        // a spawn on the same issue 8 days after the finish is new work, not a fix-up
        let mut late = fixture();
        late.push(spawn("22", 90 + 8 * 24 * 60, "e", "devin", 1));
        let s = compute(&late, at(20 * 24 * 60), Duration::days(30), By::Profile);
        assert_eq!(row(&s, "sonnet").fixups, 0);
    }

    #[test]
    fn handoff_by_event_or_replacing_spawn_is_not_a_clean_finish() {
        let r = row(&run(&fixture(), By::Profile), "sonnet").clone();
        assert_eq!(r.no_handoff_pct, 50.0); // b handed off
        let mut ev = fixture();
        let mut rep = spawn("23", 80, "g", "opus", 3);
        if let crate::event::Body::Known(crate::event::Known::SpawnRequested(s)) = &mut rep.body {
            s.replaces = Some("agent:h/s/a".into());
        }
        ev.push(rep);
        assert_eq!(row(&run(&ev, By::Profile), "sonnet").no_handoff_pct, 0.0);
    }

    #[test]
    fn interventions_per_agent_hour_count_idle_stalled_hung_stuck_and_unread() {
        let mut log = fixture();
        log.push(ev_host_tell("30", 40, "IDLE", "agent:h/s/a"));
        log.push(ev_host_tell("31", 41, "STALLED", "a")); // short name
        log.push(ev_hung("32", 42, "agent:h/s/a"));
        log.push(ev(
            "33",
            43,
            "host:h",
            "agent.stuck",
            json!({"agent": "agent:h/s/a", "cause": "quota", "detail": "d"}),
        ));
        // a tell to `a` handed back unread
        log.push(ev(
            "34",
            44,
            "agent:h/s/lead",
            "msg.sent",
            json!({"to": "agent:h/s/a", "kind": "REVIEW", "ref": "r", "text": "t"}),
        ));
        log.push(evre(
            "35",
            "34",
            55,
            "host:h",
            "msg.sent",
            json!({"to": "agent:h/s/lead", "kind": "UNREAD", "ref": "r", "text": "t"}),
        ));
        // a lived minute 1..95 = 94 min; b minute 11..71 = 60 min; 5 events over 154 min
        let r = row(&run(&log, By::Profile), "sonnet").clone();
        assert!(
            (r.interventions_per_hour.unwrap() - 5.0 / (154.0 / 60.0)).abs() < 1e-6,
            "{r:?}"
        );
    }

    fn ev_host_tell(id: &str, mins: i64, kind: &str, reference: &str) -> Event {
        ev(
            id,
            mins,
            "host:h",
            "msg.sent",
            json!({"to": "agent:h/s/lead", "kind": kind, "ref": reference, "text": "t"}),
        )
    }

    fn ev_hung(id: &str, mins: i64, agent: &str) -> Event {
        ev(
            id,
            mins,
            "host:h",
            "proc.hung",
            json!({"host": "h", "pid": 1, "agent": agent, "since": at(mins), "command": "c"}),
        )
    }

    #[test]
    fn time_to_done_spread_is_a_standard_deviation() {
        let r = row(&run(&fixture(), By::Profile), "sonnet").clone();
        assert!((r.done_sd_min.unwrap() - 30.0).abs() < 1e-6, "{r:?}"); // 90 and 30: mean 60, sd 30
    }

    #[test]
    fn by_kind_groups_on_the_program_and_unknown_spawns_fall_back_to_the_profile() {
        let s = run(&fixture(), By::Kind);
        assert_eq!(row(&s, "claude").n, 2);
        assert_eq!(s.by, "kind");
        // the failed devin spawn never came up, so it has no program
        assert_eq!(row(&s, "devin (never up)").spawn_failures, 1);
    }

    #[test]
    fn spawns_before_the_window_are_left_out() {
        let s = compute(
            &fixture(),
            at(40 * 24 * 60),
            Duration::days(14),
            By::Profile,
        );
        assert!(s.rows.is_empty(), "{s:?}");
    }

    #[test]
    fn quota_cost_is_the_rise_of_the_programs_window_over_the_agents_life() {
        let mut ev = fixture();
        let q = |id: &str, mins: i64, pct: f64| ev_quota(id, mins, pct);
        ev.push(q("40", 0, 10.0));
        ev.push(q("41", 95, 30.0)); // a: 10 -> 30
        let r = row(&run(&ev, By::Profile), "sonnet").clone();
        assert_eq!(r.quota_pp, Some(10.0)); // a: 10 -> 30, b: 10 -> 10 (no newer sample)
    }

    fn ev_quota(id: &str, mins: i64, pct: f64) -> Event {
        ev(
            id,
            mins,
            "host:h",
            "agent.quota",
            json!({
            "host": "h", "kind": "claude", "read_at": at(mins),
            "windows": [{"name": "5h", "used_pct": pct}]}),
        )
    }

    #[test]
    fn a_merge_the_lead_requests_is_traced_to_the_agent_whose_tells_name_the_pr() {
        let mut log = fixture();
        // b's tell names PR 5; the lead (not b) asks for the merge, and it fails
        log.push(ev(
            "50",
            42,
            "agent:h/s/b",
            "msg.sent",
            json!({"to": "coordinator", "kind": "READY", "ref": "o/r#5", "text": "PR 5 ready"}),
        ));
        log.push(ev(
            "51",
            43,
            "agent:h/s/lead",
            "merge.requested",
            json!({"repo": "o/r", "pr": 5, "sha": "abcdef1", "host": "h"}),
        ));
        log.push(evre(
            "52",
            "51",
            44,
            "host:h",
            "merge.result",
            json!({"ok": false, "text": "red"}),
        ));
        let r = row(&run(&log, By::Profile), "sonnet").clone();
        assert_eq!(r.gate_reds, Some(1.0)); // a's red + b's red over 2 finished
    }

    #[test]
    fn no_data_is_none_not_zero() {
        let log = vec![
            spawn("01", 0, "a", "sonnet", 1),
            ev(
                "02",
                0,
                "host:h",
                "spawn.result",
                json!({"agent": "agent:h/s/a", "ok": true}),
            ),
        ];
        let r = row(&run(&log, By::Profile), "sonnet").clone();
        assert_eq!((r.gate_reds, r.done_sd_min, r.quota_pp), (None, None, None));
        assert!(render(&run(&log, By::Profile)).contains("done sd ?m"));
    }

    #[test]
    fn a_spawn_with_no_result_and_no_agent_up_is_unconfirmed_and_left_out() {
        let log = vec![spawn("01", 0, "a", "sonnet", 1)];
        let r = row(&run(&log, By::Profile), "sonnet").clone();
        assert_eq!((r.n, r.unconfirmed, r.spawn_failures), (0, 1, 0));
    }

    #[test]
    fn a_quota_reset_during_the_agents_life_and_empty_readings_give_no_rise() {
        let mut log = fixture();
        log.push(ev_quota("40", 0, 80.0));
        log.push(ev_quota("41", 95, 5.0)); // reset: negative rise, dropped
        log.push(ev(
            "42",
            50,
            "host:h",
            "agent.quota",
            json!({"host": "h", "kind": "claude", "read_at": at(50), "windows": []}),
        ));
        let r = row(&run(&log, By::Profile), "sonnet").clone();
        assert_eq!((r.quota_pp, r.quota_n), (Some(0.0), 1)); // only b (80 -> 80) counts
    }

    #[test]
    fn a_running_agent_counts_hours_to_now_and_a_restart_reopens_it() {
        let a = "agent:h/s/a";
        let log = vec![
            spawn("01", 0, "a", "sonnet", 1),
            ev(
                "02",
                0,
                "host:h",
                "spawn.result",
                json!({"agent": a, "ok": true}),
            ),
            up("03", 0, "a", "claude"),
            down("04", 30, "a"),
            up("05", 60, "a", "claude"), // restarted: no longer down
            ev_host_tell("06", 70, "IDLE", a),
        ];
        let s = compute(&log, at(120), Duration::days(1), By::Profile);
        assert_eq!(row(&s, "sonnet").interventions_per_hour, Some(0.5)); // 1 over 2h
    }

    #[test]
    fn a_stuck_record_without_a_cause_and_events_after_now_do_not_count() {
        let mut log = fixture();
        log.push(ev(
            "60",
            40,
            "host:h",
            "agent.stuck",
            json!({"agent": "agent:h/s/a", "detail": "cleared"}),
        ));
        log.push(ev_host_tell("61", 24 * 60 + 5, "IDLE", "agent:h/s/a")); // after now
        let with = row(&run(&log, By::Profile), "sonnet").clone();
        let base = row(&run(&fixture(), By::Profile), "sonnet").clone();
        assert_eq!(with.interventions_per_hour, base.interventions_per_hour);
    }

    #[test]
    fn render_names_every_row_with_its_count_and_a_legend() {
        let text = render(&run(&fixture(), By::Profile));
        assert!(text.contains("sonnet") && text.contains("devin"), "{text}");
        assert!(text.contains("n=2"), "{text}");
        assert!(text.contains("p50") && text.contains("legend"), "{text}");
        assert!(render(&Scorecard::default()).contains("no spawns in the window"));
    }
}
