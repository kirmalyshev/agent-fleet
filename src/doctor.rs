//! `agent-fleet doctor`: what needs a human look, read off the state. Pure, like the reducer.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::event::AgentState;
use crate::state::{Agent, State, Worktree};
use crate::ui::{ago, dur};

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
    /// `stale`, `hung`, `heavy`, `duplicate`, `forgotten`, `unpushed`, `orphan`.
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
        let Some(u) = &a.usage else { continue };
        if a.state == AgentState::Working
            && idle_for >= HUNG
            && u.cpu_pct < HUNG_CPU_PCT
            && wt.is_none_or(|(_, w)| now - w.since >= HUNG)
        {
            let still = wt.map_or(String::new(), |(_, w)| {
                format!(", its worktree unchanged for {}", ago(now, w.since))
            });
            out.push(finding(
                "hung",
                id,
                format!(
                    "working for {} at {}% CPU{still}",
                    ago(now, a.since),
                    u.cpu_pct
                ),
            ));
        }
        if u.rss_kb >= HEAVY_RSS_KB || u.cpu_pct >= HEAVY_CPU_PCT {
            out.push(finding(
                "heavy",
                id,
                format!(
                    "{:.1} GB, {}% CPU over {} processes",
                    u.rss_kb as f64 / (1024.0 * 1024.0),
                    u.cpu_pct,
                    u.procs
                ),
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
                        "no agent, nothing unsaved, last commit {} ago{running}{what}",
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
            usage: Some(Usage {
                rss_kb,
                cpu_pct: cpu,
                procs: 3,
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
}
