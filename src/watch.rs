//! The watch tick: every process an agent started, classified `working`, `waiting`, `hung` or
//! `orphan`. One classifier, pure: the host feeds it two process tables and what it read off
//! the machine; doctor, `kill` and `agent-fleet proc` read the verdicts it left in the log.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::scan::{Proc, Procs};

/// How often the host runs the watch tick.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(120);
/// Younger processes are not classified: a command that short is not stuck.
pub const MIN_AGE: Duration = Duration::minutes(2);
/// Idle this many ticks in a row is hung.
pub const HUNG_TICKS: u32 = 2;
/// A wait this long is reported: nothing waits more than 30 min.
pub const WAIT_MAX: Duration = Duration::minutes(30);

/// The shells an agent runs its commands in; their subtrees are what the agent started.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Working,
    /// In a known queue: `on` is the resource, `since` when it joined.
    Waiting {
        on: String,
        since: DateTime<Utc>,
    },
    /// Idle this tick but not yet for `HUNG_TICKS`.
    Idle,
    Hung(Evidence),
    /// Proved orphaned; `why` is the proof.
    Orphan {
        why: String,
    },
}

/// Why a process is hung: its CPU over the last interval, when its output file last
/// changed (`None`: its output is no file), and its deepest descendant's command.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub cpu_pct: u32,
    pub output_at: Option<DateTime<Utc>>,
    pub deepest: Option<String>,
}

impl Evidence {
    pub fn text(&self, ticks_min: i64) -> String {
        let out = self.output_at.map_or("output not a file".to_string(), |t| {
            format!("no output since {}", t.format("%H:%M"))
        });
        let deepest = self
            .deepest
            .as_deref()
            .map(|d| format!("; deepest: {}", short_cmd(d)))
            .unwrap_or_default();
        format!(
            "{}% CPU, {out}, no working child for {ticks_min}m{deepest}",
            self.cpu_pct
        )
    }
}

/// A command line cut to fit a one-line notice.
pub fn short_cmd(c: &str) -> String {
    const MAX: usize = 120;
    match c.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &c[..i]),
        None => c.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    pub pid: u32,
    /// The agent that started it; `None` for an orphan no worktree claims.
    pub agent: Option<String>,
    pub verdict: Verdict,
}

/// What the host read off the machine for one tick, beside the process table.
#[derive(Debug, Default)]
pub struct Seen {
    /// Agent id → the agent program's pid.
    pub agents: Vec<(String, u32)>,
    /// Lock waiters: pid → (resource, since).
    pub waiting: HashMap<u32, (String, DateTime<Utc>)>,
    /// Proved orphans: pid → (why, the agent whose worktree it runs in).
    pub orphans: HashMap<u32, (String, Option<String>)>,
    /// Processes with a listening socket: servers, kept on purpose.
    pub listening: HashSet<u32>,
    /// When each process's stdout/stderr file last changed; `None` when it could not be read,
    /// and then nothing is proved hung.
    pub output_at: Option<HashMap<u32, DateTime<Utc>>>,
}

/// The classifier's memory between ticks: the last table and each process's idle streak.
#[derive(Debug, Default)]
pub struct Watch {
    prev: Option<(Procs, DateTime<Utc>)>,
    idle: HashMap<(u32, Option<DateTime<Utc>>), u32>,
}

impl Watch {
    /// One tick. The first only remembers the table: CPU needs two.
    pub fn step(&mut self, now: Procs, at: DateTime<Utc>, seen: &Seen) -> Vec<Classified> {
        let Some((before, then)) = self.prev.take() else {
            self.prev = Some((now, at));
            return Vec::new();
        };
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, p) in &now.0 {
            children.entry(p.ppid).or_default().push(pid);
        }
        let mut t = Tick {
            now: &now,
            before: &before,
            then,
            at,
            ms: (at - then).num_milliseconds().max(1) as u64,
            seen,
            children,
            streaks: &self.idle,
            next: HashMap::new(),
            out: Vec::new(),
        };
        for (agent, apid) in &seen.agents {
            for &shell in t.kids(*apid).to_vec().iter() {
                if SHELLS.contains(&now.0[&shell].name.as_str()) {
                    for c in t.kids(shell).to_vec() {
                        t.visit(c, agent);
                    }
                }
            }
        }
        for (&pid, (why, agent)) in &seen.orphans {
            if now.0.contains_key(&pid) && !t.out.iter().any(|c| c.pid == pid) {
                t.out.push(Classified {
                    pid,
                    agent: agent.clone(),
                    verdict: Verdict::Orphan { why: why.clone() },
                });
            }
        }
        let (mut out, next) = (t.out, t.next);
        out.sort_by_key(|c| c.pid);
        self.idle = next;
        self.prev = Some((now, at));
        out
    }
}

/// One tick's walk over the agents' trees.
struct Tick<'a> {
    now: &'a Procs,
    before: &'a Procs,
    then: DateTime<Utc>,
    at: DateTime<Utc>,
    ms: u64,
    seen: &'a Seen,
    children: HashMap<u32, Vec<u32>>,
    streaks: &'a HashMap<(u32, Option<DateTime<Utc>>), u32>,
    next: HashMap<(u32, Option<DateTime<Utc>>), u32>,
    out: Vec<Classified>,
}

impl Tick<'_> {
    fn kids(&self, pid: u32) -> &[u32] {
        self.children.get(&pid).map_or(&[], Vec::as_slice)
    }

    /// CPU ms `pid` used since the last tick; all of it when it started since.
    fn used(&self, pid: u32) -> u64 {
        let p = &self.now.0[&pid];
        let was = self.before.0.get(&pid).filter(|b| same(b, p));
        p.cpu_ms.saturating_sub(was.map_or(0, |b| b.cpu_ms))
    }

    fn fresh(&self, pid: u32) -> bool {
        self.before
            .0
            .get(&pid)
            .is_none_or(|b| !same(b, &self.now.0[&pid]))
    }

    fn wrote(&self, pid: u32) -> Option<DateTime<Utc>> {
        self.seen.output_at.as_ref()?.get(&pid).copied()
    }

    /// Working on its own: a device tool other than `simctl bootstatus`, over 1% CPU, its
    /// output file changed, or a child started since the last tick.
    fn works(&self, pid: u32) -> bool {
        let p = &self.now.0[&pid];
        let bootstatus = p.args.as_deref().is_some_and(|a| a.contains("bootstatus"));
        (crate::scan::device_tool(p) && !bootstatus)
            || self.used(pid) * 100 > self.ms
            || self.wrote(pid).is_some_and(|t| t > self.then)
            || self.kids(pid).iter().any(|&c| self.fresh(c))
    }

    fn deepest(&self, pid: u32) -> Option<(usize, u32)> {
        self.kids(pid)
            .iter()
            .map(|&c| self.deepest(c).map_or((1, c), |(d, p)| (d + 1, p)))
            .max_by_key(|&(d, p)| (d, std::cmp::Reverse(p)))
    }

    /// Classify `pid` and its subtree; `true` when anything in it is busy.
    fn visit(&mut self, pid: u32, agent: &str) -> bool {
        let p = &self.now.0[&pid];
        let old = p.started.is_some_and(|s| self.at - s >= MIN_AGE);
        let key = (pid, p.started);
        let push = |t: &mut Self, verdict| {
            t.out.push(Classified {
                pid,
                agent: Some(agent.to_string()),
                verdict,
            })
        };
        if let Some((on, since)) = self.seen.waiting.get(&pid) {
            if old {
                let verdict = Verdict::Waiting {
                    on: on.clone(),
                    since: *since,
                };
                push(self, verdict);
            }
            return true;
        }
        if self.seen.listening.contains(&pid) {
            return true;
        }
        let mut busy = false;
        for c in self.kids(pid).to_vec() {
            busy |= self.visit(c, agent);
        }
        busy |= self.works(pid);
        if !old {
            return busy;
        }
        if busy {
            push(self, Verdict::Working);
            return true;
        }
        let streak = self.streaks.get(&key).copied().unwrap_or(0) + 1;
        self.next.insert(key, streak);
        let verdict = match &self.seen.output_at {
            Some(_) if streak >= HUNG_TICKS => Verdict::Hung(Evidence {
                cpu_pct: (self.used(pid) * 100 / self.ms) as u32,
                output_at: self.wrote(pid),
                deepest: self.deepest(pid).and_then(|(_, d)| {
                    let d = &self.now.0[&d];
                    d.args.clone().or_else(|| Some(d.name.clone()))
                }),
            }),
            _ => Verdict::Idle,
        };
        push(self, verdict);
        false
    }
}

/// `lsof -F pftn` output → pid → the regular files among its listed fds.
pub fn parse_lsof_files(s: &str) -> HashMap<u32, Vec<String>> {
    let (mut out, mut pid, mut reg): (HashMap<u32, Vec<String>>, Option<u32>, bool) =
        (HashMap::new(), None, false);
    for l in s.lines() {
        match l.split_at_checked(1).unwrap_or(("", "")) {
            ("p", v) => pid = v.parse().ok(),
            ("f", _) => reg = false,
            ("t", v) => reg = v == "REG",
            ("n", v) if reg => {
                if let Some(p) = pid {
                    out.entry(p).or_default().push(v.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// `lsof -F p` output → the pids it lists.
pub fn parse_lsof_pids(s: &str) -> HashSet<u32> {
    s.lines()
        .filter_map(|l| l.strip_prefix('p')?.parse().ok())
        .collect()
}

fn same(a: &Proc, b: &Proc) -> bool {
    a.started == b.started && a.name == b.name
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: u32 = 10;
    const SHELL: u32 = 20;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-16T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// pid, ppid, name, args, cpu ms, minutes old at t0.
    fn table(list: &[(u32, u32, &str, &str, u64, i64)]) -> Procs {
        Procs(
            list.iter()
                .map(|&(pid, ppid, name, args, cpu_ms, age)| {
                    (
                        pid,
                        Proc {
                            ppid,
                            cpu_ms,
                            rss_kb: 1,
                            name: name.into(),
                            cwd: None,
                            deleted: false,
                            args: Some(args.into()),
                            started: Some(t0() - Duration::minutes(age)),
                            pgid: None,
                        },
                    )
                })
                .collect(),
        )
    }

    /// The pane shell (1) → the agent (10) → its tool shell (20) → `rest`.
    fn tree(rest: &[(u32, u32, &str, &str, u64, i64)]) -> Procs {
        let mut v = vec![
            (1, 0, "zsh", "-zsh", 0, 60),
            (AGENT, 1, "claude", "claude", 0, 60),
            (15, AGENT, "node", "node context7-mcp", 0, 60),
            (
                SHELL,
                AGENT,
                "zsh",
                "/bin/zsh -c eval 'scripts/e2e.sh'",
                0,
                30,
            ),
        ];
        v.extend_from_slice(rest);
        table(&v)
    }

    fn seen() -> Seen {
        Seen {
            agents: vec![("agent:h/s/o-1".into(), AGENT)],
            output_at: Some(HashMap::new()),
            ..Seen::default()
        }
    }

    fn at(min: i64) -> DateTime<Utc> {
        t0() + Duration::minutes(min)
    }

    fn verdict(v: &[Classified], pid: u32) -> Option<&Verdict> {
        v.iter().find(|c| c.pid == pid).map(|c| &c.verdict)
    }

    /// e2e.sh (30) holds the lock; its only child is `simctl bootstatus` (31).
    fn wedged(cpu: u64) -> Procs {
        tree(&[
            (30, SHELL, "bash", "bash scripts/e2e.sh", cpu, 20),
            (31, 30, "simctl", "xcrun simctl bootstatus ABC -b", 0, 20),
        ])
    }

    #[test]
    fn a_lone_bootstatus_under_a_lock_holder_is_hung_after_two_ticks() {
        let mut w = Watch::default();
        assert!(
            w.step(wedged(0), at(0), &seen()).is_empty(),
            "first tick only remembers"
        );
        let v = w.step(wedged(0), at(2), &seen());
        assert_eq!(
            verdict(&v, 30),
            Some(&Verdict::Idle),
            "one idle tick is not hung"
        );
        let v = w.step(wedged(0), at(4), &seen());
        let Some(Verdict::Hung(e)) = verdict(&v, 30) else {
            panic!("{v:#?}")
        };
        assert_eq!(e.cpu_pct, 0);
        assert_eq!(e.deepest.as_deref(), Some("xcrun simctl bootstatus ABC -b"));
        assert!(matches!(verdict(&v, 31), Some(Verdict::Hung(_))));
        assert_eq!(v[0].agent.as_deref(), Some("agent:h/s/o-1"));
    }

    #[test]
    fn a_cpu_busy_java_child_keeps_the_tree_working() {
        let mut w = Watch::default();
        let busy = |cpu| {
            tree(&[
                (30, SHELL, "bash", "bash scripts/e2e.sh", 0, 20),
                (32, 30, "java", "java -classpath x Main", cpu, 20),
            ])
        };
        w.step(busy(0), at(0), &seen());
        w.step(busy(60_000), at(2), &seen());
        let v = w.step(busy(120_000), at(4), &seen());
        assert_eq!(verdict(&v, 30), Some(&Verdict::Working));
        assert_eq!(verdict(&v, 32), Some(&Verdict::Working));
    }

    #[test]
    fn a_device_tool_by_name_and_a_growing_output_file_are_working() {
        let mut w = Watch::default();
        let t = || {
            tree(&[
                (30, SHELL, "xcodebuild", "xcodebuild test", 0, 20),
                (40, SHELL, "bun", "bun test", 0, 20),
            ])
        };
        let mut s = seen();
        w.step(t(), at(0), &s);
        for m in [2, 4] {
            s.output_at = Some([(40, at(m) - Duration::seconds(5))].into());
            let v = w.step(t(), at(m), &s);
            assert_eq!(verdict(&v, 30), Some(&Verdict::Working), "xcodebuild");
            assert_eq!(verdict(&v, 40), Some(&Verdict::Working), "output grew");
        }
    }

    #[test]
    fn a_shell_that_keeps_starting_children_is_working() {
        let mut w = Watch::default();
        let poll = |child| {
            tree(&[
                (30, SHELL, "bash", "bash poll.sh", 0, 20),
                (child, 30, "sleep", "sleep 5", 0, 0),
            ])
        };
        w.step(poll(100), at(0), &seen());
        w.step(poll(101), at(2), &seen());
        let v = w.step(poll(102), at(4), &seen());
        assert_eq!(verdict(&v, 30), Some(&Verdict::Working));
    }

    #[test]
    fn a_lock_waiter_is_waiting_with_its_queue_and_its_ancestors_are_not_hung() {
        let mut w = Watch::default();
        let mut s = seen();
        s.waiting.insert(30, ("sim".into(), at(-40)));
        for m in [0, 2, 4] {
            let v = w.step(wedged(0), at(m), &s);
            if m == 4 {
                assert_eq!(
                    verdict(&v, 30),
                    Some(&Verdict::Waiting {
                        on: "sim".into(),
                        since: at(-40)
                    })
                );
                assert_eq!(verdict(&v, 31), None, "inside a wait, not classified");
            }
        }
    }

    #[test]
    fn the_agent_its_shells_and_its_mcp_servers_are_never_classified() {
        let mut w = Watch::default();
        for m in [0, 2, 4, 6] {
            let v = w.step(wedged(0), at(m), &seen());
            for pid in [1, AGENT, 15, SHELL] {
                assert_eq!(verdict(&v, pid), None, "pid {pid} at {m}");
            }
        }
    }

    #[test]
    fn young_processes_servers_and_unreadable_outputs_are_never_hung() {
        let t = || {
            tree(&[
                (30, SHELL, "bash", "bash scripts/e2e.sh", 0, 1),
                (50, SHELL, "bun", "bun dev", 0, 20),
            ])
        };
        let mut w = Watch::default();
        let mut s = seen();
        s.listening.insert(50);
        for m in [0, 2, 4] {
            let v = w.step(t(), at(m), &s);
            assert_eq!(verdict(&v, 50), None, "a server");
            if m < 2 {
                assert_eq!(verdict(&v, 30), None, "too young");
            }
        }
        let mut w = Watch::default();
        let s = Seen {
            output_at: None,
            ..seen()
        };
        for m in [0, 2, 4, 6] {
            let v = w.step(wedged(0), at(m), &s);
            assert!(
                !v.iter().any(|c| matches!(c.verdict, Verdict::Hung(_))),
                "{v:#?}"
            );
        }
    }

    #[test]
    fn a_proved_orphan_is_an_orphan_of_the_agent_whose_worktree_it_runs_in() {
        let mut w = Watch::default();
        let mut s = seen();
        let mut t = wedged(0);
        t.0.insert(
            70,
            Proc {
                ppid: 1,
                ..t.0[&31].clone()
            },
        );
        s.orphans.insert(
            70,
            ("sim lock owner dead".into(), Some("agent:h/s/o-1".into())),
        );
        w.step(wedged(0), at(0), &s);
        let v = w.step(t, at(2), &s);
        let c = v.iter().find(|c| c.pid == 70).expect("classified");
        assert_eq!(
            c.verdict,
            Verdict::Orphan {
                why: "sim lock owner dead".into()
            }
        );
        assert_eq!(c.agent.as_deref(), Some("agent:h/s/o-1"));
    }

    #[test]
    fn work_in_between_resets_the_idle_streak() {
        let mut w = Watch::default();
        w.step(wedged(0), at(0), &seen());
        w.step(wedged(0), at(2), &seen());
        w.step(wedged(60_000), at(4), &seen());
        let v = w.step(wedged(60_000), at(6), &seen());
        assert_eq!(verdict(&v, 30), Some(&Verdict::Idle));
    }

    #[test]
    fn lsof_output_parses_to_regular_files_and_pids() {
        let s = "p30\nf1\ntREG\nn/tmp/out.log\nf2\ntCHR\nn/dev/null\np31\nf1\ntPIPE\nn->0x1\n";
        assert_eq!(
            parse_lsof_files(s),
            [(30, vec!["/tmp/out.log".to_string()])].into()
        );
        assert_eq!(parse_lsof_pids("p5\nf3\np7\n"), [5, 7].into());
    }

    #[test]
    fn evidence_reads_as_one_line() {
        let e = Evidence {
            cpu_pct: 0,
            output_at: None,
            deepest: Some("xcrun simctl bootstatus ABC -b".into()),
        };
        assert_eq!(
            e.text(4),
            "0% CPU, output not a file, no working child for 4m; deepest: xcrun simctl bootstatus ABC -b"
        );
    }
}
