//! `agent-fleet doctor`: what needs a human look, read off the state. Pure, like the reducer.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::event::{AgentState, Level};
use crate::state::{Agent, Hold, State, Worktree};
use crate::ui::{ago, at, dur, hold};

/// Idle, done or blocked this long: the agent is stale.
pub const STALE: Duration = Duration::hours(2);
/// Working this long with no CPU and no change in its worktree: the agent may be hung.
pub const HUNG: Duration = Duration::minutes(30);
pub const HUNG_CPU_PCT: u32 = 2;
pub const HEAVY_RSS_KB: u64 = 4 * 1024 * 1024;
pub const HEAVY_CPU_PCT: u32 = 200;
/// Commits this old and still on no remote are worth a nudge.
pub const UNPUSHED: Duration = Duration::hours(1);
/// A clean worktree nobody works in, untouched this long, is clutter.
pub const ABANDONED: Duration = Duration::days(3);
/// A refused kill stays a finding this long.
pub const REFUSED: Duration = Duration::hours(1);
/// Queued this long for a resource: worth a look at who holds it.
pub const QUEUED: Duration = Duration::minutes(10);
/// A quota window this full with its reset still ahead: slow down or it will run out.
pub const QUOTA_PCT: f64 = 90.0;
/// A spawn request unanswered this long means its host is not listening.
pub const SPAWN_UNANSWERED: Duration = Duration::minutes(5);
/// Idle or blocked this long after working: somebody should be told. Same as `host --stall-min`.
pub const STALLED: Duration = Duration::minutes(crate::spawn::STALL_MIN as i64);
/// A failed spawn stays a finding this long.
pub const SPAWN_FAILED: Duration = Duration::hours(1);
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
    /// `queued`, `refused`, `role`, `quota`, `spawn`, `unmanaged`, `stalled`.
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
    let cwd = a.usage.as_ref()?.cwd.as_deref()?;
    s.worktrees
        .iter()
        .filter(|(_, w)| {
            w.state.host == a.host
                && (cwd == w.state.path || cwd.starts_with(&format!("{}/", w.state.path)))
        })
        .max_by_key(|(_, w)| w.state.path.len())
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

/// `stalled`: idle or blocked at least `min` since it last worked. The host's push uses this
/// exact predicate, so the board and the notice can never disagree.
pub fn stalled(a: &Agent, now: DateTime<Utc>, min: Duration) -> Option<String> {
    let idle_for = now - a.since;
    (matches!(a.state, AgentState::Idle | AgentState::Blocked)
        && idle_for >= min
        && a.worked.is_some())
    .then(|| format!("idle for {}m since it worked", idle_for.num_minutes()))
}

/// `hung`: `working` for HUNG at under HUNG_CPU_PCT, its worktree unchanged as long.
pub fn hung(s: &State, a: &Agent, now: DateTime<Utc>) -> Option<String> {
    // a worktree waiting on a resource is a legitimate wait, not a hang
    if resource_wait(s, a).is_some() {
        return None;
    }
    let u = a.usage.as_ref()?;
    let wt = worktree_of(s, a);
    if a.state != AgentState::Working
        || now - a.since < HUNG
        || !u.cpu_pct.is_some_and(|c| c < HUNG_CPU_PCT)
        || !wt.is_none_or(|(_, w)| now - w.since >= HUNG)
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
        if let Some(detail) = hung(s, a, now) {
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
        if !occupied.contains_key(key) {
            if unsaved && unchanged >= UNPUSHED {
                out.push(finding(
                    "forgotten",
                    key,
                    format!(
                        "no agent for {}, {} unpushed commits, {} dirty files{running}",
                        ago(now, w.since),
                        st.unpushed,
                        st.dirty
                    ),
                ));
            } else if !unsaved && st.path != st.repo && now - st.last_commit >= ABANDONED {
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
    for k in s.refused_kills.iter().filter(|k| now - k.ts < REFUSED) {
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
            Some(r) if !r.ok && sp.answered.is_some_and(|t| now - t < SPAWN_FAILED) => {
                out.push(finding(
                    "spawn",
                    &r.agent,
                    format!("spawn failed: {}", r.error.as_deref().unwrap_or("?")),
                ));
            }
            _ => {}
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
                files: files.iter().map(|f| f.to_string()).collect(),
                last_commit: since,
                in_main: false,
            },
            since,
        }
    }

    fn kinds(f: &[Finding]) -> Vec<(&str, &str)> {
        f.iter().map(|f| (f.kind, f.subject.as_str())).collect()
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
        assert_eq!(f.len(), 7, "{f:#?}");
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
        assert_eq!(kinds(&findings(&s, now)), vec![("hung", "agent:h/s/mac")]);
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
        assert!(hung(&s, q, now).is_none(), "a queued agent is not hung");
        let f = findings(&s, now);
        assert!(
            !f.iter()
                .any(|x| x.kind == "stalled" && x.subject == "agent:h/s/q")
        );
    }
}
