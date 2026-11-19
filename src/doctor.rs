//! `agent-fleet doctor`: what needs a human look, read off the state. Pure, like the reducer.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::event::{AgentState, Level};
use crate::state::{Agent, Hold, State, Worktree};
use crate::ui::{ago, at, dur, hold};
use crate::update::short;

/// Idle, done or blocked this long: the agent is stale.
pub const STALE: Duration = Duration::hours(2);
/// Working this long with no CPU and no change in its worktree: the agent may be hung.
pub const HUNG: Duration = Duration::minutes(30);
/// `HUNG` for an agent no `proc.hung` names: its own stall, or one the watch cannot see into.
pub const HUNG_UNPROVEN: Duration = Duration::minutes(90);
pub const HUNG_CPU_PCT: u32 = 2;
pub const HEAVY_RSS_KB: u64 = 4 * 1024 * 1024;
pub const HEAVY_CPU_PCT: u32 = 200;
/// Commits this old and still on no remote are worth a nudge.
pub const UNPUSHED: Duration = Duration::hours(1);
/// A clean worktree nobody works in, untouched this long, is clutter.
pub const ABANDONED: Duration = Duration::days(3);
/// A clean worktree nobody works in, still running processes this long after it last changed.
pub const LEFTOVER: Duration = Duration::minutes(10);
/// A refused kill stays a finding this long.
pub const REFUSED: Duration = Duration::hours(1);
/// Queued this long for a resource: worth a look at who holds it.
pub const QUEUED: Duration = Duration::minutes(10);
/// A tell still undelivered this long: its recipient is busy, blocked, or has no host ringing.
pub const UNDELIVERED: Duration = Duration::minutes(10);
/// A quota window this full with its reset still ahead: slow down or it will run out.
pub const QUOTA_PCT: f64 = 90.0;
/// A spawn request unanswered this long means its host is not listening.
pub const SPAWN_UNANSWERED: Duration = Duration::minutes(5);
/// Idle or blocked this long after working: somebody should be told. Same as `host --stall-min`.
pub const STALLED: Duration = Duration::minutes(crate::spawn::STALL_MIN as i64);
/// A failed spawn stays a finding this long.
pub const SPAWN_FAILED: Duration = Duration::hours(1);
/// A host's latest self-update that failed is a finding for this long after the request, and
/// longer while the host still runs the commit it failed on (nothing will ask it again until
/// `origin/main` moves); one a live host left unanswered is a finding from `UPDATE_UNANSWERED`
/// after it.
pub const UPDATE_FAILED: Duration = Duration::hours(2);
/// A failed update is kept past `UPDATE_FAILED` only while its host reported this recently.
pub const UPDATE_HOST_RECENT: Duration = Duration::hours(24);
/// Above the longest an update can take to answer: the wait for another update's lock plus a
/// build (`update::LOCK_WAIT` + `update::BUILD`, 65 min), with room for the git steps.
pub const UPDATE_UNANSWERED: Duration = Duration::minutes(75);
/// A `proc.hung` report confirmed this recently is kill proof: three watch ticks.
pub const HUNG_FRESH: Duration = Duration::minutes(6);
/// A host whose `host.health` is older than this is not compared for `build`.
pub const HOST_LIVE: Duration = Duration::minutes(10);
/// Hosts that agree on a commit `origin/main` left behind this long are a `build` finding: the
/// self-update (asked within 5 min, done in minutes) did not take.
pub const BEHIND: Duration = Duration::minutes(15);
/// Files every branch touches; sharing them is not duplicate work.
const SHARED_BY_ALL: &[&str] = &[
    "bun.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "uv.lock",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// `stale`, `hung`, `heavy`, `duplicate`, `forgotten`, `unpushed`, `orphan`, `breach`,
    /// `queued`, `undelivered`, `refused`, `role`, `quota`, `spawn`, `unmanaged`, `stalled`, `build`, `update`, `orphan-pr`
    /// (added by `main`).
    pub kind: &'static str,
    /// The agent id or `<host>:<path>` it is about.
    pub subject: String,
    pub detail: String,
}

fn finding(kind: &'static str, subject: impl Into<String>, detail: String) -> Finding {
    Finding {
        kind,
        subject: subject.into(),
        detail,
    }
}

/// The worktree an agent works in: the deepest one on its host containing its cwd.
pub fn worktree_of<'a>(s: &'a State, a: &Agent) -> Option<(&'a String, &'a Worktree)> {
    containing(s, &a.host, a.usage.as_ref()?.cwd.as_deref()?)
}

/// The deepest known worktree on `host` containing `dir`.
fn containing<'a>(s: &'a State, host: &str, dir: &str) -> Option<(&'a String, &'a Worktree)> {
    s.worktrees
        .iter()
        .filter(|(_, w)| {
            w.state.host == host
                && (dir == w.state.path || dir.starts_with(&format!("{}/", w.state.path)))
        })
        .max_by_key(|(_, w)| w.state.path.len())
}

/// Worktree key → the live agents that work in it: their cwd is in it, or they were spawned into
/// it (`State::assigned_worktrees`). Neither `forgotten` nor `dev clean` touches these.
pub fn workers(s: &State) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut add = |key: String, id: &str| {
        let v = out.entry(key).or_default();
        if !v.iter().any(|a| a == id) {
            v.push(id.to_string());
        }
    };
    for (id, a) in &s.agents {
        if let Some((key, _)) = worktree_of(s, a) {
            add(key.clone(), id);
        }
    }
    let hosts: std::collections::BTreeSet<&String> = s.agents.values().map(|a| &a.host).collect();
    for host in hosts {
        for (id, tree) in s.assigned_worktrees(host) {
            if let Some(t) = tree {
                let key = containing(s, host, &t)
                    .map_or_else(|| crate::state::worktree_key(host, &t), |(k, _)| k.clone());
                add(key, &id);
            }
        }
    }
    out
}

/// Who a resource holder is for a capacity rule: its worktree, else its cwd, else its pid.
fn holder_of(s: &State, host: &str, h: &Hold) -> String {
    let Some(cwd) = h.cwd.as_deref() else {
        return format!("pid {}", h.pid);
    };
    s.worktrees
        .iter()
        .filter(|(_, w)| {
            w.state.host == host
                && (cwd == w.state.path || cwd.starts_with(&format!("{}/", w.state.path)))
        })
        .max_by_key(|(_, w)| w.state.path.len())
        .map_or(cwd.to_string(), |(k, _)| k.clone())
}

fn procs(n: u32) -> String {
    format!("{n} process{}", if n == 1 { "" } else { "es" })
}

fn mb(kb: u64) -> String {
    format!("{:.0} MB", kb as f64 / 1024.0)
}

fn files(v: &[&String]) -> String {
    let shown: Vec<&str> = v.iter().take(3).map(|f| f.as_str()).collect();
    match v.len() {
        n if n > 3 => format!("{} (+{})", shown.join(", "), n - 3),
        _ => shown.join(", "),
    }
}

/// `agent` worked, or a later spawn of it succeeded, after its spawn requested at `since`.
fn spawned_since(s: &State, agent: &str, since: DateTime<Utc>) -> bool {
    s.agents
        .get(agent)
        .and_then(|a| a.worked)
        .is_some_and(|w| w > since)
        || s.spawns.values().any(|o| {
            o.request.agent == agent && o.since > since && o.result.as_ref().is_some_and(|r| r.ok)
        })
}

/// The intentional hold on an agent's open task: an open blocker, else an unfinished dep.
/// `None` when no open task of its is held.
pub fn held(s: &State, id: &str) -> Option<String> {
    for (tid, t) in &s.tasks {
        if t.owner.as_deref() != Some(id) || t.state.is_terminal() {
            continue;
        }
        for b in s.blockers.values() {
            if b.task.as_deref() == Some(tid) {
                let mut d = format!("held: {}", b.what);
                if let Some(o) = &b.owner {
                    d += &format!(" · {o}");
                }
                if let Some(w) = &b.unblocks_when {
                    d += &format!(" · unblocks {w}");
                }
                return Some(d);
            }
        }
        if let Some(dep) = t
            .blocked_by
            .iter()
            .find(|d| s.tasks.get(*d).is_none_or(|dt| !dt.state.is_terminal()))
        {
            return Some(format!("held: waits for {dep}"));
        }
    }
    None
}

/// A "~21m" or "ETA 3h" in a hold's scope, as a duration.
fn eta_of(scope: &str) -> Option<Duration> {
    let low = scope.to_lowercase();
    let pos = low
        .find('~')
        .map(|p| p + 1)
        .or_else(|| low.find("eta ").map(|p| p + 4))?;
    let rest = low[pos..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().ok()?;
    Some(match rest[digits.len()..].trim_start().chars().next() {
        Some('h') => Duration::hours(n),
        _ => Duration::minutes(n),
    })
}

fn eta_str(d: Duration) -> String {
    let m = d.num_minutes();
    if m >= 60 {
        format!("{}h{:02}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// The resource the agent's worktree holds or queues for, as `(detail, since, eta)`: `since`
/// starts the wait's clock, `eta` the holder's stated one ("~21m" in its scope). A legitimate
/// wait, never `hung` or `stalled`.
pub fn resource_wait(s: &State, a: &Agent) -> Option<(String, DateTime<Utc>, Option<Duration>)> {
    let (_, w) = worktree_of(s, a)?;
    let inside = |h: &Hold| {
        h.cwd
            .as_deref()
            .is_some_and(|c| c == w.state.path || c.starts_with(&format!("{}/", w.state.path)))
    };
    for r in s.resources.values().filter(|r| r.host == a.host) {
        if let Some(h) = &r.holder
            && inside(h)
        {
            let scope = h
                .scope
                .as_deref()
                .map(|sc| format!(" ({sc})"))
                .unwrap_or_default();
            return Some((format!("holds {}{scope}", r.resource), h.since, None));
        }
        if let Some(wt) = r.waiting.iter().find(|w| inside(w)) {
            let holder = r.holder.as_ref();
            let eta = holder.and_then(|h| h.scope.as_deref()).and_then(eta_of);
            let behind = holder
                .map(|h| {
                    format!(
                        " behind {}",
                        h.scope
                            .clone()
                            .or_else(|| h.cwd.clone())
                            .unwrap_or_else(|| format!("pid {}", h.pid))
                    )
                })
                .unwrap_or_default();
            let eta_s = eta
                .map(|d| format!(", ~{}", eta_str(d)))
                .unwrap_or_default();
            return Some((
                format!("waiting for {}{behind}{eta_s}", r.resource),
                wt.since,
                eta,
            ));
        }
    }
    None
}

/// `stalled`: idle or blocked at least `min` since it last worked — but never parked: an
/// agent that reached `done` and has not worked since awaits its next assignment, and an
/// idle stretch there is the `stale` finding's to name, not a stall's. The host's push uses
/// this exact predicate, so the board and the notice can never disagree.
pub fn stalled(a: &Agent, now: DateTime<Utc>, min: Duration) -> Option<String> {
    let idle_for = now - a.since;
    let parked = a.finished.is_some_and(|f| a.worked.is_none_or(|w| w <= f));
    (matches!(a.state, AgentState::Idle | AgentState::Blocked)
        && idle_for >= min
        && !parked
        && a.worked.is_some())
    .then(|| format!("idle for {}m since it worked", idle_for.num_minutes()))
}

/// `hung`: `working` and under HUNG_CPU_PCT, its worktree unchanged, for HUNG when the host's watch
/// proved a process of the agent hung (`proc.hung`), else for HUNG_UNPROVEN. A pane tree that
/// samples 0% CPU while a command waits under load proves nothing alone (s-c844, 18 Nov: 38 min,
/// `bun run check` queued under load 68), so the bare reading needs three times as long; the
/// watch's report is what makes 30 minutes enough.
pub fn hung(s: &State, id: &str, a: &Agent, now: DateTime<Utc>) -> Option<String> {
    let need = if hung_pids_of(s, id, now) {
        HUNG
    } else {
        HUNG_UNPROVEN
    };
    // a worktree waiting on a resource is a legitimate wait, not a hang
    if resource_wait(s, a).is_some() {
        return None;
    }
    let u = a.usage.as_ref()?;
    let wt = worktree_of(s, a);
    if a.state != AgentState::Working
        || now - a.since < need
        || !u.cpu_pct.is_some_and(|c| c < HUNG_CPU_PCT)
        || !wt.is_none_or(|(_, w)| now - w.since >= need)
    {
        return None;
    }
    let still = wt.map_or(String::new(), |(_, w)| {
        format!(", its worktree unchanged for {}", ago(now, w.since))
    });
    Some(format!(
        "working for {} at {}% CPU{still}",
        ago(now, a.since),
        u.cpu_pct.unwrap_or(0)
    ))
}

/// Who a stuck agent's notice goes to: its declared lead, else an up root-coordinator, else
/// nobody — and never the agent itself.
pub fn escalate<'a>(s: &'a State, id: &str) -> Option<&'a str> {
    let to = s
        .roles
        .get(id)
        .and_then(|r| r.lead.as_deref())
        .or_else(|| {
            s.agents
                .keys()
                .find(|a| {
                    s.roles
                        .get(*a)
                        .is_some_and(|r| r.level == Level::RootCoordinator)
                })
                .map(String::as_str)
        })?;
    (to != id).then_some(to)
}

/// "; <who> told" when a stuck agent has somebody, "; nobody to tell" when it does not.
fn told(s: &State, id: &str) -> String {
    escalate(s, id).map_or_else(|| "; nobody to tell".to_string(), |t| format!("; {t} told"))
}

/// Pids on `host` that doctor proves orphaned, with the directory each runs in. A repo's main
/// checkout is never included: its processes may be the human's. The host's own `proc.orphaned`
/// proofs — a reparented tool whose device is gone, a lock whose recorded owner died — count
/// the same: their `cwd`, when the platform reports one, is the directory to check.
pub fn killable(
    s: &State,
    now: DateTime<Utc>,
    host: &str,
) -> std::collections::BTreeMap<u32, String> {
    let mut out: std::collections::BTreeMap<u32, String> = findings(s, now)
        .iter()
        .filter(|f| {
            f.kind == "orphan"
                || (f.kind == "forgotten"
                    && s.worktrees
                        .get(&f.subject)
                        .is_some_and(|w| w.state.path != w.state.repo))
        })
        .filter_map(|f| s.worktree_procs.get(&f.subject))
        .filter(|p| p.procs.host == host)
        .flat_map(|p| p.procs.pids.iter().map(|&pid| (pid, p.procs.path.clone())))
        .collect();
    for o in s.orphans.values().filter(|o| o.host == host) {
        out.insert(o.pid, o.cwd.clone().unwrap_or_else(|| "/".into()));
    }
    out
}

/// Pids on `host` a `proc.hung` report names, with when it was first found hung: a live
/// process with that pid that started later is not the one that hung. Only a report the host
/// confirmed within `HUNG_FRESH` counts: one nobody re-checks proves nothing for `kill`.
pub fn hung_pids(
    s: &State,
    host: &str,
    now: DateTime<Utc>,
) -> std::collections::BTreeMap<u32, DateTime<Utc>> {
    s.hung
        .values()
        .filter(|h| h.host == host && h.checked.is_some_and(|c| now - c <= HUNG_FRESH))
        .flat_map(|h| {
            let since = h.since.unwrap_or(DateTime::<Utc>::MIN_UTC);
            std::iter::once(h.pid)
                .chain(h.pids.iter().copied())
                .map(move |p| (p, since))
        })
        .collect()
}

/// Whether the host's watch has a fresh `proc.hung` for a process this agent started.
fn hung_pids_of(s: &State, id: &str, now: DateTime<Utc>) -> bool {
    s.hung
        .values()
        .any(|h| h.agent.as_deref() == Some(id) && h.checked.is_some_and(|c| now - c <= HUNG_FRESH))
}

pub fn findings(s: &State, now: DateTime<Utc>) -> Vec<Finding> {
    let mut out = Vec::new();
    // Worktree key → the live agents working in it.
    let mut occupied: BTreeMap<&String, Vec<&String>> = BTreeMap::new();
    for (id, a) in &s.agents {
        let wt = worktree_of(s, a);
        if let Some((key, _)) = wt {
            occupied.entry(key).or_default().push(id);
        }
        let idle_for = now - a.since;
        let cause = s
            .stuck
            .get(id)
            .map(|st| format!("; {}", st.cause.name()))
            .unwrap_or_default();
        if let Some(h) = held(s, id) {
            out.push(finding("held", id, format!("{h}{}", told(s, id))));
        } else if resource_wait(s, a).is_none()
            && let Some(detail) = stalled(a, now, STALLED)
        {
            out.push(finding(
                "stalled",
                id,
                format!("{detail}{cause}{}", told(s, id)),
            ));
        }
        if matches!(
            a.state,
            AgentState::Idle | AgentState::Done | AgentState::Blocked
        ) && idle_for >= STALE
        {
            let state = serde_json::to_value(a.state).unwrap_or_default();
            out.push(finding(
                "stale",
                id,
                format!(
                    "{} for {}: give it work or retire it",
                    state.as_str().unwrap_or("?"),
                    ago(now, a.since)
                ),
            ));
        }
        if let Some(r) = s.roles.get(id)
            && let Some(lead) = &r.lead
        {
            if !s.agents.contains_key(lead) {
                out.push(finding("role", id, format!("its lead {lead} is down")));
            }
            if let Some(l) = s.roles.get(lead)
                && l.level <= r.level
            {
                out.push(finding(
                    "role",
                    id,
                    format!("reports to {lead}, a {}, but is a {}", l.level, r.level),
                ));
            }
        }
        if let Some(detail) = hung(s, id, a, now) {
            out.push(finding(
                "hung",
                id,
                format!("{detail}{cause}{}", told(s, id)),
            ));
        }
        let Some(u) = &a.usage else { continue };
        if let (Some(rss), Some(cpu)) = (u.rss_kb, u.cpu_pct)
            && (rss >= HEAVY_RSS_KB || cpu >= HEAVY_CPU_PCT)
        {
            out.push(finding(
                "heavy",
                id,
                format!(
                    "{:.1} GB, {}% CPU over {} processes",
                    rss as f64 / (1024.0 * 1024.0),
                    cpu,
                    u.procs.unwrap_or(0)
                ),
            ));
        }
    }
    let roots: Vec<&String> = s
        .agents
        .keys()
        .filter(|id| {
            s.roles
                .get(*id)
                .is_some_and(|r| r.level == Level::RootCoordinator)
        })
        .collect();
    if roots.len() > 1 {
        let all = roots
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        for extra in roots.iter().skip(1) {
            out.push(finding(
                "role",
                *extra,
                format!("{} root coordinators are up: {all}", roots.len()),
            ));
        }
    }
    for (key, agents) in &occupied {
        if agents.len() > 1 {
            let names: Vec<&str> = agents.iter().map(|a| a.as_str()).collect();
            out.push(finding(
                "duplicate",
                key.as_str(),
                format!(
                    "{} agents in one worktree: {}",
                    agents.len(),
                    names.join(", ")
                ),
            ));
        }
    }
    let busy: Vec<(&String, &Worktree)> = occupied
        .keys()
        .filter_map(|k| s.worktrees.get_key_value(*k))
        .collect();
    for (i, (ka, a)) in busy.iter().enumerate() {
        for (kb, b) in &busy[i + 1..] {
            let shared: Vec<&String> = a
                .state
                .files
                .iter()
                .filter(|f| {
                    b.state.files.contains(f) && !SHARED_BY_ALL.iter().any(|l| f.ends_with(l))
                })
                .collect();
            if !shared.is_empty() {
                out.push(finding(
                    "duplicate",
                    format!("{ka} + {kb}"),
                    format!(
                        "{} and {} both change {}",
                        occupied[ka][0],
                        occupied[kb][0],
                        files(&shared)
                    ),
                ));
            }
        }
    }
    for (key, p) in &s.worktree_procs {
        let p = &p.procs;
        let names = p.names.join(", ");
        // Defence in depth: a directory that is a known worktree is never "deleted".
        if p.orphaned && !s.worktrees.contains_key(key) {
            let pids: Vec<String> = p.pids.iter().map(u32::to_string).collect();
            out.push(finding(
                "orphan",
                key,
                format!(
                    "{} ({names}) still running in this deleted worktree, {}: kill {}",
                    procs(p.procs),
                    mb(p.rss_kb),
                    pids.join(" ")
                ),
            ));
        } else if p.rss_kb >= HEAVY_RSS_KB || p.cpu_pct >= HEAVY_CPU_PCT {
            out.push(finding(
                "heavy",
                key,
                format!(
                    "{names}: {}, {}% CPU over {} processes",
                    mb(p.rss_kb),
                    p.cpu_pct,
                    p.procs
                ),
            ));
        }
    }
    for (key, o) in &s.orphans {
        out.push(finding(
            "orphan",
            key.clone(),
            format!(
                "reparented {}, {} — kill: agent-fleet kill {}",
                o.pid, o.why, o.pid
            ),
        ));
    }
    // Each host's latest update that concerns it: refused or failed, or never answered by a host
    // that is up. A later success clears it.
    let mut by_since: Vec<_> = s.updates.values().collect();
    by_since.sort_by_key(|u| std::cmp::Reverse(u.since));
    for (host, h) in &s.hosts {
        let up = now - h.ts < Duration::minutes(3);
        // Newest first: a request too young to judge does not hide an older failure.
        for u in &by_since {
            // A request already stale when the host started was ignored by it, not left
            // unanswered.
            // A minute of slack: the host first looks a few seconds after it starts.
            let ignored = h.started.is_some_and(|st| {
                st - u.since >= crate::update::REQUEST_MAX_AGE + Duration::minutes(1)
            });
            let concerns = u.results.contains_key(host)
                || (up
                    && !ignored
                    && u.host.as_deref().is_none_or(|t| t == host)
                    && u.since <= h.ts);
            let age = now - u.since;
            if !concerns {
                continue;
            }
            let r = u.results.get(host);
            // A host that has not reported for a day is gone, not failing.
            let still_failed = now - h.ts < UPDATE_HOST_RECENT
                && r.is_some_and(|r| !r.ok && r.commit.is_some() && r.commit == h.commit);
            if age >= UPDATE_FAILED && !still_failed {
                break;
            }
            let why = match r {
                Some(r) if !r.ok => {
                    format!(
                        "failed: {}",
                        r.error.as_deref().unwrap_or("no error reported")
                    )
                }
                None if age >= UPDATE_UNANSWERED => "not answered".to_string(),
                None => continue,
                Some(_) => break,
            };
            out.push(finding(
                "update",
                format!("host:{host}"),
                format!(
                    "self-update asked {} ago by {}: {why}",
                    ago(now, u.since),
                    u.by
                ),
            ));
            break;
        }
    }
    for (key, h) in &s.hung {
        let pids: Vec<String> = std::iter::once(h.pid)
            .chain(h.pids.iter().copied().filter(|&p| p != h.pid))
            .map(|p| p.to_string())
            .collect();
        let up = h
            .started
            .map(|t| format!(", up {}", ago(now, t)))
            .unwrap_or_default();
        out.push(finding(
            "hung",
            key.clone(),
            format!(
                "{} (pid {}{up}{}) hung {}: {} — kill: agent-fleet kill {}",
                h.command
                    .as_deref()
                    .map_or("?".into(), crate::watch::short_cmd),
                h.pid,
                h.agent
                    .as_deref()
                    .map(|a| format!(", {}", crate::host::short(a)))
                    .unwrap_or_default(),
                h.since.map_or("?".into(), |t| ago(now, t)),
                h.evidence.as_deref().unwrap_or("?"),
                pids.join(" ")
            ),
        ));
    }
    let workers = workers(s);
    for (key, w) in &s.worktrees {
        let st = &w.state;
        let running = s.worktree_procs.get(key).map_or(String::new(), |p| {
            format!(
                "; still runs {} ({}), {}",
                procs(p.procs.procs),
                p.procs.names.join(", "),
                mb(p.procs.rss_kb)
            )
        });
        let unsaved = st.unpushed > 0 || st.dirty > 0;
        let unchanged = now - w.since;
        let submodules = match st.dirty_submodules {
            0 => String::new(),
            n if n == st.dirty => " (only submodule pointers moved)".into(),
            n => format!(" ({n} of them submodule pointers)"),
        };
        // Only a worktree herdr or spawn made is the fleet's to call forgotten: a main checkout
        // or a scratch worktree an agent once stood in is somebody's own.
        if !workers.contains_key(key) {
            if st.herdr && unsaved && unchanged >= UNPUSHED {
                out.push(finding(
                    "forgotten",
                    key,
                    format!(
                        "no agent for {}, {} unpushed commits, {} dirty files{submodules}{running}",
                        ago(now, w.since),
                        st.unpushed,
                        st.dirty
                    ),
                ));
            } else if !unsaved
                && st.herdr
                && (now - st.last_commit >= ABANDONED
                    || (!running.is_empty() && unchanged >= LEFTOVER))
            {
                let what = if running.is_empty() {
                    ": remove it"
                } else {
                    ""
                };
                out.push(finding(
                    "forgotten",
                    key,
                    format!(
                        "no agent, nothing unsaved{}, last commit {} ago{running}{what}",
                        if st.in_main {
                            ", all of it in main"
                        } else {
                            ""
                        },
                        ago(now, st.last_commit)
                    ),
                ));
            }
        } else if st.unpushed > 0 && now - st.last_commit >= UNPUSHED {
            out.push(finding(
                "unpushed",
                key,
                format!(
                    "{} commits on {} not on any remote for {}",
                    st.unpushed,
                    st.branch.as_deref().unwrap_or("a detached HEAD"),
                    dur((now - st.last_commit).num_milliseconds().max(0) as u64)
                ),
            ));
        }
    }
    for (key, l) in &s.limits {
        if l.until.is_some_and(|u| now >= u) {
            continue;
        }
        let held: Vec<(&String, &Hold)> = s
            .resources
            .values()
            .filter(|r| r.host == l.host && l.resources.contains(&r.resource))
            .filter_map(|r| Some((&r.resource, r.holder.as_ref()?)))
            .collect();
        let holders: std::collections::BTreeSet<String> =
            held.iter().map(|(_, h)| holder_of(s, &l.host, h)).collect();
        if holders.len() > l.max as usize {
            let what: Vec<String> = held
                .iter()
                .map(|(r, h)| format!("{r}: {}", hold(now, h)))
                .collect();
            out.push(finding(
                "breach",
                key,
                format!(
                    "{} hold {} at once, the rule allows {}: {}",
                    holders.len(),
                    l.resources.join(" / "),
                    l.max,
                    what.join("; ")
                ),
            ));
        }
    }
    // A refusal because the pid was already gone refused nothing.
    for k in s
        .refused_kills
        .iter()
        .filter(|k| now - k.ts < REFUSED && !k.reason.ends_with(crate::kill::GONE))
    {
        out.push(finding(
            "refused",
            &k.by,
            format!(
                "tried to kill pid {} ({}) on {} {} ago: {}",
                k.pid,
                k.name,
                k.host,
                ago(now, k.ts),
                k.reason
            ),
        ));
    }
    for u in s.unmanaged.values() {
        out.push(finding(
            "unmanaged",
            format!("{}:pid {}", u.host, u.pid),
            format!(
                "{} running outside herdr in {} for {}",
                u.program,
                u.cwd.as_deref().unwrap_or("?"),
                ago(now, u.since)
            ),
        ));
    }
    for (key, q) in &s.quotas {
        if q.exhausted {
            out.push(finding(
                "quota",
                key,
                format!("out of quota (read {} ago)", ago(now, q.read_at)),
            ));
        } else if let Some(w) = q
            .windows
            .iter()
            .find(|w| w.used_pct >= QUOTA_PCT && w.resets_at.is_some_and(|r| r > now))
        {
            out.push(finding(
                "quota",
                key,
                format!(
                    "{} {:.0}% used until {}",
                    w.name,
                    w.used_pct,
                    at(now, w.resets_at.unwrap())
                ),
            ));
        }
    }
    for sp in s.spawns.values() {
        match &sp.result {
            None if now - sp.since >= SPAWN_UNANSWERED => out.push(finding(
                "spawn",
                &sp.request.agent,
                format!(
                    "host {} has not picked it up ({} ago)",
                    sp.request.host,
                    ago(now, sp.since)
                ),
            )),
            Some(r)
                if !r.ok
                    && sp.answered.is_some_and(|t| now - t < SPAWN_FAILED)
                    && !spawned_since(s, &sp.request.agent, sp.since) =>
            {
                out.push(finding(
                    "spawn",
                    &r.agent,
                    format!("spawn failed: {}", r.error.as_deref().unwrap_or("?")),
                ));
            }
            _ => {}
        }
    }
    // Hosts that reported lately but run different builds: one of them missed an update.
    let hosts: Vec<_> = s
        .hosts
        .iter()
        .filter(|(_, h)| now - h.ts < HOST_LIVE)
        .collect();
    let live: Vec<(&String, Option<&str>)> = hosts
        .iter()
        .map(|(n, h)| (*n, h.commit.as_deref()))
        .collect();
    if live.iter().any(|(_, c)| *c != live[0].1) {
        for (name, commit) in &live {
            let others: Vec<String> = live
                .iter()
                .filter(|(n, _)| n != name)
                .map(|(n, c)| format!("{n} {}", short(*c)))
                .collect();
            out.push(finding(
                "build",
                *name,
                format!(
                    "runs {}; {} (agent-fleet self-update)",
                    short(*commit),
                    others.join(", ")
                ),
            ));
        }
    } else {
        // They agree, on a commit `origin/main` has moved past.
        for (name, h) in &hosts {
            if let Some(since) = h.behind_since.filter(|t| now - *t >= BEHIND) {
                out.push(finding(
                    "build",
                    *name,
                    format!(
                        "runs {}, behind origin/main for {} (agent-fleet self-update)",
                        short(h.commit.as_deref()),
                        ago(now, since)
                    ),
                ));
            }
        }
    }
    for (key, r) in &s.resources {
        for w in r.waiting.iter().filter(|w| now - w.since >= QUEUED) {
            let behind = r
                .holder
                .as_ref()
                .map_or("nobody".to_string(), |h| hold(now, h));
            out.push(finding(
                "queued",
                key,
                format!("{}, behind {behind}", hold(now, w)),
            ));
        }
    }
    // Per recipient: how many tells wait, the oldest, and what the recipient is doing.
    let mut waiting: BTreeMap<&str, (usize, DateTime<Utc>)> = BTreeMap::new();
    for m in s.msgs.values().filter(|m| now - m.since >= UNDELIVERED) {
        let w = waiting.entry(&m.to).or_insert((0, m.since));
        w.0 += 1;
        w.1 = w.1.min(m.since);
    }
    for (to, (n, since)) in waiting {
        let doing =
            s.recipient(to)
                .and_then(|r| s.agents.get(r))
                .map_or("not up".to_string(), |a| {
                    let st = serde_json::to_value(a.state).unwrap_or_default();
                    format!("{} for {}", st.as_str().unwrap_or("?"), ago(now, a.since))
                });
        out.push(finding(
            "undelivered",
            to,
            format!(
                "{n} tell(s) waiting, oldest {}; recipient is {doing}",
                ago(now, since)
            ),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::WorktreeState;
    use crate::state::{Usage, worktree_key};

    fn agent(state: AgentState, since: DateTime<Utc>, cwd: &str, cpu: u32, rss_kb: u64) -> Agent {
        Agent {
            host: "h".into(),
            kind: None,
            pane: None,
            state,
            since,
            worked: None,
            finished: None,
            usage: Some(Usage {
                rss_kb: Some(rss_kb),
                cpu_pct: Some(cpu),
                procs: Some(3),
                cwd: Some(cwd.into()),
                ts: since,
            }),
        }
    }

    fn tree(
        path: &str,
        files: &[&str],
        unpushed: u32,
        dirty: u32,
        since: DateTime<Utc>,
    ) -> Worktree {
        Worktree {
            state: WorktreeState {
                host: "h".into(),
                path: path.into(),
                repo: "/r/main".into(),
                branch: Some("b".into()),
                head: "abc".into(),
                unpushed,
                dirty,
                dirty_submodules: 0,
                files: files.iter().map(|f| f.to_string()).collect(),
                last_commit: since,
                in_main: false,
                herdr: true,
            },
            since,
        }
    }

    fn kinds(f: &[Finding]) -> Vec<(&str, &str)> {
        f.iter().map(|f| (f.kind, f.subject.as_str())).collect()
    }

    #[test]
    fn a_failed_or_unanswered_update_is_a_finding_until_a_later_one_succeeds() {
        use crate::event::UpdateResult;
        let now = Utc::now();
        let ans = |ok: bool, error: Option<&str>| UpdateResult {
            host: "mac".into(),
            ok,
            commit: None,
            error: error.map(String::from),
        };
        let up = |mins: i64, host: Option<&str>, results: Vec<UpdateResult>| crate::state::Update {
            by: "host:ops".into(),
            since: now - Duration::minutes(mins),
            host: host.map(String::from),
            results: results.into_iter().map(|r| (r.host.clone(), r)).collect(),
        };
        let mut s = State::default();
        s.hosts.insert(
            "mac".into(),
            crate::state::Host {
                disk_free: 0,
                disk_total: 0,
                agents: 0,
                commit: None,
                behind_since: None,
                ts: now,
                started: None,
            },
        );
        s.updates.insert(
            "a".into(),
            up(
                30,
                None,
                vec![ans(false, Some("a clone with uncommitted changes"))],
            ),
        );
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("update", "host:mac")], "{f:#?}");
        assert!(
            f[0].detail.contains("failed: a clone with uncommitted"),
            "{}",
            f[0].detail
        );
        // A later success clears it.
        s.updates
            .insert("b".into(), up(5, None, vec![ans(true, None)]));
        assert!(findings(&s, now).is_empty());
        // Never answered by a live host: a finding once old enough, not before.
        s.updates.clear();
        s.updates.insert("c".into(), up(3, Some("mac"), vec![]));
        assert!(findings(&s, now).is_empty(), "too soon");
        s.updates.insert("c".into(), up(80, Some("mac"), vec![]));
        let f = findings(&s, now);
        assert!(f[0].detail.contains("not answered"), "{f:#?}");
        // Asked of another host, or an hour old: nothing.
        s.updates.insert("c".into(), up(80, Some("other"), vec![]));
        assert!(findings(&s, now).is_empty());
        s.updates.insert("c".into(), up(150, Some("mac"), vec![]));
        assert!(findings(&s, now).is_empty());
    }

    #[test]
    fn a_young_request_does_not_hide_an_older_failure_and_a_quiet_host_is_not_blamed() {
        use crate::event::UpdateResult;
        let now = Utc::now();
        let host = |ago_min: i64| crate::state::Host {
            disk_free: 0,
            disk_total: 0,
            agents: 0,
            commit: None,
            behind_since: None,
            ts: now - Duration::minutes(ago_min),
            started: None,
        };
        let up = |mins: i64, results: Vec<UpdateResult>| crate::state::Update {
            by: "host:ops".into(),
            since: now - Duration::minutes(mins),
            host: None,
            results: results.into_iter().map(|r| (r.host.clone(), r)).collect(),
        };
        let failed = UpdateResult {
            host: "mac".into(),
            ok: false,
            commit: None,
            error: None,
        };
        let mut s = State::default();
        s.hosts.insert("mac".into(), host(0));
        s.updates.insert("old".into(), up(30, vec![failed.clone()]));
        s.updates.insert("new".into(), up(2, vec![]));
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("update", "host:mac")], "{f:#?}");
        assert!(
            f[0].detail.contains("failed: no error reported"),
            "{}",
            f[0].detail
        );
        // A host quiet for 10 min is down, not ignoring the request.
        s.updates.insert("new".into(), up(45, vec![]));
        s.hosts.insert("mac".into(), host(10));
        s.updates.remove("old");
        assert!(findings(&s, now).is_empty(), "stale host");
        // A request newer than the host's last health: it may not have seen it yet.
        s.hosts.insert("mac".into(), host(0));
        s.updates.insert("new".into(), up(-1, vec![]));
        assert!(findings(&s, now).is_empty());
        // A success with nothing before it.
        s.updates.insert(
            "ok".into(),
            up(5, vec![UpdateResult { ok: true, ..failed }]),
        );
        assert!(findings(&s, now).is_empty());
    }

    #[test]
    fn the_unanswered_limit_is_above_the_longest_an_update_takes_to_answer() {
        let longest = crate::update::LOCK_WAIT + crate::update::BUILD;
        assert!(UPDATE_UNANSWERED > Duration::from_std(longest).unwrap());
        assert!(UPDATE_FAILED > UPDATE_UNANSWERED);
    }

    #[test]
    fn a_failure_stays_a_finding_while_the_host_runs_the_commit_it_failed_on() {
        use crate::event::UpdateResult;
        let now = Utc::now();
        let mut s = State::default();
        let host = |commit: &str| crate::state::Host {
            disk_free: 0,
            disk_total: 0,
            agents: 0,
            commit: Some(commit.into()),
            behind_since: None,
            ts: now,
            started: None,
        };
        let failed = UpdateResult {
            host: "mac".into(),
            ok: false,
            commit: Some("aaa".into()),
            error: Some("cargo install: boom".into()),
        };
        let up = crate::state::Update {
            by: "host:ops".into(),
            since: now - Duration::hours(5),
            host: None,
            results: [("mac".to_string(), failed.clone())].into(),
        };
        s.updates.insert("a".into(), up.clone());
        s.hosts.insert("mac".into(), host("aaa"));
        let f = findings(&s, now);
        assert_eq!(
            kinds(&f),
            vec![("update", "host:mac")],
            "5 h on, still on aaa: {f:#?}"
        );
        // It moved on to another commit: the failure is history.
        s.hosts.insert("mac".into(), host("bbb"));
        assert!(findings(&s, now).is_empty());
        // An older host's failure names no commit: the time limit applies.
        s.hosts.insert("mac".into(), host("aaa"));
        let mut old = up;
        old.results.get_mut("mac").unwrap().commit = None;
        s.updates.insert("a".into(), old);
        assert!(findings(&s, now).is_empty());
    }

    #[test]
    fn a_later_success_clears_a_retained_failure_and_the_unanswered_limit_has_its_edges() {
        use crate::event::UpdateResult;
        let now = Utc::now();
        let mut s = State::default();
        s.hosts.insert(
            "mac".into(),
            crate::state::Host {
                disk_free: 0,
                disk_total: 0,
                agents: 0,
                commit: Some("aaa".into()),
                behind_since: None,
                ts: now,
                started: Some(now - Duration::minutes(5)),
            },
        );
        let result = |ok: bool, commit: &str| UpdateResult {
            host: "mac".into(),
            ok,
            commit: Some(commit.into()),
            error: None,
        };
        let up = |mins: i64, results: Vec<UpdateResult>| crate::state::Update {
            by: "host:ops".into(),
            since: now - Duration::minutes(mins),
            host: None,
            results: results.into_iter().map(|r| (r.host.clone(), r)).collect(),
        };
        s.updates
            .insert("old".into(), up(500, vec![result(false, "aaa")]));
        assert_eq!(findings(&s, now).len(), 1, "retained");
        s.updates
            .insert("new".into(), up(100, vec![result(true, "aaa")]));
        assert!(findings(&s, now).is_empty(), "a later success clears it");
        // Unanswered: quiet up to the limit, a finding after it. The host started after the
        // request, so the request (newer than its start) is not the "ignored" kind.
        s.updates.clear();
        s.hosts.get_mut("mac").unwrap().started = Some(now - Duration::minutes(600));
        s.updates.insert("r".into(), up(65, vec![]));
        assert!(
            findings(&s, now).is_empty(),
            "65 min: a build can still be running"
        );
        s.updates.insert("r".into(), up(80, vec![]));
        assert_eq!(findings(&s, now).len(), 1);
        // Exactly the request limit at host start: ignored; a minute less: not.
        s.hosts.get_mut("mac").unwrap().started = Some(now - Duration::minutes(64));
        s.updates.insert("r".into(), up(80, vec![]));
        assert!(findings(&s, now).is_empty(), "stale (16 min) at start");
        s.hosts.get_mut("mac").unwrap().started = Some(now - Duration::minutes(65));
        assert_eq!(
            findings(&s, now).len(),
            1,
            "15 min old at start: within the slack"
        );
        // A stale request that the host did answer still reports its result.
        s.hosts.get_mut("mac").unwrap().started = Some(now - Duration::minutes(65));
        s.updates
            .insert("r".into(), up(80, vec![result(false, "aaa")]));
        assert_eq!(findings(&s, now).len(), 1);
    }

    #[test]
    fn a_retained_failure_ends_when_its_host_has_not_reported_for_a_day() {
        use crate::event::UpdateResult;
        let now = Utc::now();
        let mut s = State::default();
        let host = |reported_ago: Duration| crate::state::Host {
            disk_free: 0,
            disk_total: 0,
            agents: 0,
            commit: Some("aaa".into()),
            behind_since: None,
            ts: now - reported_ago,
            started: None,
        };
        s.updates.insert(
            "a".into(),
            crate::state::Update {
                by: "host:ops".into(),
                since: now - Duration::hours(30),
                host: None,
                results: [(
                    "mac".to_string(),
                    UpdateResult {
                        host: "mac".into(),
                        ok: false,
                        commit: Some("aaa".into()),
                        error: Some("boom".into()),
                    },
                )]
                .into(),
            },
        );
        s.hosts.insert("mac".into(), host(Duration::hours(2)));
        assert_eq!(
            findings(&s, now).len(),
            1,
            "reported 2 h ago: still failing"
        );
        s.hosts.insert(
            "mac".into(),
            host(UPDATE_HOST_RECENT - Duration::minutes(1)),
        );
        assert_eq!(findings(&s, now).len(), 1, "just inside the day");
        s.hosts.insert("mac".into(), host(UPDATE_HOST_RECENT));
        assert!(findings(&s, now).is_empty(), "a day exactly: gone");
        s.hosts.insert("mac".into(), host(Duration::hours(25)));
        assert!(
            findings(&s, now).is_empty(),
            "silent for a day: gone, not failing"
        );
    }

    #[test]
    fn a_request_older_than_the_limit_when_the_host_started_was_ignored_not_unanswered() {
        let now = Utc::now();
        let mut s = State::default();
        let host = |started_min_ago: i64| crate::state::Host {
            disk_free: 0,
            disk_total: 0,
            agents: 0,
            commit: None,
            behind_since: None,
            ts: now,
            started: Some(now - Duration::minutes(started_min_ago)),
        };
        s.updates.insert(
            "r".into(),
            crate::state::Update {
                by: "host:ops".into(),
                since: now - Duration::minutes(90),
                host: None,
                results: Default::default(),
            },
        );
        // Started 60 min ago: the request was 30 min old then, past the limit: ignored.
        s.hosts.insert("mac".into(), host(60));
        assert!(findings(&s, now).is_empty());
        // Started 80 min ago: it was 10 min old, fresh: the host should have answered.
        s.hosts.insert("mac".into(), host(80));
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("update", "host:mac")], "{f:#?}");
    }

    #[test]
    fn each_kind_of_trouble_is_found() {
        let now = Utc::now();
        let h = |n| now - Duration::hours(n);
        let mut s = State::default();
        let mut add_tree = |w: Worktree| {
            s.worktrees.insert(worktree_key("h", &w.state.path), w);
        };
        add_tree(tree("/w/a", &["src/x.ts", "bun.lock"], 0, 0, h(2)));
        add_tree(tree("/w/b", &["src/x.ts", "bun.lock"], 0, 0, h(0)));
        add_tree(tree("/w/old", &[], 2, 1, h(5)));
        add_tree(tree("/w/clutter", &[], 0, 0, h(24 * 4)));
        add_tree(tree("/w/late", &[], 3, 0, h(2)));
        s.agents.insert(
            "agent:h/s/hung".into(),
            agent(AgentState::Working, h(1), "/w/a/src", 0, 1024),
        );
        s.agents.insert(
            "agent:h/s/busy".into(),
            agent(AgentState::Working, h(1), "/w/b", 250, 5 * 1024 * 1024),
        );
        s.agents.insert(
            "agent:h/s/idle".into(),
            agent(AgentState::Idle, h(3), "/w/late", 0, 1024),
        );
        proc_hung(&mut s, "agent:h/s/hung", 78, now);
        let f = findings(&s, now);
        let k = kinds(&f);
        assert!(k.contains(&("hung", "agent:h/s/hung")), "{f:#?}");
        assert!(
            !k.contains(&("hung", "agent:h/s/busy")),
            "its worktree just changed"
        );
        assert!(k.contains(&("heavy", "agent:h/s/busy")));
        assert!(k.contains(&("stale", "agent:h/s/idle")));
        assert!(k.contains(&("duplicate", "h:/w/a + h:/w/b")));
        let dup = f.iter().find(|f| f.kind == "duplicate").unwrap();
        assert!(!dup.detail.contains("bun.lock"), "{}", dup.detail);
        assert!(k.contains(&("forgotten", "h:/w/old")));
        assert!(k.contains(&("forgotten", "h:/w/clutter")));
        assert!(k.contains(&("unpushed", "h:/w/late")));
        // the agent-level hung and the process-level hung it needs
        assert_eq!(f.len(), 8, "{f:#?}");
    }

    #[test]
    fn orphans_and_processes_in_forgotten_worktrees() {
        use crate::event::WorktreeProcs;
        use crate::state::Procs;
        let now = Utc::now();
        let mut s = State::default();
        s.worktrees.insert(
            worktree_key("h", "/w/old"),
            tree("/w/old", &[], 0, 0, now - Duration::days(5)),
        );
        let procs = |path: &str, orphaned| Procs {
            procs: WorktreeProcs {
                host: "h".into(),
                path: path.into(),
                procs: 2,
                rss_kb: 300 * 1024,
                cpu_pct: 1,
                names: vec!["bun".into(), "postgres".into()],
                pids: vec![41, 42],
                orphaned,
            },
            ts: now,
        };
        s.worktree_procs
            .insert(worktree_key("h", "/w/old"), procs("/w/old", false));
        s.worktree_procs
            .insert(worktree_key("h", "/w/gone"), procs("/w/gone", true));
        let f = findings(&s, now);
        assert_eq!(
            kinds(&f),
            vec![("orphan", "h:/w/gone"), ("forgotten", "h:/w/old")]
        );
        assert!(f[0].detail.ends_with("kill 41 42"), "{}", f[0].detail);
        assert!(
            f[1].detail
                .contains("still runs 2 processes (bun, postgres), 300 MB"),
            "{}",
            f[1].detail
        );
        assert!(
            !f[1].detail.contains("remove it"),
            "stop what runs first: {}",
            f[1].detail
        );
    }

    fn leftover_state(agent_in_it: bool) -> (State, DateTime<Utc>) {
        use crate::event::WorktreeProcs;
        use crate::state::Procs;
        let now = Utc::now();
        let mut s = State::default();
        let key = worktree_key("h", "/w/fresh");
        let mut w = tree("/w/fresh", &[], 0, 0, now - Duration::hours(3));
        w.since = now - Duration::minutes(20);
        s.worktrees.insert(key.clone(), w);
        s.worktree_procs.insert(
            key,
            Procs {
                procs: WorktreeProcs {
                    host: "h".into(),
                    path: "/w/fresh".into(),
                    procs: 1,
                    rss_kb: 1024,
                    cpu_pct: 0,
                    names: vec!["server".into()],
                    pids: vec![77],
                    orphaned: false,
                },
                ts: now,
            },
        );
        if agent_in_it {
            s.agents.insert(
                "agent:h/s/a".into(),
                agent(AgentState::Working, now, "/w/fresh", 0, 1024),
            );
        }
        (s, now)
    }

    #[test]
    fn a_clean_agentless_worktree_still_running_processes_is_found() {
        let (s, now) = leftover_state(false);
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("forgotten", "h:/w/fresh")], "{f:#?}");
        assert_eq!(
            killable(&s, now, "h").get(&77).map(String::as_str),
            Some("/w/fresh")
        );
        assert!(
            f[0].detail.contains("still runs 1 process"),
            "{}",
            f[0].detail
        );
        let (mut s, now) = leftover_state(false);
        s.worktrees.values_mut().for_each(|w| w.since = now);
        assert!(findings(&s, now).is_empty(), "inside the grace");
    }

    #[test]
    fn hosts_running_different_builds_are_a_finding() {
        let now = Utc::now();
        let mut s = State::default();
        let host = |s: &mut State, name: &str, commit: Option<&str>, age: i64| {
            let h = crate::state::Host {
                disk_free: 1,
                disk_total: 1,
                agents: 0,
                commit: commit.map(str::to_string),
                behind_since: None,
                ts: now - Duration::minutes(age),
                started: None,
            };
            s.hosts.insert(name.into(), h);
        };
        host(&mut s, "a", Some("1111111aaaa"), 0);
        host(&mut s, "b", Some("1111111aaaa"), 1);
        host(&mut s, "gone", Some("0000000"), 60);
        assert!(
            findings(&s, now).is_empty(),
            "a long-silent host is not compared"
        );
        host(&mut s, "c", None, 0);
        let f = findings(&s, now);
        assert_eq!(
            kinds(&f),
            vec![("build", "a"), ("build", "b"), ("build", "c")],
            "{f:#?}"
        );
        assert_eq!(
            f[2].detail,
            "runs unknown; a 1111111, b 1111111 (agent-fleet self-update)"
        );
    }

    #[test]
    fn a_fleet_on_one_commit_behind_origin_main_is_a_finding_and_one_on_main_is_not() {
        let now = Utc::now();
        let mut s = State::default();
        let host = |s: &mut State, name: &str, behind_min: Option<i64>| {
            let h = crate::state::Host {
                disk_free: 1,
                disk_total: 1,
                agents: 0,
                commit: Some("1111111aaaa".into()),
                behind_since: behind_min.map(|m| now - Duration::minutes(m)),
                ts: now,
                started: None,
            };
            s.hosts.insert(name.into(), h);
        };
        host(&mut s, "a", None);
        host(&mut s, "b", None);
        assert!(findings(&s, now).is_empty(), "on origin/main");
        host(&mut s, "a", Some(5));
        host(&mut s, "b", Some(4));
        assert!(findings(&s, now).is_empty(), "behind, the update has time");
        host(&mut s, "a", Some(40));
        host(&mut s, "b", Some(16));
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("build", "a"), ("build", "b")], "{f:#?}");
        assert_eq!(
            f[0].detail,
            "runs 1111111, behind origin/main for 40m (agent-fleet self-update)"
        );
    }

    #[test]
    fn a_watch_notice_goes_to_the_agent_its_lead_and_the_root_once_each() {
        let now = Utc::now();
        let role = |level: Level, lead: Option<&str>| crate::state::Role {
            level,
            lead: lead.map(str::to_string),
            domain: None,
            by: "agent:h/s/root".into(),
            since: now,
        };
        let mut s = State::default();
        for id in ["agent:h/s/a", "agent:h/s/root"] {
            s.agents
                .insert(id.into(), agent(AgentState::Working, now, "/w/a", 0, 1));
        }
        s.roles
            .insert("agent:h/s/root".into(), role(Level::RootCoordinator, None));
        s.roles.insert(
            "agent:h/s/a".into(),
            role(Level::Implementer, Some("agent:h/s/lead")),
        );
        let r = crate::watch::recipients;
        assert_eq!(
            r(&s, Some("agent:h/s/a")),
            vec!["agent:h/s/a", "agent:h/s/lead", "agent:h/s/root"]
        );
        s.roles.get_mut("agent:h/s/a").unwrap().lead = None;
        assert_eq!(
            r(&s, Some("agent:h/s/a")),
            vec!["agent:h/s/a", "agent:h/s/root"],
            "no lead: the root once"
        );
        assert_eq!(
            r(&s, None),
            vec!["agent:h/s/root"],
            "an orphan nobody claims"
        );
    }

    #[test]
    fn a_hung_process_is_a_finding_and_names_its_pids_for_kill() {
        let now = Utc::now();
        let mut s = State::default();
        s.hung.insert(
            "h:30".into(),
            crate::event::ProcessHung {
                host: "h".into(),
                pid: 30,
                pids: vec![30, 31],
                agent: Some("agent:h/s/o-1".into()),
                command: Some("bash scripts/e2e.sh".into()),
                started: Some(now - Duration::minutes(20)),
                evidence: Some("0% CPU, output not a file".into()),
                since: Some(now - Duration::minutes(4)),
                checked: Some(now - Duration::minutes(1)),
            },
        );
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("hung", "h:30")], "{f:#?}");
        assert!(
            f[0].detail.contains("bash scripts/e2e.sh (pid 30")
                && f[0].detail.contains("o-1")
                && f[0].detail.contains("0% CPU")
                && f[0].detail.ends_with("agent-fleet kill 30 31"),
            "{}",
            f[0].detail
        );
        let k = hung_pids(&s, "h", now);
        assert_eq!(k.keys().copied().collect::<Vec<_>>(), vec![30, 31]);
        assert_eq!(k[&31], now - Duration::minutes(4));
        assert!(hung_pids(&s, "other", now).is_empty());
        assert!(
            hung_pids(&s, "h", now + HUNG_FRESH).is_empty(),
            "a report nobody re-checked is no proof"
        );
        assert!(
            killable(&s, now, "h").is_empty(),
            "a hung report is no orphan proof"
        );
    }

    #[test]
    fn an_occupied_worktree_running_processes_is_not_found() {
        let (s, now) = leftover_state(true);
        assert!(findings(&s, now).is_empty());
        assert!(killable(&s, now, "h").is_empty());
    }

    #[test]
    fn only_a_worktree_herdr_made_is_forgotten() {
        // A main checkout, or a linked worktree outside herdr's directory (a scratchpad): the
        // host reports neither as herdr's, dirty or clean, with processes or without.
        for main in [true, false] {
            for dirty in [2, 0] {
                let (mut s, now) = leftover_state(false);
                let w = s.worktrees.values_mut().next().unwrap();
                if main {
                    w.state.repo = w.state.path.clone();
                }
                w.state.herdr = false;
                w.state.dirty = dirty;
                w.state.last_commit = now - Duration::days(5);
                w.since = now - Duration::hours(2);
                let f = findings(&s, now);
                assert!(f.is_empty(), "main {main}, dirty {dirty}: {f:#?}");
                assert!(killable(&s, now, "h").is_empty());
            }
        }
    }

    #[test]
    fn a_worktree_dirty_only_by_submodule_pointers_says_so() {
        let now = Utc::now();
        let mut s = State::default();
        let mut w = tree("/w/sub", &[], 0, 1, now - Duration::hours(2));
        w.state.dirty_submodules = 1;
        s.worktrees.insert(worktree_key("h", "/w/sub"), w);
        let mut w = tree("/w/mix", &[], 0, 3, now - Duration::hours(2));
        w.state.dirty_submodules = 1;
        s.worktrees.insert(worktree_key("h", "/w/mix"), w);
        let f = findings(&s, now);
        let detail = |p: &str| &f.iter().find(|f| f.subject == p).unwrap().detail;
        assert!(
            detail("h:/w/sub").contains("1 dirty files (only submodule pointers moved)"),
            "{f:#?}"
        );
        assert!(
            detail("h:/w/mix").contains("3 dirty files (1 of them submodule pointers)"),
            "{f:#?}"
        );
    }

    #[test]
    fn a_clean_agentless_worktree_without_processes_is_unchanged() {
        let (mut s, now) = leftover_state(false);
        s.worktree_procs.clear();
        assert!(findings(&s, now).is_empty());
    }

    #[test]
    fn two_agents_in_one_worktree_is_duplicate_work() {
        let now = Utc::now();
        let mut s = State::default();
        s.worktrees
            .insert(worktree_key("h", "/w/a"), tree("/w/a", &[], 0, 0, now));
        for name in ["agent:h/s/x", "agent:h/s/y"] {
            s.agents
                .insert(name.into(), agent(AgentState::Working, now, "/w/a", 50, 1));
        }
        assert_eq!(kinds(&findings(&s, now)), vec![("duplicate", "h:/w/a")]);
    }

    #[test]
    fn an_agent_measured_only_by_its_cwd_is_placed_and_never_hung() {
        let now = Utc::now();
        let mut s = State::default();
        s.worktrees.insert(
            worktree_key("h", "/w/a"),
            tree("/w/a", &[], 0, 0, now - Duration::hours(2)),
        );
        let mut a = agent(AgentState::Working, now - Duration::hours(1), "/w/a", 0, 0);
        let u = a.usage.as_mut().unwrap();
        (u.rss_kb, u.cpu_pct, u.procs) = (None, None, None);
        s.agents.insert("agent:h/s/mac".into(), a.clone());
        assert_eq!(worktree_of(&s, &a).map(|(k, _)| k.as_str()), Some("h:/w/a"));
        assert!(findings(&s, now).is_empty(), "no CPU reading is not 0% CPU");
    }

    /// The host's watch proved a process of `agent` hung, just now.
    fn proc_hung(s: &mut State, agent: &str, pid: u32, now: DateTime<Utc>) {
        s.hung.insert(
            format!("h:{pid}"),
            crate::event::ProcessHung {
                host: "h".into(),
                pid,
                pids: vec![],
                agent: Some(agent.into()),
                command: None,
                started: None,
                evidence: None,
                since: None,
                checked: Some(now),
            },
        );
    }

    #[test]
    fn an_agent_at_zero_cpu_without_a_hung_process_is_not_hung() {
        // 24 processes at 0% CPU for 38 min: a check tree waiting under load (s-c844, 18 Nov).
        let now = Utc::now();
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/queued".into(),
            agent(
                AgentState::Working,
                now - Duration::hours(1),
                "/w/a",
                0,
                1024,
            ),
        );
        assert!(findings(&s, now).iter().all(|f| f.kind != "hung"));
    }

    #[test]
    fn a_macos_sampled_agent_can_be_hung() {
        // On macOS the numbers now come from `ps`, so the low-CPU read is real.
        let now = Utc::now();
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/mac".into(),
            agent(
                AgentState::Working,
                now - Duration::hours(1),
                "/w/a",
                0,
                1024,
            ),
        );
        proc_hung(&mut s, "agent:h/s/mac", 77, now);
        let f = findings(&s, now);
        let k = kinds(&f);
        assert!(k.contains(&("hung", "agent:h/s/mac")), "{k:?}");
    }

    #[test]
    fn an_agent_at_zero_cpu_is_hung_only_when_proven_or_for_three_times_as_long() {
        let now = Utc::now();
        let at = |since: Duration, proven: bool| {
            let mut s = State::default();
            s.agents.insert(
                "agent:h/s/q".into(),
                agent(AgentState::Working, now - since, "/w/a", 0, 1024),
            );
            if proven {
                proc_hung(&mut s, "agent:h/s/q", 77, now);
            }
            kinds(&findings(&s, now)).contains(&("hung", "agent:h/s/q"))
        };
        assert!(
            !at(Duration::minutes(40), false),
            "s-c844: 40 min queued, unproven"
        );
        assert!(at(Duration::minutes(40), true), "proven hung at 40 min");
        assert!(
            at(Duration::minutes(100), false),
            "100 min of nothing is hung"
        );
        assert!(
            !at(Duration::minutes(20), true),
            "proven but not yet long enough"
        );
    }

    #[test]
    fn a_stuck_finding_says_who_was_told() {
        use crate::state::Role;
        let now = Utc::now();
        let mut s = State::default();
        let role = |level: Level, lead: Option<&str>| Role {
            level,
            lead: lead.map(str::to_string),
            domain: None,
            by: "agent:h/s/root".into(),
            since: now,
        };
        let mut a = agent(AgentState::Idle, now - Duration::minutes(20), "/w/a", 0, 1);
        a.worked = Some(now - Duration::minutes(40));
        s.agents.insert("agent:h/s/a".into(), a);
        let detail = |s: &State| {
            findings(s, now)
                .into_iter()
                .find(|f| f.kind == "stalled" && f.subject == "agent:h/s/a")
                .map(|f| f.detail)
        };
        assert_eq!(
            detail(&s).as_deref(),
            Some("idle for 20m since it worked; nobody to tell")
        );
        s.roles.insert(
            "agent:h/s/a".into(),
            role(Level::Implementer, Some("agent:h/s/lead")),
        );
        assert_eq!(
            detail(&s).as_deref(),
            Some("idle for 20m since it worked; agent:h/s/lead told"),
            "the lead wins over any root"
        );
        // No lead: an up root-coordinator is told; it is never the agent itself.
        s.roles.get_mut("agent:h/s/a").unwrap().lead = None;
        let mut root = agent(AgentState::Idle, now - Duration::minutes(20), "/w/b", 0, 1);
        root.worked = Some(now - Duration::minutes(40));
        s.agents.insert("agent:h/s/root".into(), root);
        s.roles
            .insert("agent:h/s/root".into(), role(Level::RootCoordinator, None));
        assert_eq!(
            detail(&s).as_deref(),
            Some("idle for 20m since it worked; agent:h/s/root told")
        );
        let f = findings(&s, now);
        let r = f.iter().find(|f| f.subject == "agent:h/s/root").unwrap();
        assert_eq!(r.detail, "idle for 20m since it worked; nobody to tell");
    }

    #[test]
    fn a_parked_agent_is_stale_never_stalled() {
        let now = Utc::now();
        // Reached `done` and idle since: parked awaiting work — `stale` names it, `stalled` does not.
        let mut a = agent(AgentState::Idle, now - Duration::minutes(20), "/w/a", 0, 1);
        a.worked = Some(now - Duration::minutes(40));
        a.finished = Some(now - Duration::minutes(25));
        assert_eq!(stalled(&a, now, STALLED), None);
        // Working again after the finish re-arms the stall: a long idle then is one.
        a.worked = Some(now - Duration::minutes(15));
        assert!(stalled(&a, now, STALLED).is_some());
    }

    #[test]
    fn a_capacity_rule_counts_worktrees_and_a_long_wait_is_queued() {
        use crate::event::ResourceLimit;
        use crate::state::Resource;
        let now = Utc::now();
        let mut s = State::default();
        s.worktrees
            .insert(worktree_key("h", "/w/a"), tree("/w/a", &[], 0, 0, now));
        let at = |pid, cwd: &str, mins| Hold {
            pid,
            cwd: Some(cwd.into()),
            scope: None,
            since: now - Duration::minutes(mins),
        };
        let mut put = |name: &str, holder: Option<Hold>, waiting: Vec<Hold>| {
            s.resources.insert(
                worktree_key("h", name),
                Resource {
                    host: "h".into(),
                    resource: name.into(),
                    holder,
                    waiting,
                },
            );
        };
        put("sim", Some(at(1, "/w/a", 30)), vec![at(3, "/w/b", 12)]);
        put("build.1", Some(at(2, "/w/a/src/mobile", 5)), vec![]);
        let rule = |max, until| ResourceLimit {
            host: "h".into(),
            name: "native".into(),
            resources: vec!["sim".into(), "build.1".into()],
            max,
            until,
        };
        s.limits.insert("h:native".into(), rule(1, None));
        assert_eq!(
            kinds(&findings(&s, now)),
            vec![("queued", "h:sim")],
            "one worktree holding both is one holder"
        );
        s.resources.get_mut("h:build.1").unwrap().holder = Some(at(2, "/w/c", 5));
        assert_eq!(
            kinds(&findings(&s, now)),
            vec![("breach", "h:native"), ("queued", "h:sim")]
        );
        s.limits
            .insert("h:native".into(), rule(1, Some(now - Duration::minutes(1))));
        assert_eq!(
            kinds(&findings(&s, now)),
            vec![("queued", "h:sim")],
            "expired"
        );
    }

    #[test]
    fn a_role_finding_when_the_lead_is_down() {
        use crate::state::Role;
        let now = Utc::now();
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/worker".into(),
            agent(AgentState::Working, now, "/w/a", 50, 1),
        );
        s.roles.insert(
            "agent:h/s/worker".into(),
            Role {
                level: Level::Implementer,
                lead: Some("agent:h/s/lead".into()),
                domain: None,
                by: "agent:h/s/lead".into(),
                since: now,
            },
        );
        // A role for an agent that is not up is no finding on its own.
        s.roles.insert(
            "agent:h/s/ghost".into(),
            Role {
                level: Level::Implementer,
                lead: Some("agent:h/s/lead".into()),
                domain: None,
                by: "agent:h/s/lead".into(),
                since: now,
            },
        );
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("role", "agent:h/s/worker")]);
        assert_eq!(f[0].detail, "its lead agent:h/s/lead is down");
    }

    #[test]
    fn a_role_finding_when_the_lead_is_not_above() {
        use crate::state::Role;
        let now = Utc::now();
        let mut s = State::default();
        for id in ["agent:h/s/worker", "agent:h/s/lead"] {
            s.agents
                .insert(id.into(), agent(AgentState::Working, now, "/w/a", 50, 1));
        }
        let role = |lead: Option<&str>| Role {
            level: Level::DomainLead,
            lead: lead.map(str::to_string),
            domain: None,
            by: "agent:h/s/root".into(),
            since: now,
        };
        s.roles
            .insert("agent:h/s/worker".into(), role(Some("agent:h/s/lead")));
        s.roles.insert("agent:h/s/lead".into(), role(None));
        let f = findings(&s, now);
        assert_eq!(kinds(&f), vec![("role", "agent:h/s/worker")]);
        assert_eq!(
            f[0].detail,
            "reports to agent:h/s/lead, a domain-lead, but is a domain-lead"
        );
    }

    #[test]
    fn a_role_finding_for_each_extra_root_coordinator() {
        use crate::state::Role;
        let now = Utc::now();
        let mut s = State::default();
        for id in ["agent:h/s/root-a", "agent:h/s/root-b", "agent:h/s/root-c"] {
            s.agents
                .insert(id.into(), agent(AgentState::Working, now, "/w/a", 50, 1));
            s.roles.insert(
                id.into(),
                Role {
                    level: Level::RootCoordinator,
                    lead: None,
                    domain: None,
                    by: "agent:h/s/root-a".into(),
                    since: now,
                },
            );
        }
        let f = findings(&s, now);
        let subjects: Vec<&str> = f.iter().map(|f| f.subject.as_str()).collect();
        assert_eq!(subjects, ["agent:h/s/root-b", "agent:h/s/root-c"]);
        assert!(f[0].detail.contains("3 root coordinators are up"));
    }

    #[test]
    fn an_unmanaged_agent_is_a_finding() {
        let now = Utc::now();
        let mut s = State::default();
        s.unmanaged.insert(
            "h:77".into(),
            crate::state::Unmanaged {
                host: "h".into(),
                pid: 77,
                program: "devin".into(),
                cwd: Some("/w/x".into()),
                since: now - Duration::minutes(10),
            },
        );
        let f = findings(&s, now);
        let u = f.iter().find(|f| f.kind == "unmanaged").unwrap();
        assert_eq!(u.subject, "h:pid 77");
        assert!(u.detail.contains("devin") && u.detail.contains("/w/x"));
    }

    #[test]
    fn a_quota_finding_when_exhausted_or_nearly_full() {
        use crate::event::QuotaWindow;
        use crate::state::Quota;
        let now = Utc::now();
        let mut s = State::default();
        let quota = |exhausted, windows: Vec<(&str, f64, Option<DateTime<Utc>>)>| Quota {
            windows: windows
                .into_iter()
                .map(|(name, used_pct, resets_at)| QuotaWindow {
                    name: name.into(),
                    used_pct,
                    resets_at,
                })
                .collect(),
            plan: None,
            exhausted,
            read_at: now,
            ts: now,
            exhausted_since: exhausted.then_some(now),
        };
        s.quotas.insert(
            "h:claude".into(),
            quota(false, vec![("5h", 95.0, Some(now + Duration::hours(1)))]),
        );
        s.quotas
            .insert("h:codex".into(), quota(true, vec![("5h", 100.0, None)]));
        // Nearly full but already reset: no finding.
        s.quotas.insert(
            "h2:claude".into(),
            quota(false, vec![("5h", 99.0, Some(now - Duration::minutes(1)))]),
        );
        // Nearly full with no reset known: no finding.
        s.quotas
            .insert("h3:claude".into(), quota(false, vec![("day", 99.0, None)]));
        // Room to run: no finding.
        s.quotas.insert(
            "h:devin".into(),
            quota(false, vec![("week", 50.0, Some(now + Duration::days(3)))]),
        );
        let f = findings(&s, now);
        assert_eq!(
            kinds(&f),
            vec![("quota", "h:claude"), ("quota", "h:codex")],
            "{f:#?}"
        );
        assert!(f[0].detail.contains("5h 95% used until"), "{}", f[0].detail);
        assert!(f[1].detail.contains("out of quota"), "{}", f[1].detail);
    }

    #[test]
    fn a_quiet_fleet_has_no_findings() {
        let now = Utc::now();
        let mut s = State::default();
        s.worktrees
            .insert(worktree_key("h", "/w/a"), tree("/w/a", &["f"], 1, 0, now));
        s.agents.insert(
            "agent:h/s/x".into(),
            agent(AgentState::Working, now, "/w/a", 80, 1),
        );
        assert!(findings(&s, now).is_empty());
    }

    #[test]
    fn a_held_task_and_a_resource_wait_are_neither_stalled_nor_hung() {
        let now = Utc::now();
        let h = |n| now - Duration::hours(n);
        let mut s = State::default();
        s.worktrees
            .insert(worktree_key("h", "/w/a"), tree("/w/a", &[], 0, 0, h(1)));
        let mut a = agent(AgentState::Idle, h(1), "/w/a", 0, 1024);
        a.worked = Some(h(1));
        s.agents.insert("agent:h/s/held".into(), a);
        s.tasks.insert(
            "task:p/1".into(),
            crate::state::Task {
                project: "p".into(),
                title: "t".into(),
                url: None,
                state: crate::event::TaskState::Working,
                since: h(1),
                owner: Some("agent:h/s/held".into()),
                lead: None,
                blocked_by: Default::default(),
                handoffs: vec![],
            },
        );
        s.blockers.insert(
            "b1".into(),
            crate::state::Blocker {
                task: Some("task:p/1".into()),
                what: "waits for the perf run".into(),
                owner: None,
                unblocks_when: None,
                pid: None,
                started: None,
                by: "agent:h/s/x".into(),
                since: h(1),
            },
        );
        assert_eq!(
            held(&s, "agent:h/s/held").as_deref(),
            Some("held: waits for the perf run")
        );
        let f = findings(&s, now);
        assert!(
            f.iter()
                .any(|x| x.kind == "held" && x.subject == "agent:h/s/held"),
            "{f:#?}"
        );
        assert!(
            !f.iter()
                .any(|x| x.kind == "stalled" && x.subject == "agent:h/s/held")
        );
        // a working agent 1st in the sim queue behind a "~21m" e2e is a legitimate wait
        s.agents.insert(
            "agent:h/s/q".into(),
            agent(AgentState::Working, h(1), "/w/a", 0, 1024),
        );
        s.resources.insert(
            "h:sim".into(),
            crate::state::Resource {
                host: "h".into(),
                resource: "sim".into(),
                holder: Some(Hold {
                    pid: 1,
                    cwd: Some("/w/other".into()),
                    scope: Some("e2e, ~21m".into()),
                    since: h(1),
                }),
                waiting: vec![Hold {
                    pid: 2,
                    cwd: Some("/w/a/x".into()),
                    scope: None,
                    since: now - Duration::minutes(10),
                }],
            },
        );
        let q = s.agents.get("agent:h/s/q").unwrap();
        let (detail, _, eta) = resource_wait(&s, q).unwrap();
        assert!(
            detail.contains("waiting for sim behind e2e, ~21m") || detail.contains("~21m"),
            "{detail}"
        );
        assert_eq!(eta, Some(Duration::minutes(21)));
        assert!(
            hung(&s, "q", q, now).is_none(),
            "a queued agent is not hung"
        );
        let f = findings(&s, now);
        assert!(
            !f.iter()
                .any(|x| x.kind == "stalled" && x.subject == "agent:h/s/q")
        );
    }

    #[test]
    fn a_refusal_of_a_gone_pid_is_no_finding() {
        let now = Utc::now();
        let mut s = State::default();
        for (pid, reason) in [
            (7, format!("pid 7: {}", crate::kill::GONE)),
            (8, "pid 8 (node) is not yours".to_string()),
        ] {
            s.refused_kills.push(crate::state::RefusedKill {
                by: "agent:h/s/a".into(),
                host: "h".into(),
                pid,
                name: "node".into(),
                reason,
                ts: now,
            });
        }
        let f = findings(&s, now);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert!(
            f[0].kind == "refused" && f[0].detail.contains("pid 8"),
            "{f:#?}"
        );
    }

    #[test]
    fn a_failed_spawn_clears_once_the_agent_works_or_a_later_spawn_succeeds() {
        let now = Utc::now();
        let id = "agent:h/s/a";
        let spawn = |ok, since: DateTime<Utc>| crate::state::Spawn {
            request: serde_json::from_value(serde_json::json!({
                "name": "a", "agent": id, "host": "h", "session": "s", "repo": "r",
                "branch": "b", "profile": "p", "brief": "x", "why": "t"
            }))
            .unwrap(),
            by: "x".into(),
            project: "p".into(),
            result: Some(crate::event::SpawnResult {
                agent: id.into(),
                ok,
                pane: None,
                worktree: None,
                error: (!ok).then(|| "no pane".into()),
            }),
            since,
            answered: Some(since),
        };
        let failed = now - Duration::minutes(10);
        let mut s = State::default();
        s.spawns.insert("e1".into(), spawn(false, failed));
        assert_eq!(kinds(&findings(&s, now)), [("spawn", id)], "still failed");
        let mut a = agent(AgentState::Working, now, "/w/a", 0, 0);
        a.worked = Some(failed - Duration::minutes(1));
        s.agents.insert(id.into(), a.clone());
        assert_eq!(
            kinds(&findings(&s, now)),
            [("spawn", id)],
            "worked before it"
        );
        a.worked = Some(now);
        s.agents.insert(id.into(), a);
        assert_eq!(kinds(&findings(&s, now)), [], "it works now");
        s.agents.clear();
        s.spawns
            .insert("e0".into(), spawn(true, failed - Duration::minutes(1)));
        assert_eq!(
            kinds(&findings(&s, now)),
            [("spawn", id)],
            "an older ok spawn"
        );
        s.spawns.insert("e2".into(), spawn(true, now));
        assert_eq!(kinds(&findings(&s, now)), [], "a later spawn succeeded");
    }

    #[test]
    fn a_tell_waiting_on_a_busy_agent_is_an_undelivered_finding() {
        let now = Utc::now();
        let id = "agent:h/s/a";
        let msg = |since| crate::state::Msg {
            from: "agent:h/s/c".into(),
            to: id.into(),
            kind: "OK".into(),
            reference: "r".into(),
            text: "go".into(),
            since,
        };
        let mut s = State::default();
        s.agents
            .insert(id.into(), agent(AgentState::Working, now, "/w/a", 0, 0));
        s.msgs
            .insert("m1".into(), msg(now - UNDELIVERED + Duration::minutes(1)));
        assert_eq!(kinds(&findings(&s, now)), [], "fresh tell is not late yet");
        s.msgs
            .insert("m2".into(), msg(now - UNDELIVERED - Duration::minutes(3)));
        s.msgs
            .insert("m3".into(), msg(now - UNDELIVERED - Duration::minutes(1)));
        let f = findings(&s, now);
        assert_eq!(
            kinds(&f),
            [("undelivered", id)],
            "one finding per recipient"
        );
        assert!(
            f[0].detail.starts_with("2 tell(s) waiting"),
            "{}",
            f[0].detail
        );
        assert!(f[0].detail.contains("working"), "{}", f[0].detail);
    }
}
