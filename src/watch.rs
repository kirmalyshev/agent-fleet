//! The watch tick: every process an agent started, classified `working`, `waiting`, `hung` or
//! `orphan`. One classifier, pure: the host feeds it a process table each tick and what it
//! read off the machine. The host logs `hung` as `proc.hung`; doctor, `kill` and
//! `agent-fleet proc` read that, the host's `proc.orphaned` proofs and the lock queues.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::event::{Body, Event, Known, Level, MsgSent, ProcessHung};
use crate::host::Host;
use crate::scan::{Proc, Procs};
use crate::state::State;

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
    /// Idle this tick, but not yet for `HUNG_TICKS` ticks or not provably (no `output_at`).
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
    /// Processes with a listening TCP socket: servers, kept on purpose.
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
    /// The last step only remembered its table: it classified nothing.
    fresh: bool,
}

impl Watch {
    /// The table the last tick read, when that tick classified it.
    pub fn last(&self) -> Option<&Procs> {
        self.prev.as_ref().filter(|_| !self.fresh).map(|(p, _)| p)
    }

    /// One tick. The first only remembers the table: CPU needs two. So does a tick after a
    /// gap of more than two intervals (the machine slept): idle streaks only count real ticks.
    pub fn step(&mut self, now: Procs, at: DateTime<Utc>, seen: &Seen) -> Vec<Classified> {
        let Some((before, then)) = self
            .prev
            .take()
            .filter(|(_, then)| at - *then <= Duration::from_std(EVERY * 2).unwrap_or(MIN_AGE))
        else {
            (self.prev, self.fresh) = (Some((now, at)), true);
            self.idle.clear();
            return Vec::new();
        };
        self.fresh = false;
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
            visited: HashSet::new(),
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
        let (mut out, mut next, visited) = (t.out, t.next, t.visited);
        // A live process this tick did not walk (its pane unreadable) keeps its streak.
        for (k, n) in &self.idle {
            if !visited.contains(k) && now.0.get(&k.0).is_some_and(|p| p.started == k.1) {
                next.insert(*k, *n);
            }
        }
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
    visited: HashSet<(u32, Option<DateTime<Utc>>)>,
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
            || self.used(pid).saturating_mul(100) > self.ms
            || self.wrote(pid).is_some_and(|t| t > self.then)
            || self.kids(pid).iter().any(|&c| self.fresh(c))
    }

    /// Mark `pid`'s subtree walked without classifying it.
    fn skip(&mut self, pid: u32) {
        for p in self.now.tree(pid) {
            self.visited.insert((p, self.now.0[&p].started));
        }
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
        // Walked: a streak it does not renew this tick ends (a wait or a listen breaks it).
        self.visited.insert(key);
        let push = |t: &mut Self, verdict| {
            t.out.push(Classified {
                pid,
                agent: Some(agent.to_string()),
                verdict,
            })
        };
        if let Some((on, since)) = self.seen.waiting.get(&pid) {
            self.skip(pid);
            if old {
                let verdict = Verdict::Waiting {
                    on: on.clone(),
                    since: *since,
                };
                push(self, verdict);
            }
            return true;
        }
        // ponytail: "mcp" in a non-flag argv word marks an MCP server started through a shell
        // (`npx -y some-mcp`, `docker run … github-mcp-server`); a task naming mcp is skipped
        // too. Skipped, not busy: its parent can still be hung. A listener is a server it
        // keeps, and keeps its parent busy. A skipped subtree's streaks end.
        let mcp = p.args.as_deref().is_some_and(|a| {
            a.split_whitespace()
                .any(|w| !w.starts_with('-') && w.to_lowercase().contains("mcp"))
        });
        if mcp || self.seen.listening.contains(&pid) {
            self.skip(pid);
            return !mcp;
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
        // A tick that cannot see output proves nothing: the streak holds, it does not grow.
        let was = self.streaks.get(&key).copied().unwrap_or(0);
        let streak = was + u32::from(self.seen.output_at.is_some());
        self.next.insert(key, streak);
        let verdict = match &self.seen.output_at {
            Some(_) if streak >= HUNG_TICKS => Verdict::Hung(Evidence {
                cpu_pct: (self.used(pid).saturating_mul(100) / self.ms) as u32,
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

// ---------- the host's tick ----------

/// What the log says of `pid`: `hung` (a `proc.hung` report), `orphan` (a `proc.orphaned`
/// proof), `waiting` (in a lock's queue), else `no report`; with its evidence.
pub fn verdict_of(
    s: &State,
    host: &str,
    pid: u32,
    now: DateTime<Utc>,
) -> (&'static str, Option<String>) {
    if let Some(h) = s
        .hung
        .values()
        .find(|h| h.host == host && (h.pid == pid || h.pids.contains(&pid)))
    {
        return ("hung", h.evidence.clone());
    }
    if let Some(o) = s.orphans.get(&format!("{host}:{pid}")) {
        return ("orphan", Some(o.why.clone()));
    }
    for r in s.resources.values().filter(|r| r.host == host) {
        if let Some(w) = r.waiting.iter().find(|w| w.pid == pid) {
            let d = format!("for {} since {}", r.resource, crate::ui::ago(now, w.since));
            return ("waiting", Some(d));
        }
    }
    ("no report", None)
}

/// Who hears about a process: the agent that started it, its lead, and the root coordinator.
pub fn recipients(s: &State, agent: Option<&str>) -> Vec<String> {
    let root = s.agents.keys().find(|a| {
        s.roles
            .get(*a)
            .is_some_and(|r| r.level == Level::RootCoordinator)
    });
    let lead = agent.and_then(|a| crate::doctor::escalate(s, a));
    let mut out: Vec<String> = Vec::new();
    for to in [agent, lead, root.map(String::as_str)]
        .into_iter()
        .flatten()
    {
        if !out.iter().any(|o| o == to) {
            out.push(to.to_string());
        }
    }
    out
}

/// Each process matching `is` whose parent does not, with it and the matching ones under it.
pub fn tops(
    v: &[Classified],
    procs: &Procs,
    is: impl Fn(&Verdict) -> bool,
) -> Vec<(u32, Vec<u32>)> {
    let hit: HashSet<u32> = v.iter().filter(|c| is(&c.verdict)).map(|c| c.pid).collect();
    let mut out: Vec<(u32, Vec<u32>)> = hit
        .iter()
        .copied()
        .filter(|p| procs.0.get(p).is_none_or(|x| !hit.contains(&x.ppid)))
        .map(|top| {
            let mut pids: Vec<u32> = procs
                .tree(top)
                .into_iter()
                .filter(|p| hit.contains(p))
                .collect();
            pids.sort_unstable();
            (top, pids)
        })
        .collect();
    out.sort();
    out
}

/// The live agent on `host` whose worktree holds `cwd` (the deepest one).
fn agent_in(s: &State, host: &str, cwd: &str) -> Option<String> {
    s.agents
        .iter()
        .filter(|(_, a)| a.host == host)
        .filter_map(|(id, a)| {
            let (_, w) = crate::doctor::worktree_of(s, a)?;
            let p = &w.state.path;
            (cwd == p || cwd.starts_with(&format!("{p}/"))).then_some((p.len(), id))
        })
        .max()
        .map(|(_, id)| id.clone())
}

/// How long one `lsof` or `simctl` call may take before the tick gives up on it.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `cmd args`' stdout when it answered: exit 0, or exit 1 with nothing on stderr (`lsof` lists
/// nothing, or a pid exited meanwhile). An error, a signal or a timeout is no answer.
async fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let call = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(TOOL_TIMEOUT, call).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            eprintln!("agent-fleet host: watch: {cmd}: {e}");
            return None;
        }
        Err(_) => {
            eprintln!("agent-fleet host: watch: {cmd} {args:?} timed out");
            return None;
        }
    };
    let err = String::from_utf8_lossy(&out.stderr);
    match out.status.code() {
        Some(0) => {}
        Some(1) if err.trim().is_empty() => {}
        _ => {
            eprintln!("agent-fleet host: watch: {cmd} {args:?}: {}", err.trim());
            return None;
        }
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Listening processes, and when each of `pids`' stdout/stderr files last changed. `None`
/// when `lsof` gives no answer: then nothing is proved hung.
async fn machine(pids: &[u32]) -> Option<(HashSet<u32>, HashMap<u32, DateTime<Utc>>)> {
    let listening = parse_lsof_pids(&run("lsof", &["-wnP", "-iTCP", "-sTCP:LISTEN", "-Fp"]).await?);
    if pids.is_empty() {
        return Some((listening, HashMap::new()));
    }
    let csv: Vec<String> = pids.iter().map(u32::to_string).collect();
    let args = ["-wnP", "-a", "-d", "1,2", "-Fpftn", "-p", &csv.join(",")];
    let files = parse_lsof_files(&run("lsof", &args).await?);
    let output_at = files
        .into_iter()
        .filter_map(|(pid, fs)| {
            // A file it cannot stat counts as just written: unknown output proves nothing.
            let t = fs
                .iter()
                .map(|f| {
                    std::fs::metadata(f)
                        .and_then(|m| m.modified())
                        .map_or_else(|_| Utc::now(), DateTime::<Utc>::from)
                })
                .max()?;
            Some((pid, t))
        })
        .collect();
    Some((listening, output_at))
}

/// The simulators booted on this host; empty off macOS or when `simctl` gives no answer.
async fn booted() -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Some(out) = run("xcrun", &["simctl", "list", "devices", "booted"]).await else {
        return Vec::new();
    };
    let mut v: Vec<String> = crate::scan::udids(&out).into_iter().collect();
    v.sort();
    v
}

/// The `proc.hung` reports on `host` to clear. A report that is still a top (`tops`) stands;
/// so does one whose agent's tree this tick could not walk, for a while. It clears when its
/// pid is gone or is now another process, when another report now covers it, when its walked
/// tree found it working or waiting, or when no tick confirmed it for `HUNG_FRESH`.
pub fn to_clear(
    s: &State,
    host: &str,
    v: &[Classified],
    procs: &Procs,
    tops: &[(u32, Vec<u32>)],
    walked: &HashSet<String>,
    now: DateTime<Utc>,
) -> Vec<String> {
    s.hung
        .iter()
        .filter(|(_, h)| h.host == host && !tops.iter().any(|(t, _)| *t == h.pid))
        .filter(|(_, h)| {
            let Some(p) = procs.0.get(&h.pid) else {
                return true;
            };
            let covered = tops.iter().any(|(_, pids)| pids.contains(&h.pid));
            // Walked and found working or waiting; absent from the walk (reparented) proves nothing.
            let seen = h.agent.as_ref().is_some_and(|a| walked.contains(a));
            let works = v.iter().any(|c| {
                c.pid == h.pid && matches!(c.verdict, Verdict::Working | Verdict::Waiting { .. })
            });
            // Unconfirmed for HUNG_FRESH and not idle now (left the walk): nobody can re-check
            // it. One still idle (after a sleep, a blind tick) stands until the streak rebuilds.
            let idle = v
                .iter()
                .any(|c| c.pid == h.pid && c.verdict == Verdict::Idle);
            let stale = !idle
                && h.checked
                    .is_none_or(|c| now - c > crate::doctor::HUNG_FRESH);
            (h.started.is_some() && p.started != h.started) || covered || (seen && works) || stale
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// The waits past `WAIT_MAX`: (process, the lock, since).
pub fn overdue(
    v: &[Classified],
    now: DateTime<Utc>,
) -> impl Iterator<Item = (&Classified, &String, DateTime<Utc>)> {
    v.iter().filter_map(move |c| match &c.verdict {
        Verdict::Waiting { on, since } if now - *since >= WAIT_MAX => Some((c, on, *since)),
        _ => None,
    })
}

/// Who of `to` has not been told `kind` about `key` since `since`: the log is the dedupe
/// across restarts.
pub fn unsent(
    events: &[Event],
    to: Vec<String>,
    kind: &str,
    key: &str,
    since: DateTime<Utc>,
) -> Vec<String> {
    to.into_iter()
        .filter(|to| {
            !events.iter().any(|e| {
                e.ts >= since
                    && matches!(&e.body, Body::Known(Known::MsgSent(m))
                        if m.kind == kind && m.reference == key && &m.to == to)
            })
        })
        .collect()
}

/// Every `EVERY`: classify what the agents started, report hung processes, and tell the agent,
/// its lead and the root coordinator once about a hung or orphaned process and a wait past
/// `WAIT_MAX`.
pub(crate) async fn watch_loop(host: Arc<Host>) {
    let mut w = Watch::default();
    let mut tick = tokio::time::interval(EVERY);
    // A stalled tick must not fire the next ones back to back: streaks count real intervals.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(err) = once(&host, &mut w).await {
            eprintln!("agent-fleet host: watch: {err:#}");
        }
    }
}

async fn once(host: &Arc<Host>, w: &mut Watch) -> anyhow::Result<()> {
    let shells = host.pane_shells().await?;
    let procs = Procs::read();
    if procs.0.is_empty() {
        anyhow::bail!("no process table");
    }
    let view = host.view()?;
    let s = &view.state;
    let me = host.cfg.host.clone();
    let walked: HashSet<String> = shells.iter().map(|(a, _)| a.clone()).collect();
    let mut seen = Seen::default();
    for (agent, shell) in &shells {
        for (&pid, p) in &procs.0 {
            if p.ppid == *shell {
                seen.agents.push((agent.clone(), pid));
            }
        }
    }
    let mut holders: HashMap<u32, String> = HashMap::new();
    for (name, dir) in &host.cfg.locks {
        let l = crate::scan::read_lock(dir);
        if let Some(h) = &l.holder {
            holders.insert(h.pid, name.clone());
        }
        for q in l.waiting {
            seen.waiting.insert(q.pid, (name.clone(), q.since));
        }
    }
    for o in s.orphans.values().filter(|o| o.host == me) {
        let cwd = o.cwd.clone().or_else(|| crate::scan::cwd_of(o.pid));
        let agent = cwd.and_then(|c| agent_in(s, &me, &c));
        seen.orphans.insert(o.pid, (o.why.clone(), agent));
    }
    let pids: Vec<u32> = seen
        .agents
        .iter()
        .flat_map(|(_, a)| procs.tree(*a))
        .collect();
    match machine(&pids).await {
        Some((listening, output_at)) => {
            seen.listening = listening;
            seen.output_at = Some(output_at);
        }
        None => seen.output_at = None,
    }
    let now = Utc::now();
    let v = w.step(procs, now, &seen);
    // A tick that only primed classified nothing: it clears and tells nothing either.
    let Some(procs) = w.last() else {
        return Ok(());
    };
    let of = |pid: u32| v.iter().find(|c| c.pid == pid);
    let up = |pid: u32| {
        procs
            .0
            .get(&pid)
            .map_or((String::from("?"), String::new()), |p| {
                let cmd = short_cmd(p.args.as_deref().unwrap_or(&p.name));
                let age = p
                    .started
                    .map(|t| format!(", up {}", crate::ui::ago(now, t)))
                    .unwrap_or_default();
                (cmd, age)
            })
    };
    // Hung: one report per topmost hung process.
    let hung = tops(&v, procs, |v| matches!(v, Verdict::Hung(_)));
    let mins = EVERY.as_secs() as i64 * HUNG_TICKS as i64 / 60;
    for (top, pids) in &hung {
        let Some(c) = of(*top) else { continue };
        let Verdict::Hung(e) = &c.verdict else {
            continue;
        };
        let key = format!("{me}:{top}");
        let mut evidence = e.text(mins);
        if let Some((_, r)) = holders
            .iter()
            .find(|(h, _)| pids.contains(h) || procs.under_any(*top, &[**h].into()))
        {
            evidence += &format!("; holds the {r} lock");
        }
        let p = &procs.0[top];
        let known = s.hung.get(&key).filter(|k| k.started == p.started);
        let since = known.and_then(|k| k.since).unwrap_or(now);
        // Every tick it is confirmed: `checked` keeps it kill proof.
        {
            let ok = host
                .publish(host.presence(Known::ProcessHung(ProcessHung {
                    host: me.clone(),
                    pid: *top,
                    pids: pids.clone(),
                    agent: c.agent.clone(),
                    command: Some(p.args.clone().unwrap_or_else(|| p.name.clone())),
                    started: p.started,
                    evidence: Some(evidence.clone()),
                    since: Some(since),
                    checked: Some(now),
                })))
                .await;
            // Its kill line only works once the report is in the log.
            if !ok && known.is_none() {
                continue;
            }
        }
        let (cmd, age) = up(*top);
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        let text = format!(
            "{cmd} (pid {top}{age}) is hung: {evidence} — kill: agent-fleet kill {}",
            list.join(" ")
        );
        tell(
            host,
            &view.events,
            s,
            c.agent.as_deref(),
            "HUNG",
            &key,
            since,
            text,
        )
        .await;
    }
    for key in to_clear(s, &me, &v, procs, &hung, &walked, now) {
        let pid = s.hung[&key].pid;
        host.publish(host.presence(Known::ProcessHung(ProcessHung {
            host: me.clone(),
            pid,
            pids: Vec::new(),
            agent: None,
            command: None,
            started: None,
            evidence: None,
            since: None,
            checked: None,
        })))
        .await;
    }
    // Orphans: one tell per topmost orphan.
    let orphans = tops(&v, procs, |v| matches!(v, Verdict::Orphan { .. }));
    let lock_dead = orphans.iter().any(|(t, _)| {
        of(*t).is_some_and(
            |c| matches!(&c.verdict, Verdict::Orphan { why } if why.contains("lock owner dead")),
        )
    });
    let devices = if lock_dead {
        booted().await
    } else {
        Vec::new()
    };
    for (top, pids) in orphans {
        let Some(c) = of(top) else { continue };
        let Verdict::Orphan { why } = &c.verdict else {
            continue;
        };
        let key = format!("{me}:{top}");
        let Some(o) = s.orphans.get(&key) else {
            continue;
        };
        let (cmd, age) = up(top);
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        let mut text = format!(
            "{cmd} (pid {top}{age}) is an orphan: {why} — kill: agent-fleet kill {}",
            list.join(" ")
        );
        if why.contains("lock owner dead") && !devices.is_empty() {
            let d = devices.join(" ");
            text += &format!(
                "; simulators booted on this host, which may keep its holder alive: {d} — \
                 if they are this run's, release: xcrun simctl shutdown <udids>"
            );
        }
        tell(
            host,
            &view.events,
            s,
            c.agent.as_deref(),
            "ORPHAN",
            &key,
            o.since,
            text,
        )
        .await;
    }
    for (c, on, since) in overdue(&v, now) {
        let key = format!("{me}:{}", c.pid);
        let (cmd, _) = up(c.pid);
        let behind = holders
            .iter()
            .find(|(_, r)| *r == on)
            .map(|(h, _)| format!(" behind pid {h}"))
            .unwrap_or_default();
        let text = format!(
            "{cmd} (pid {}) has waited {} for the {on} lock{behind}; nothing waits over {}m",
            c.pid,
            crate::ui::ago(now, since),
            WAIT_MAX.num_minutes()
        );
        tell(
            host,
            &view.events,
            s,
            c.agent.as_deref(),
            "WAITING",
            &key,
            since,
            text,
        )
        .await;
    }
    Ok(())
}

/// A `msg.sent` of `kind` about `key` to each recipient not yet told since `since`.
#[allow(clippy::too_many_arguments)]
async fn tell(
    host: &Arc<Host>,
    events: &[Event],
    s: &State,
    agent: Option<&str>,
    kind: &str,
    key: &str,
    since: DateTime<Utc>,
    text: String,
) {
    for to in unsent(events, recipients(s, agent), kind, key, since) {
        host.publish(host.presence(Known::MsgSent(MsgSent {
            to,
            kind: kind.into(),
            reference: key.into(),
            text: text.clone(),
        })))
        .await;
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
    fn a_hung_chain_is_one_report_at_its_top() {
        let mut w = Watch::default();
        for m in [0, 2] {
            w.step(wedged(0), at(m), &seen());
        }
        let v = w.step(wedged(0), at(4), &seen());
        let t = tops(&v, w.last().unwrap(), |v| matches!(v, Verdict::Hung(_)));
        assert_eq!(t, vec![(30, vec![30, 31])]);
    }

    #[test]
    fn the_logged_verdict_of_a_pid_matches_what_doctor_reads() {
        let mut s = State::default();
        s.hung.insert(
            "h:30".into(),
            ProcessHung {
                host: "h".into(),
                pid: 30,
                pids: vec![30, 31],
                agent: None,
                command: None,
                started: None,
                evidence: Some("0% CPU".into()),
                since: None,
                checked: None,
            },
        );
        assert_eq!(
            verdict_of(&s, "h", 31, t0()),
            ("hung", Some("0% CPU".into()))
        );
        assert_eq!(verdict_of(&s, "other", 31, t0()).0, "no report");
        assert_eq!(verdict_of(&s, "h", 99, t0()), ("no report", None));
    }

    #[test]
    fn an_mcp_server_started_through_a_shell_is_never_classified() {
        let t = || {
            tree(&[
                (25, AGENT, "sh", "sh -c npx -y some-mcp", 0, 30),
                (26, 25, "node", "node some-mcp/index.js", 0, 30),
            ])
        };
        let mut w = Watch::default();
        for m in [0, 2, 4, 6] {
            assert_eq!(verdict(&w.step(t(), at(m), &seen()), 26), None);
        }
    }

    #[test]
    fn a_gap_longer_than_two_ticks_only_reprimes() {
        let mut w = Watch::default();
        w.step(wedged(0), at(0), &seen());
        assert_eq!(
            verdict(&w.step(wedged(0), at(2), &seen()), 30),
            Some(&Verdict::Idle)
        );
        assert!(
            w.step(wedged(0), at(30), &seen()).is_empty(),
            "slept: no verdicts"
        );
        assert!(
            w.last().is_none(),
            "a primed tick hands the host nothing to act on"
        );
    }

    #[test]
    fn a_stale_output_file_is_hung_and_named_in_the_evidence() {
        let mut w = Watch::default();
        let mut s = seen();
        s.output_at = Some([(30, at(-30))].into());
        for m in [0, 2] {
            w.step(wedged(0), at(m), &s);
        }
        let Some(Verdict::Hung(e)) = verdict(&w.step(wedged(0), at(4), &s), 30).cloned() else {
            panic!("not hung")
        };
        assert_eq!(e.output_at, Some(at(-30)));
        assert!(e.text(4).contains("no output since 06:30"), "{}", e.text(4));
    }

    #[test]
    fn a_tree_left_unwalked_for_a_tick_keeps_its_streak() {
        let mut w = Watch::default();
        let none = Seen {
            agents: Vec::new(),
            ..seen()
        };
        w.step(wedged(0), at(0), &seen());
        w.step(wedged(0), at(2), &seen());
        assert_eq!(verdict(&w.step(wedged(0), at(4), &none), 30), None);
        assert!(matches!(
            verdict(&w.step(wedged(0), at(6), &seen()), 30),
            Some(Verdict::Hung(_))
        ));
    }

    fn report(pid: u32, started: Option<DateTime<Utc>>) -> ProcessHung {
        ProcessHung {
            host: "h".into(),
            pid,
            pids: vec![pid],
            agent: Some("agent:h/s/o-1".into()),
            command: None,
            started,
            evidence: Some("0% CPU".into()),
            since: Some(at(-2)),
            checked: Some(at(-1)),
        }
    }

    #[test]
    fn a_report_clears_only_on_proof_it_no_longer_holds() {
        let procs = wedged(0);
        let started = procs.0[&30].started;
        let walked: HashSet<String> = ["agent:h/s/o-1".to_string()].into();
        let c = |verdict| {
            vec![Classified {
                pid: 30,
                agent: Some("agent:h/s/o-1".into()),
                verdict,
            }]
        };
        let mut s = State::default();
        s.hung.insert("h:30".into(), report(30, started));
        let clear =
            |s: &State,
             v: &[Classified],
             procs: &Procs,
             tops: &[(u32, Vec<u32>)],
             w: &HashSet<String>| { to_clear(s, "h", v, procs, tops, w, at(0)) };
        let idle = c(Verdict::Idle);
        assert!(
            clear(&s, &idle, &procs, &[], &walked).is_empty(),
            "still idle after a restart"
        );
        assert!(
            clear(&s, &c(Verdict::Working), &procs, &[(30, vec![30])], &walked).is_empty(),
            "a top stands"
        );
        assert_eq!(
            clear(&s, &c(Verdict::Working), &procs, &[], &walked),
            vec!["h:30"],
            "works again"
        );
        assert!(
            clear(&s, &[], &procs, &[], &walked).is_empty(),
            "out of the walked tree (reparented) is not working"
        );
        assert!(
            clear(&s, &[], &procs, &[], &HashSet::new()).is_empty(),
            "its tree was not walked"
        );
        assert_eq!(
            clear(&s, &[], &table(&[]), &[], &HashSet::new()),
            vec!["h:30"],
            "gone"
        );
        s.hung.insert("h:30".into(), report(30, Some(at(-99))));
        assert_eq!(
            clear(&s, &idle, &procs, &[], &walked),
            vec!["h:30"],
            "a reused pid"
        );
        s.hung.insert("h:30".into(), report(30, started));
        assert_eq!(
            clear(&s, &idle, &procs, &[(20, vec![20, 30])], &walked),
            vec!["h:30"],
            "covered by its parent's report"
        );
        s.hung.get_mut("h:30").unwrap().checked = Some(at(-7));
        assert!(
            clear(&s, &idle, &procs, &[], &walked).is_empty(),
            "unconfirmed but still idle (after a sleep): it stands, the tell is not repeated"
        );
        assert_eq!(
            clear(&s, &[], &procs, &[], &HashSet::new()),
            vec!["h:30"],
            "no tick confirmed it for HUNG_FRESH"
        );
    }

    #[test]
    fn a_tick_without_lsof_neither_proves_nor_grows_a_streak() {
        let mut w = Watch::default();
        let blind = Seen {
            output_at: None,
            ..seen()
        };
        w.step(wedged(0), at(0), &seen());
        assert_eq!(
            verdict(&w.step(wedged(0), at(2), &blind), 30),
            Some(&Verdict::Idle)
        );
        assert_eq!(
            verdict(&w.step(wedged(0), at(4), &seen()), 30),
            Some(&Verdict::Idle),
            "one seen idle tick is not two"
        );
        assert!(matches!(
            verdict(&w.step(wedged(0), at(6), &seen()), 30),
            Some(Verdict::Hung(_))
        ));
    }

    #[test]
    fn a_waiter_is_overdue_past_thirty_minutes() {
        let w = |min: i64| Classified {
            pid: 30,
            agent: None,
            verdict: Verdict::Waiting {
                on: "sim".into(),
                since: at(-min),
            },
        };
        assert_eq!(overdue(&[w(29)], at(0)).count(), 0);
        let v = [w(31)];
        let late: Vec<_> = overdue(&v, at(0)).collect();
        assert_eq!(late.len(), 1);
        assert_eq!((late[0].1.as_str(), late[0].2), ("sim", at(-31)));
    }

    #[test]
    fn a_notice_is_sent_once_per_recipient_since_its_episode_began() {
        let sent = |ts: DateTime<Utc>, to: &str| {
            let mut e = crate::event::Envelope::new(
                crate::event::Actor {
                    kind: crate::event::ActorKind::Host,
                    id: "host:h".into(),
                },
                "p",
                Known::MsgSent(MsgSent {
                    to: to.into(),
                    kind: "HUNG".into(),
                    reference: "h:30".into(),
                    text: String::new(),
                }),
            )
            .into_event();
            e.ts = ts;
            e
        };
        let log = [sent(at(-10), "b"), sent(at(1), "a")];
        let to = || vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            unsent(&log, to(), "HUNG", "h:30", at(0)),
            vec!["b"],
            "b's was an older episode"
        );
        assert_eq!(unsent(&log, to(), "WAITING", "h:30", at(0)), to());
    }

    #[test]
    fn the_logged_orphan_and_wait_verdicts_read_back() {
        let mut s = State::default();
        s.orphans.insert(
            "h:70".into(),
            crate::state::Orphan {
                host: "h".into(),
                pid: 70,
                cwd: None,
                why: "sim lock owner dead".into(),
                since: at(0),
            },
        );
        s.resources.insert(
            "h:sim".into(),
            crate::state::Resource {
                host: "h".into(),
                resource: "sim".into(),
                holder: None,
                waiting: vec![crate::state::Hold {
                    pid: 40,
                    cwd: None,
                    scope: None,
                    since: at(-40),
                }],
            },
        );
        assert_eq!(
            verdict_of(&s, "h", 70, at(0)),
            ("orphan", Some("sim lock owner dead".into()))
        );
        assert_eq!(verdict_of(&s, "h", 40, at(0)).0, "waiting");
        assert!(
            verdict_of(&s, "h", 40, at(0))
                .1
                .unwrap()
                .starts_with("for sim since")
        );
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
