//! `agent-fleet kill`, and `hook bash`, which refuses raw kills, agents started outside herdr and
//! herdr writes into another agent's pane. A process is killed only when it
//! is the caller's own, an orphan doctor proved, or a process the watch reported hung and `kill`'s
//! own sample finds idle; never an agent, a multiplexer pane, the fleet's own daemons, or anything
//! above one of them.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Proc {
    pub ppid: u32,
    pub name: String,
}

/// Every process on this machine: pid → parent and name.
pub type Table = HashMap<u32, Proc>;

/// Programs that are agents wherever they run.
/// The agent programs: anything running one of these outside a herdr pane is unmanaged.
pub(crate) const AGENTS: &[&str] = &["claude", "devin", "codex", "agy"];
/// Terminal multiplexers whose panes hold agents.
const MUX: &[&str] = &["herdr"];

/// `ps -A -o pid= -o ppid= -o comm=`, on Linux and macOS alike.
pub fn table() -> Result<Table> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "comm="])
        .output()
        .context("run ps")?;
    if !out.status.success() {
        bail!("ps failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_ps(s: &str) -> Table {
    s.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let ppid = f.next()?.parse().ok()?;
            let comm = f.collect::<Vec<_>>().join(" ");
            // macOS gives a path, and a login shell is `-zsh`.
            let name = comm.rsplit('/').next()?.trim_start_matches('-').to_string();
            Some((pid, Proc { ppid, name }))
        })
        .collect()
}

fn name(t: &Table, pid: u32) -> &str {
    t.get(&pid).map_or("?", |p| p.name.as_str())
}

fn parent(t: &Table, pid: u32) -> Option<u32> {
    t.get(&pid).map(|p| p.ppid).filter(|&p| p != pid && p > 0)
}

/// Parent, grandparent… up to the root. Bounded, so a cycle in a racy table cannot hang it.
fn ancestors(t: &Table, pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut at = pid;
    while let Some(p) = parent(t, at) {
        if out.contains(&p) || out.len() > 512 {
            break;
        }
        out.push(p);
        at = p;
    }
    out
}

fn is_mux(t: &Table, pid: u32) -> bool {
    MUX.contains(&name(t, pid))
}

/// A shell a multiplexer opened for a pane.
fn is_pane(t: &Table, pid: u32) -> bool {
    parent(t, pid).is_some_and(|p| is_mux(t, p))
}

/// An agent: a known agent program, or whatever a pane's shell runs.
fn is_agent(t: &Table, pid: u32) -> bool {
    AGENTS.contains(&name(t, pid)) || parent(t, pid).is_some_and(|p| is_pane(t, p))
}

/// The agent whose process tree holds `pid`: its nearest agent ancestor.
fn agent_of(t: &Table, pid: u32) -> Option<u32> {
    ancestors(t, pid).into_iter().find(|&a| is_agent(t, a))
}

/// The command `pid` is part of: the shell an agent runs it in (`pid` or its nearest ancestor
/// right under an agent); `None` when no agent is above it.
pub fn command_of(t: &Table, pid: u32) -> Option<u32> {
    std::iter::once(pid)
        .chain(ancestors(t, pid))
        .find(|&p| parent(t, p).is_some_and(|a| is_agent(t, a)))
}

/// Why no caller may ever signal `pid`: an agent, a pane, a multiplexer or daemon, or anything
/// above one. What the watch reports hung must pass this, or its own `kill` line is refused.
pub fn never_killable(t: &Table, pid: u32) -> Option<String> {
    protected(t, 0, pid)
}
/// Why `pid` must never be signalled by `me`, if it must not.
fn protected(t: &Table, me: u32, pid: u32) -> Option<String> {
    let what = |p: u32| -> Option<&'static str> {
        if is_mux(t, p) {
            Some("a terminal multiplexer")
        } else if is_pane(t, p) {
            Some("a multiplexer pane's shell")
        } else if is_agent(t, p) {
            Some("an agent")
        } else if name(t, p) == "agent-fleet" && p != me {
            Some("an agent-fleet daemon")
        } else {
            None
        }
    };
    if pid <= 1 {
        return Some(format!("pid {pid} is the system's"));
    }
    if pid == me {
        return Some("that is this command".into());
    }
    if ancestors(t, me).contains(&pid) {
        let chain: Vec<&str> = ancestors(t, me)
            .into_iter()
            .take_while(|&a| a != pid)
            .chain([pid])
            .map(|a| name(t, a))
            .collect();
        return Some(format!(
            "pid {pid} ({}) is your own ancestor ({}): killing it kills you",
            name(t, pid),
            chain.join(" → ")
        ));
    }
    if let Some(w) = what(pid) {
        return Some(format!("pid {pid} ({}) is {w}", name(t, pid)));
    }
    t.keys()
        .copied()
        .filter(|&p| what(p).is_some())
        .find_map(|p| {
            ancestors(t, p).contains(&pid).then(|| {
                format!(
                    "pid {pid} ({}) is an ancestor of {} {p} ({})",
                    name(t, pid),
                    what(p).unwrap_or_default(),
                    name(t, p)
                )
            })
        })
}

/// Whether `me` may signal `pid`. `proved(foreign)` asks whether the log proves `pid` may go:
/// outside every agent's tree (`foreign` false) an orphan doctor proved or a hung report
/// counts; inside another agent's tree (`foreign` true) only a hung report does.
#[cfg(test)]
fn judge(
    t: &Table,
    me: u32,
    pid: u32,
    proved: impl Fn(bool) -> bool,
) -> std::result::Result<(), String> {
    judge_in(t, me, pid, proved, &Homes::default(), &|_| None)
}

/// An orphan: its parent is init, or a user's service manager.
fn is_orphan(t: &Table, pid: u32) -> bool {
    t.get(&pid).is_some_and(|p| {
        p.ppid == 1 || (p.ppid > 1 && matches!(name(t, p.ppid), "systemd" | "launchd"))
    })
}

fn inside(dir: &str, root: &str) -> bool {
    dir == root || dir.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// Where the fleet assigned agents to work (their spawn worktrees, or where their host first saw a
/// hand-started one), never where the caller or its pane stands now: a caller that `cd`s
/// into another agent's worktree gains nothing.
#[derive(Debug, Default, PartialEq)]
pub struct Homes {
    /// The caller's recorded worktree; `None` fails closed.
    pub mine: Option<String>,
    /// Worktrees recorded for other agents.
    pub others: Vec<String>,
}

/// The worktree a recorded agent cwd lies in: the longest of `worktrees` containing it.
fn home_in<'a>(cwd: &str, worktrees: &'a [String]) -> Option<&'a String> {
    worktrees
        .iter()
        .filter(|w| inside(cwd, w))
        .max_by_key(|w| w.len())
}

/// `agents` are (id, assigned worktree) on this host; `worktrees` the paths the fleet knows here,
/// `mains` those of them that are a repository's main checkout. A main checkout is never the
/// caller's root: its processes may be the human's (as in `doctor::killable`). It still counts
/// in `others`, which only ever refuses more.
pub fn homes(
    me: &str,
    agents: &[(String, Option<String>)],
    worktrees: &[String],
    mains: &[String],
) -> Homes {
    let home = |cwd: &Option<String>| cwd.as_deref().and_then(|c| home_in(c, worktrees)).cloned();
    Homes {
        mine: agents
            .iter()
            .find(|(id, _)| id == me)
            .and_then(|(_, c)| home(c))
            .filter(|w| !mains.contains(w)),
        others: agents
            .iter()
            .filter(|(id, _)| id != me)
            .filter_map(|(_, c)| home(c))
            .collect(),
    }
}

/// Why a kill of a pid that no longer exists is refused: nothing was there to kill.
pub const GONE: &str = "no such process";

/// `judge`, plus the caller's recorded worktree (`homes`) and a working-directory lookup: a dev server the
/// agent backgrounded outlives its Bash shell and is adopted by init, so no ancestry ties it to
/// the agent any more; an orphan whose cwd is inside `tree` still is the caller's.
pub fn judge_in(
    t: &Table,
    me: u32,
    pid: u32,
    proved: impl Fn(bool) -> bool,
    homes: &Homes,
    cwd: &dyn Fn(u32) -> Option<String>,
) -> std::result::Result<(), String> {
    if !t.contains_key(&pid) {
        return Err(format!("pid {pid}: {GONE}"));
    }
    if let Some(why) = protected(t, me, pid) {
        return Err(why);
    }
    let mine = agent_of(t, me);
    // Outside any agent (a person's shell, ssh), what that shell started is the caller's own.
    let shell = parent(t, me).filter(|_| mine.is_none());
    match agent_of(t, pid) {
        Some(a) if Some(a) == mine => Ok(()),
        Some(_) if proved(true) => Ok(()),
        Some(a) => Err(format!(
            "pid {pid} ({}) belongs to another agent's process tree: {} {a}",
            name(t, pid),
            name(t, a)
        )),
        None if shell.is_some_and(|sh| ancestors(t, pid).contains(&sh)) => Ok(()),
        None if proved(false) => Ok(()),
        None if is_orphan(t, pid) => {
            let at = cwd(pid);
            let (Some(root), Some(at)) = (homes.mine.as_deref(), at.as_deref()) else {
                return Err(if homes.mine.is_none() {
                    format!(
                        "pid {pid} ({}) is not yours: it is an orphan, but the fleet records no \
                         worktree for you to attribute it to",
                        name(t, pid)
                    )
                } else {
                    format!(
                        "pid {pid} ({}) is not yours: an orphan whose working directory is unknown",
                        name(t, pid)
                    )
                });
            };
            if let Some(o) = homes.others.iter().find(|o| *o != root && inside(at, o)) {
                Err(format!(
                    "pid {pid} ({}) is an orphan in {o}, a worktree recorded for another agent",
                    name(t, pid)
                ))
            } else if inside(at, root) {
                Ok(())
            } else {
                Err(format!(
                    "pid {pid} ({}) is not yours: an orphan outside your worktree {root}",
                    name(t, pid)
                ))
            }
        }
        None => Err(format!(
            "pid {pid} ({}) is not yours: no process of yours started it, doctor has not \
             proved it an orphan and the watch has not reported it hung",
            name(t, pid)
        )),
    }
}

/// The words of each simple command in a shell line, roughly: quotes group words, separators
/// (`;` `&` `|` newline, parentheses, backticks) split commands.
/// The `EOF` of `<<EOF`, `<<-EOF`, `<< EOF` or `cat<<EOF` — the word is already unquoted.
/// `<<<` (here-string) has no body, and `<<` inside a quoted word is not a redirect.
fn heredoc_marker(w: &str) -> Option<Option<String>> {
    let i = w.find("<<")?;
    let rest = &w[i + 2..];
    if rest.starts_with('<') || rest.starts_with(' ') {
        return None;
    }
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    if rest.is_empty() {
        Some(None) // `<< EOF`: the delimiter is the next word
    } else if rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'|'))
    {
        Some(Some(rest.to_string()))
    } else {
        None
    }
}

fn commands(line: &str) -> Vec<Vec<String>> {
    let (mut cmds, mut cmd, mut word) = (Vec::new(), Vec::new(), String::new());
    let mut quoted = false;
    // Heredoc delimiters queued by `<<` markers; while set, whole lines are body, not commands.
    let mut heredoc: std::collections::VecDeque<String> = Default::default();
    let mut chars = line.chars().peekable();
    let end_word = |cmd: &mut Vec<String>, word: &mut String, quoted: &mut bool| {
        if !word.is_empty() || *quoted {
            cmd.push(std::mem::take(word));
        }
        *quoted = false;
    };
    loop {
        if let Some(d) = heredoc.front() {
            // The whole line is heredoc body (or the delimiter that ends it).
            let mut l = String::new();
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
                l.push(c);
            }
            if l.trim_start_matches('\t') == *d {
                heredoc.pop_front();
            }
            continue;
        }
        let Some(c) = chars.next() else {
            break;
        };
        match c {
            '\'' => {
                quoted = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                quoted = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        _ => word.push(c),
                    }
                }
            }
            '\\' => word.extend(chars.next()),
            ' ' | '\t' => end_word(&mut cmd, &mut word, &mut quoted),
            ';' | '&' | '|' | '\n' | '(' | ')' | '`' | '{' | '}' => {
                if c == '(' && word == "$" {
                    word.clear();
                }
                end_word(&mut cmd, &mut word, &mut quoted);
                if !cmd.is_empty() {
                    let mut it = cmd
                        .iter()
                        .skip_while(|w| !matches!(heredoc_marker(w), Some(Some(_)) | Some(None)));
                    while let Some(w) = it.next() {
                        match heredoc_marker(w) {
                            Some(Some(d)) => heredoc.push_back(d),
                            Some(None) => {
                                if let Some(n) = it.next() {
                                    heredoc.push_back(n.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                    cmds.push(std::mem::take(&mut cmd));
                }
            }
            _ => word.push(c),
        }
    }
    end_word(&mut cmd, &mut word, &mut quoted);
    if !cmd.is_empty() {
        cmds.push(cmd);
    }
    cmds
}

const KILLERS: &[&str] = &["kill", "pkill", "killall"];
/// Wrapper options that take the next word as their value, per wrapper (`sudo -n` is a flag,
/// `nice -n 5` takes one).
fn value_opts(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "timeout" => &["-k", "-s"],
        "nice" => &["-n"],
        "exec" => &["-a"],
        "sudo" | "doas" => &["-u", "-g", "-h", "-p", "-C"],
        "env" => &["-u", "-S", "-C"],
        "stdbuf" => &["-i", "-o", "-e"],
        "xargs" => &["-n", "-s", "-P", "-I", "-i", "-L", "-d"],
        _ => &[],
    }
}

/// `timeout`'s duration: `10`, `10.5s`, `2m`, `1h`, `1d`.
fn is_duration(w: &str) -> bool {
    let (num, unit) = w
        .find(|c: char| c.is_ascii_alphabetic())
        .map_or((w, ""), |i| w.split_at(i));
    !num.is_empty() && num.parse::<f64>().is_ok() && matches!(unit, "" | "s" | "m" | "h" | "d")
}
/// Words that run the command after them.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "nohup", "command", "builtin", "exec", "env", "nice", "setsid", "time",
    "timeout", "xargs", "stdbuf", "then", "do", "else", "if", "elif", "while", "until", "!",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Each simple command in `line` gets `hit` on its argv stripped of assignments and wrappers, and
/// whether it runs on another machine (inside `ssh <host>`);
/// `hit` also sees what `sh -c`, `eval`, `ssh <host>` and `find -exec` hide inside, recursed 4
/// deep. Best effort, not a sandbox: it guards against a slip.
fn scan_cmds<T>(
    line: &str,
    depth: u32,
    remote: bool,
    hit: &impl Fn(&[String], bool) -> Option<T>,
) -> Option<T> {
    if depth > 4 {
        return None;
    }
    for words in commands(line) {
        let mut w: &[String] = &words;
        loop {
            match w.first().map(|s| base(s)) {
                Some(f) if is_assignment(f) => w = &w[1..],
                // `command -v|-V` asks where a program lives; it runs nothing.
                Some("command") if matches!(w.get(1).map(String::as_str), Some("-v" | "-V")) => {
                    break;
                }
                Some(f) if WRAPPERS.contains(&f) => {
                    let opts = value_opts(f);
                    w = &w[1..];
                    while let Some(a) = w.first() {
                        if a.starts_with('-') {
                            // Options that take a value lose it too: `sudo -u bob`, `exec -a x`,
                            // `env -u FOO`, `timeout -k 2`.
                            w = &w[if opts.contains(&a.as_str()) { 2 } else { 1 }.min(w.len())..];
                        } else if is_assignment(a) || a.parse::<f64>().is_ok() || is_duration(a) {
                            w = &w[1..];
                        } else {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        let Some(first) = w.first().map(|s| base(s)) else {
            continue;
        };
        if let Some(found) = hit(w, remote) {
            return Some(found);
        }
        let inner: Vec<String> = if SHELLS.contains(&first) {
            w.iter()
                .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                .and_then(|i| w.get(i + 1))
                .into_iter()
                .cloned()
                .collect()
        } else if first == "eval" {
            vec![w[1..].join(" ")]
        } else if first == "ssh" {
            let mut i = 1;
            while let Some(a) = w.get(i).filter(|a| a.starts_with('-')) {
                let takes = a.len() == 2 && "bcDEeFIiJLlmOopQRSWw".contains(&a[1..]);
                i += if takes { 2 } else { 1 };
            }
            vec![w.get(i + 1..).unwrap_or_default().join(" ")]
        } else {
            w.iter()
                .enumerate()
                .filter(|(_, a)| matches!(a.as_str(), "-exec" | "-execdir" | "-ok"))
                .map(|(i, _)| w[i + 1..].join(" "))
                .collect()
        };
        let remote = remote || first == "ssh";
        if let Some(found) = inner
            .iter()
            .find_map(|s| scan_cmds(s, depth + 1, remote, hit))
        {
            return Some(found);
        }
    }
    None
}

/// The first raw kill in a shell line: `kill`, `pkill` or `killall` run as a command, also behind
/// `sudo`/`xargs`/`timeout`…, inside `sh -c`, `eval`, `find -exec` and `ssh <host> …`. `kill -0`
/// and `kill -l` signal nothing and pass.
pub fn raw_kill(line: &str) -> Option<String> {
    scan_cmds(line, 0, false, &|w: &[String], _| {
        let first = base(&w[0]);
        let harmless = first == "kill"
            && w.get(1)
                .is_some_and(|a| matches!(a.as_str(), "-0" | "-l" | "-L"));
        (KILLERS.contains(&first) && !harmless).then(|| w.join(" "))
    })
}

/// The read-only and admin forms of an agent program — anything else starts a session.
fn agent_admin(args: &[String]) -> bool {
    match args {
        [] => false,
        [f] => matches!(
            f.as_str(),
            "--version" | "-V" | "-v" | "version" | "--help" | "-h" | "help" | "doctor" | "models"
        ),
        [f, r, ..] => match f.as_str() {
            "doctor" | "models" => true,
            "auth" => r == "status",
            "mcp" => r == "list" || r == "get",
            "update" => r == "--check",
            _ => false,
        },
    }
}

/// A one-shot run that answers and exits: `claude -p|--print …`, `codex exec …`, `agy -p|--print|--prompt …`.
fn agent_oneshot(w: &[String]) -> bool {
    match base(&w[0]) {
        "claude" => w[1..].iter().any(|a| a == "-p" || a == "--print"),
        "codex" => first_arg(&w[1..]).is_some_and(|a| a == "exec"),
        "agy" => w[1..]
            .iter()
            .any(|a| matches!(a.as_str(), "-p" | "--print" | "--prompt")),
        _ => false,
    }
}

/// `npx`/`bunx`/`pnpm|bun|yarn dlx` running an agent package still starts it outside herdr.
fn agent_pkg(w: &[String]) -> bool {
    let known = |pkg: Option<&String>| {
        pkg.is_some_and(|p| {
            let name = match p.rfind('@') {
                Some(i) if i > 0 => &p[..i],
                _ => p.as_str(),
            };
            matches!(name, "@anthropic-ai/claude-code" | "@openai/codex")
        })
    };
    match base(&w[0]) {
        "npx" | "bunx" | "pnpx" => known(first_arg(&w[1..])),
        "pnpm" | "bun" | "yarn" => {
            w.get(1).is_some_and(|s| s == "dlx") && known(first_arg(&w[2..]))
        }
        _ => false,
    }
}

/// The first word that is not an option.
fn first_arg(w: &[String]) -> Option<&String> {
    w.iter().find(|a| !a.starts_with('-'))
}

/// A command that starts an agent program outside herdr — `claude`, `devin`, `codex` or `agy` run for a
/// session, or an agent package via npx/bunx/dlx, wherever it hides. `herdr agent start` and the
/// admin forms pass, and so do one-shot runs from a caller that is itself in herdr.
pub fn agent_start(line: &str, in_herdr: bool) -> Option<String> {
    scan_cmds(line, 0, false, &|w: &[String], _| {
        let program = AGENTS.contains(&base(&w[0]))
            && !agent_admin(&w[1..])
            && !(in_herdr && agent_oneshot(w));
        (program || agent_pkg(w)).then(|| w.join(" "))
    })
}

pub const HOOK_REFUSAL: &str = "agent-fleet: raw kill, pkill and killall are blocked for fleet agents. \
Use `agent-fleet kill <pid>...` (find pids with pgrep): it refuses agents, their panes and \
everything above them, your own ancestors and other agents' processes, and kills your own \
processes, the orphans `agent-fleet doctor` proved and the processes the watch reported hung, TERM then KILL. `kill -0` passes.";

pub const UNMANAGED_REFUSAL: &str = "agent-fleet: every agent runs in herdr. Spawn one with \
`agent-fleet spawn <owner/repo>#<issue>`, or start it in a pane with \
`herdr agent start <name> --kind <program> --pane <pane>`. The read-only and admin forms \
(--version, --help, auth status, doctor, models, mcp list|get, update --check) pass, and so \
do one-shot `claude -p`, `codex exec` and `agy -p` from an agent that is itself in herdr.";

/// The PreToolUse hook for Claude Code and Devin CLI (same format): the tool call on stdin,
/// refused with exit 2 when it is a shell command running a raw kill or starting an agent
/// program outside herdr. Anything it cannot read passes.
pub fn hook(stdin: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdin).ok()?;
    // Claude Code's shell tool is `Bash`, Devin CLI's is `exec`; both put the line in `command`.
    if !matches!(v["tool_name"].as_str(), Some("Bash" | "exec")) {
        return None;
    }
    let line = v["tool_input"]["command"].as_str()?;
    if let Some(hit) = raw_kill(line) {
        return Some(format!("{HOOK_REFUSAL}\nBlocked: {hit}"));
    }
    let hit = agent_start(line, std::env::var("HERDR_ENV").is_ok_and(|v| v == "1"))?;
    Some(format!("{UNMANAGED_REFUSAL}\nBlocked: {hit}"))
}

/// A herdr command that types into a pane: `pane send-text|send-keys|run` or
/// `agent prompt|send-keys`, with its `--session` and target as written, and its argv joined.
#[derive(Debug, PartialEq)]
pub struct HerdrWrite {
    pub session: Option<String>,
    pub target: String,
    pub cmd: String,
}

/// Every herdr write in a shell line, in order, wherever it hides (as `raw_kill` finds kills).
/// A write to `--current` is the caller's own pane, and one sent to another machine
/// (`--machine`, `--remote`, `ssh <host> herdr …`) is out of this hook's reach: both pass.
pub fn herdr_writes(line: &str) -> Vec<HerdrWrite> {
    let found = std::cell::RefCell::new(Vec::new());
    scan_cmds(line, 0, false, &|w: &[String], remote| {
        if let Some(hit) = (!remote).then(|| herdr_argv(w)).flatten() {
            found.borrow_mut().push(hit);
        }
        None::<()>
    });
    found.into_inner()
}

/// The keys `send-keys` may press in another agent's pane: they repair its input, typing nothing.
const REPAIR_KEYS: &[&str] = &[
    "enter",
    "return",
    "esc",
    "escape",
    "tab",
    "backspace",
    "up",
    "down",
    "left",
    "right",
];

fn herdr_argv(w: &[String]) -> Option<HerdrWrite> {
    if base(&w[0]) != "herdr" {
        return None;
    }
    let mut session = None;
    let mut i = 1;
    while let Some(a) = w.get(i).filter(|a| a.starts_with('-')) {
        if let Some(v) = a.strip_prefix("--session=") {
            session = Some(v.to_string());
        } else if a == "--session" {
            session = w.get(i + 1).cloned();
            i += 1;
        } else if a.starts_with("--machine") || a.starts_with("--remote") {
            return None;
        }
        i += 1;
    }
    let verb = (w.get(i)?.as_str(), w.get(i + 1)?.as_str());
    if !matches!(
        verb,
        ("pane", "send-text" | "send-keys" | "run") | ("agent", "prompt" | "send-keys")
    ) {
        return None;
    }
    let mut rest = w[i + 2..].iter();
    let target = loop {
        let a = rest.next()?;
        match a.as_str() {
            "--current" => return None,
            // The options of these verbs that take a value.
            "--timeout" | "--until" => {
                rest.next();
            }
            _ if !a.starts_with('-') => break a.clone(),
            _ => {}
        }
    };
    // Repairing a terminal's input (a queued message's Enter, a stray dialog's Esc) sends keys
    // and no text: it passes.
    let mut keys = rest.filter(|a| !a.starts_with('-')).peekable();
    if verb.1 == "send-keys"
        && keys.peek().is_some()
        && keys.all(|k| REPAIR_KEYS.contains(&k.to_ascii_lowercase().as_str()))
    {
        return None;
    }
    Some(HerdrWrite {
        session,
        target,
        cmd: w.join(" "),
    })
}

/// The socket of herdr session `target`, from the caller's own: the default session's sits at
/// `<root>/herdr.sock`, a named one's at `<root>/sessions/<name>/herdr.sock`.
pub fn session_socket(
    own: &std::path::Path,
    own_session: &str,
    target: &str,
) -> Option<std::path::PathBuf> {
    if target == own_session {
        return Some(own.to_path_buf());
    }
    let dir = own.parent()?;
    let root = if own_session == "default" {
        dir
    } else {
        dir.parent()?.parent()?
    };
    Some(if target == "default" {
        root.join("herdr.sock")
    } else {
        root.join("sessions").join(target).join("herdr.sock")
    })
}

/// The agent and pane `w` would type into, from herdr's `agent.get` answer for its target, when
/// that pane is not the caller's own (`me`: its session and pane). A pane no agent holds passes.
pub fn pane_holder(
    me: Option<(&str, &str)>,
    w: &HerdrWrite,
    got: &serde_json::Value,
) -> Option<(String, String)> {
    let a = got.get("agent")?;
    let pane = a["pane_id"].as_str()?;
    let mine = me.is_some_and(|(session, own)| {
        own == pane && w.session.as_deref().is_none_or(|s| s == session)
    });
    (!mine).then(|| {
        (
            a["name"].as_str().unwrap_or("<agent>").to_string(),
            pane.to_string(),
        )
    })
}

pub fn pane_refusal(w: &HerdrWrite, agent: &str, pane: &str) -> String {
    format!(
        "agent-fleet: typing into another agent's pane is blocked. Tell it instead: \
`agent-fleet tell {agent} TYPE ref \"…\"` (queued, rung on its doorbell and acked when read). \
Your own pane (`--current`) passes, and so does `send-keys` of named keys only, to repair its \
input (`herdr agent send-keys {agent} enter`: {}); a spawn's first prompt comes from the host.\n\
Blocked: {} (agent {agent} holds pane {pane})",
        REPAIR_KEYS.join(" "),
        w.cmd
    )
}

/// The herdr half of `hook bash`: the first write in the line into a pane another agent holds,
/// as herdr answers `agent.get` (`--session <name>` asks that session's socket). A caller
/// without `HERDR_SESSION` is in herdr's default session. No socket, no answer, or no agent
/// there: the write passes; every lookup together gets a second.
pub async fn herdr_hook(stdin: &str) -> Option<(HerdrWrite, String, String)> {
    let v: serde_json::Value = serde_json::from_str(stdin).ok()?;
    if !matches!(v["tool_name"].as_str(), Some("Bash" | "exec")) {
        return None;
    }
    let writes = herdr_writes(v["tool_input"]["command"].as_str()?);
    if writes.is_empty() {
        return None;
    }
    let env = |k: &str| std::env::var(k).ok();
    let sock = std::path::PathBuf::from(env("HERDR_SOCKET_PATH")?);
    let session = env("HERDR_SESSION").unwrap_or_else(|| "default".into());
    let pane = env("HERDR_PANE_ID");
    let me = pane.as_deref().map(|p| (session.as_str(), p));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        for w in writes {
            let target = w.session.as_deref().unwrap_or(&session);
            let Some(sock) = session_socket(&sock, &session, target) else {
                continue;
            };
            let params = serde_json::json!({ "target": w.target });
            let Ok(got) = crate::host::herdr_call(&sock, "agent.get", params).await else {
                continue;
            };
            if let Some((agent, pane)) = pane_holder(me, &w, &got) {
                return Some((w, agent, pane));
            }
        }
        None
    })
    .await
    .ok()?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// herdr → pane shell → claude → Bash tool shell → script → agent-fleet kill, a second agent
    /// in another pane, and an unrelated daemon.
    fn fleet() -> Table {
        parse_ps(
            "1 0 /sbin/launchd
             100 1 /opt/homebrew/bin/herdr
             101 100 -zsh
             102 101 /Users/u/.local/bin/claude
             103 102 /bin/zsh
             104 103 bash
             105 104 agent-fleet
             106 102 node
             107 106 sleep
             201 100 -zsh
             202 201 devin-cli
             203 202 xcodebuild
             300 1 postgres
             301 1 agent-fleet
             500 1 herdr
             501 500 zsh",
        )
    }

    const ME: u32 = 105;

    #[test]
    fn the_incident_killing_the_parent_of_your_script_is_refused() {
        let t = fleet();
        for pid in [104, 103, 102] {
            let why = judge(&t, ME, pid, |_| true).unwrap_err();
            assert!(why.contains("your own ancestor"), "{pid}: {why}");
        }
        assert!(
            judge(&t, ME, 102, |_| true)
                .unwrap_err()
                .contains("bash → zsh → claude")
        );
    }

    #[test]
    fn agents_panes_the_multiplexer_and_daemons_are_refused() {
        let t = fleet();
        for (pid, what) in [
            (100, "your own ancestor"),
            (101, "your own ancestor"),
            (500, "multiplexer"),
            (501, "pane's shell"),
            (201, "pane's shell"),
            (202, "an agent"),
            (301, "daemon"),
            (1, "system"),
            (ME, "this command"),
        ] {
            let why = judge(&t, ME, pid, |_| true).unwrap_err();
            assert!(why.contains(what), "{pid}: {why}");
        }
    }

    #[test]
    fn an_ancestor_of_an_agent_is_refused_even_when_it_is_no_ancestor_of_yours() {
        let mut t = fleet();
        // A wrapper script that started another agent: killing it takes the agent down too.
        t.insert(
            400,
            Proc {
                ppid: 1,
                name: "run-agent.sh".into(),
            },
        );
        t.insert(
            401,
            Proc {
                ppid: 400,
                name: "claude".into(),
            },
        );
        let why = judge(&t, ME, 400, |_| true).unwrap_err();
        assert!(why.contains("ancestor of an agent 401"), "{why}");
        t.get_mut(&401).unwrap().name = "agy".into();
        let why = judge(&t, ME, 400, |_| true).unwrap_err();
        assert!(why.contains("ancestor of an agent 401"), "agy: {why}");
    }

    #[test]
    fn your_own_processes_pass_and_other_agents_and_strangers_do_not() {
        let t = fleet();
        assert_eq!(judge(&t, ME, 106, |_| false), Ok(()));
        assert_eq!(judge(&t, ME, 107, |_| false), Ok(()));
        let other = judge(&t, ME, 203, |foreign| !foreign).unwrap_err();
        assert_eq!(
            judge(&t, ME, 203, |foreign| foreign),
            Ok(()),
            "a hung report is proof inside another agent's tree"
        );
        assert!(
            other.contains("another agent's process tree: devin-cli 202"),
            "{other}"
        );
        assert!(
            judge(&t, ME, 300, |_| false)
                .unwrap_err()
                .contains("not yours")
        );
        assert_eq!(judge(&t, ME, 300, |_| true), Ok(()), "a proved orphan");
        assert!(
            judge(&t, ME, 999, |_| true)
                .unwrap_err()
                .contains("no such process")
        );
    }

    #[test]
    fn a_command_is_the_tool_shell_an_agent_runs_it_in() {
        let t = fleet();
        assert_eq!(
            command_of(&t, 105),
            Some(103),
            "under bash under the tool shell"
        );
        assert_eq!(
            command_of(&t, 107),
            Some(106),
            "under the agent's node child"
        );
        assert_eq!(command_of(&t, 203), Some(203), "a direct child of an agent");
        assert_eq!(command_of(&t, 300), None, "no agent above");
    }

    fn mine() -> Homes {
        Homes {
            mine: Some("/w/mine".into()),
            others: vec!["/w/other".into()],
        }
    }

    #[test]
    fn homes_come_from_the_assigned_worktrees_not_from_where_the_caller_stands() {
        let ag = |id: &str, c: &str| (id.to_string(), Some(c.to_string()));
        let wts: Vec<String> = ["/w/mine", "/w/other", "/w"].map(String::from).into();
        let agents = [
            ag("a:me", "/w/mine/web"),
            ag("a:you", "/w/other"),
            ("a:blank".into(), None),
        ];
        assert_eq!(
            homes("a:me", &agents, &wts, &[]),
            Homes {
                mine: Some("/w/mine".into()),
                others: vec!["/w/other".into()]
            },
            "the longest recorded worktree holds the cwd"
        );
        assert_eq!(
            homes("a:blank", &agents, &wts, &[]).mine,
            None,
            "no cwd recorded"
        );
        assert_eq!(
            homes("a:ghost", &agents, &wts, &[]).mine,
            None,
            "not an agent here"
        );
        assert_eq!(
            homes("a:me", &agents, &[], &[]).mine,
            None,
            "no worktree recorded"
        );
    }

    #[test]
    fn a_main_checkout_is_never_the_callers_root() {
        let ag = |id: &str, c: &str| (id.to_string(), Some(c.to_string()));
        // `/r` and `/s` are main checkouts (path == repo); `/r-wt/*` are linked worktrees of `/r`.
        let wts: Vec<String> = ["/r", "/s", "/r-wt/hand", "/r-wt/spawned"]
            .map(String::from)
            .into();
        let mains: Vec<String> = ["/r", "/s"].map(String::from).into();
        let agents = [
            ag("a:main", "/r/web"),         // hand-started, first seen in a main checkout
            ag("a:hand", "/r-wt/hand/web"), // hand-started, first seen in a linked worktree
            ag("a:spawned", "/r-wt/spawned"), // spawn result names a linked worktree
            ag("a:spawned-main", "/s"),     // spawn result names a main checkout
        ];
        let h = |me| homes(me, &agents, &wts, &mains);
        assert_eq!(h("a:main").mine, None, "first seen in a main checkout");
        assert_eq!(h("a:hand").mine.as_deref(), Some("/r-wt/hand"));
        assert_eq!(h("a:spawned").mine.as_deref(), Some("/r-wt/spawned"));
        assert_eq!(
            h("a:spawned-main").mine,
            None,
            "spawned into a main checkout"
        );
        assert!(
            h("a:hand").others.contains(&"/r".to_string()),
            "a main checkout still protects its orphans from other agents"
        );

        let mut t = fleet();
        t.insert(
            700,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        t.insert(
            701,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        let cwd = |p: u32| match p {
            700 => Some("/r/web".to_string()),
            _ => Some("/r-wt/hand/web".to_string()),
        };
        let go = |pid, me| judge_in(&t, ME, pid, |_| false, &h(me), &cwd);
        let main = go(700, "a:main").unwrap_err();
        assert!(main.contains("records no worktree for you"), "{main}");
        assert!(go(700, "a:hand").is_err(), "not in the caller's worktree");
        assert_eq!(go(701, "a:hand"), Ok(()), "a linked worktree still works");
    }

    #[test]
    fn orphans_are_refused_without_a_record_and_in_another_agents_worktree() {
        let mut t = fleet();
        t.insert(
            700,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        t.insert(
            701,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        t.insert(
            702,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        let cwd = |p: u32| match p {
            700 => Some("/w/mine/web".to_string()),
            701 => Some("/w/other/web".to_string()),
            _ => Some("/w/elsewhere".to_string()),
        };
        let go = |pid, h: &Homes| judge_in(&t, ME, pid, |_| false, h, &cwd);
        assert_eq!(go(700, &mine()), Ok(()));
        let none = go(700, &Homes::default()).unwrap_err();
        assert!(
            none.contains("records no worktree for you to attribute"),
            "{none}"
        );
        // The caller may stand in the other agent's directory; its record still says /w/mine.
        let other = go(701, &mine()).unwrap_err();
        assert!(
            other.contains("orphan in /w/other, a worktree recorded for another agent"),
            "{other}"
        );
        let out = go(702, &mine()).unwrap_err();
        assert!(
            out.contains("an orphan outside your worktree /w/mine"),
            "{out}"
        );
    }

    #[test]
    fn a_dev_server_reparented_out_of_the_agents_tree_is_yours_by_its_worktree() {
        let mut t = fleet();
        // `bun serve.ts &` from the agent's Bash shell, whose shell then exited: ppid 1, or the
        // user's systemd on Linux. A stranger's postgres sits in no worktree of yours.
        t.insert(
            600,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        t.insert(
            601,
            Proc {
                ppid: 650,
                name: "vite".into(),
            },
        );
        t.insert(
            650,
            Proc {
                ppid: 1,
                name: "systemd".into(),
            },
        );
        t.insert(
            602,
            Proc {
                ppid: 600,
                name: "esbuild".into(),
            },
        );
        t.insert(
            603,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        let cwd = |p: u32| match p {
            600..=602 => Some("/w/mine/web".to_string()),
            603 => Some("/w/other/web".to_string()),
            _ => None,
        };
        let go = |pid| judge_in(&t, ME, pid, |_| false, &mine(), &cwd);
        assert_eq!(go(600), Ok(()), "orphan, cwd in my worktree");
        assert_eq!(go(601), Ok(()), "parent is a user systemd");
        assert!(
            go(602).unwrap_err().contains("not yours"),
            "not orphaned: its parent is a live bun"
        );
        assert!(
            go(603).unwrap_err().contains("recorded for another agent"),
            "other worktree"
        );
        assert!(
            judge_in(&t, ME, 600, |_| false, &Homes::default(), &cwd).is_err(),
            "no worktree known"
        );
        // `/w/mine-two` is not inside `/w/mine`.
        t.insert(
            604,
            Proc {
                ppid: 1,
                name: "bun".into(),
            },
        );
        let cwd2 = |_| Some("/w/mine-two".to_string());
        assert!(judge_in(&t, ME, 604, |_| false, &mine(), &cwd2).is_err());
    }

    #[test]
    fn outside_any_agent_what_your_shell_started_is_yours() {
        let t = parse_ps(
            "1 0 launchd
             10 1 sshd
             11 10 sh
             12 11 agent-fleet
             13 11 sleep
             20 1 sleep",
        );
        assert_eq!(judge(&t, 12, 13, |_| false), Ok(()));
        assert!(
            judge(&t, 12, 20, |_| false)
                .unwrap_err()
                .contains("not yours")
        );
        assert!(
            judge(&t, 12, 11, |_| false)
                .unwrap_err()
                .contains("your own ancestor")
        );
    }

    #[test]
    fn raw_kills_are_found_where_they_hide() {
        for line in [
            "kill -9 $PP $P",
            "kill 123",
            "pkill -f metro",
            "/usr/bin/killall node",
            "cd /x && kill $(cat pid)",
            "pgrep -f expo | xargs kill",
            "pgrep -f expo | xargs -r kill -TERM",
            "sudo -n kill 1",
            "FOO=1 timeout 5 kill 3",
            "sh -c 'kill 5'",
            "bash -lc \"pkill node\"",
            "eval kill 4",
            "find /proc -name x -exec kill {} \\;",
            "ssh mac 'kill -9 123'",
            "ssh -o BatchMode=yes mac pkill -f maestro",
            "if true; then kill 1; fi",
            "echo $(kill 2)",
            "(kill 3)",
        ] {
            assert!(raw_kill(line).is_some(), "missed: {line}");
        }
        for line in [
            "kill -0 123",
            "kill -l",
            "agent-fleet kill 123",
            "git commit -m 'kill the flaky test'",
            "echo kill",
            "rg -n 'pkill' scripts/",
            "pgrep -fl metro",
            "ssh mac 'pgrep -f maestro'",
            "cargo test kill",
            "ls skill/",
        ] {
            assert_eq!(raw_kill(line), None, "false alarm: {line}");
        }
    }

    #[test]
    fn an_agent_started_outside_herdr_is_blocked() {
        for line in [
            "claude",
            "claude -p 'fix it'",
            "devin -p 'do x'",
            "codex exec 'y'",
            "/usr/local/bin/claude -p x",
            "nohup devin -p x &",
            "bash -c \"claude -p x\"",
            "sudo claude",
            "timeout 10 codex exec go",
            "agy -p x",
            "FOO=1 claude -p x",
            "eval claude -p x",
            "echo hi | xargs devin -p",
            "claude --dangerously-skip-permissions",
            "claude mcp serve",
            "timeout 10s claude",
            "sudo -u bob claude",
            "env -u FOO claude",
            "exec -a x devin -p y",
            "npx @anthropic-ai/claude-code",
            "npx -y @anthropic-ai/claude-code@2",
            "bunx @openai/codex exec go",
            "pnpm dlx @anthropic-ai/claude-code",
            "yarn dlx @openai/codex",
            "timeout -k 2 30s claude",
        ] {
            assert!(agent_start(line, false).is_some(), "{line}");
        }
        for line in [
            "claude --version",
            "claude -V",
            "claude version",
            "claude --help",
            "claude -h",
            "claude help",
            "claude auth status",
            "claude doctor",
            "claude doctor extra",
            "claude models",
            "claude mcp list",
            "claude mcp get srv",
            "codex update --check",
            "herdr agent start a --kind claude --pane p1",
            "echo claude",
            "cat claude.md",
            "git commit -m 'the claude fix'",
            "kill -0 1",
            "claude -v",
            "command -v claude",
            "command -V codex",
            "FOO=1 command -v devin",
            "npx cowsay hi",
            "pnpm dlx prettier x",
            "cat > /tmp/f <<'EOF'\nclaude -p x\nEOF\necho done",
            "echo a << b",
        ] {
            assert_eq!(agent_start(line, false), None, "{line}");
        }
    }

    #[test]
    fn a_one_shot_passes_only_from_inside_herdr() {
        for line in [
            "claude -p 'review this'",
            "claude --model opus --print x",
            "codex exec 'audit'",
            "timeout 300 codex exec x",
            "agy --print audit",
            "agy --prompt audit",
            "agy -p audit",
        ] {
            assert_eq!(agent_start(line, true), None, "{line}");
            assert!(agent_start(line, false).is_some(), "{line}");
        }
        for line in [
            "devin -p x",
            "claude",
            "codex",
            "agy",
            "agy -m x",
            "claude --resume",
        ] {
            assert!(agent_start(line, true).is_some(), "{line}");
        }
    }

    #[test]
    fn the_hook_blocks_bare_agent_programs_too() {
        let call = |cmd: &str| {
            hook(
                &serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": cmd } })
                    .to_string(),
            )
        };
        let refused = call("devin -p 'go'").unwrap();
        assert!(refused.contains("every agent runs in herdr"), "{refused}");
        assert!(refused.contains("agent-fleet spawn"), "{refused}");
        assert!(call("claude --version").is_none());
        assert!(call("herdr agent start a --kind devin --pane p1").is_none());
        // a raw kill still wins over an agent line in the same command
        assert!(call("kill 9; claude -p x").unwrap().contains("raw kill"));
    }

    #[test]
    fn the_hook_blocks_only_bash_raw_kills() {
        let call = |tool: &str, cmd: &str| {
            hook(
                &serde_json::json!({"tool_name": tool, "tool_input": {"command": cmd}}).to_string(),
            )
        };
        assert!(
            call("Bash", "kill -9 1")
                .unwrap()
                .contains("agent-fleet kill")
        );
        assert_eq!(call("Bash", "agent-fleet kill 1"), None);
        assert_eq!(call("Read", "kill -9 1"), None);
        assert_eq!(hook("not json"), None);
    }

    #[test]
    fn herdr_writes_into_a_pane_are_found_where_they_hide() {
        let w = |line: &str| {
            herdr_writes(line)
                .into_iter()
                .map(|w| (w.session, w.target))
                .collect::<Vec<_>>()
        };
        let at = |s: Option<&str>, t: &str| vec![(s.map(str::to_string), t.to_string())];
        // a write into another session's pane, as the coordinator typed it
        assert_eq!(
            w("herdr --session s1 pane send-text wY9:p1 hi"),
            at(Some("s1"), "wY9:p1")
        );
        for (line, target) in [
            ("herdr pane send-keys w1:p2 q", "w1:p2"),
            ("herdr pane run w1:p2 'just test'", "w1:p2"),
            (
                "herdr agent prompt reviewer 'go' --wait --timeout 9",
                "reviewer",
            ),
            (
                "herdr agent prompt --wait --timeout 9 reviewer go",
                "reviewer",
            ),
            ("herdr agent prompt --until idle reviewer go", "reviewer"),
            ("herdr agent send-keys reviewer y", "reviewer"),
            ("herdr pane send-keys w1:p2 enter h i enter", "w1:p2"),
            ("herdr agent send-keys reviewer ctrl+c", "reviewer"),
            ("/opt/homebrew/bin/herdr pane send-text p1 x", "p1"),
            ("sleep 1 && sh -c 'herdr pane send-text p1 x'", "p1"),
            ("HERDR_X=1 herdr pane send-text p1 x", "p1"),
        ] {
            assert_eq!(w(line), at(None, target), "{line}");
        }
        assert_eq!(
            w("herdr --session=other pane send-text p1 x"),
            at(Some("other"), "p1")
        );
        // every write in the line, so a harmless first one hides nothing
        assert_eq!(
            w("herdr pane run w1:p3 'npm test' && herdr agent prompt reviewer go"),
            [at(None, "w1:p3"), at(None, "reviewer")].concat()
        );
        for line in [
            "herdr pane read p1",
            "herdr agent get reviewer",
            "herdr agent start a --kind claude --pane p1",
            "herdr pane send-text --current hi",
            // repairing a terminal's input: named keys only, no text
            "herdr agent send-keys reviewer enter",
            "herdr pane send-keys w1:p2 Esc",
            "herdr agent send-keys reviewer esc esc up enter",
            "herdr --machine box pane send-text p1 x",
            "herdr --remote box pane send-text p1 x",
            "ssh box herdr agent prompt reviewer go",
            "echo herdr pane send-text p1 x",
            "herdr pane send-text",
        ] {
            assert_eq!(w(line), vec![], "false alarm: {line}");
        }
    }

    #[test]
    fn another_sessions_socket_sits_where_herdr_puts_it() {
        let sock = |own: &str, me: &str, s: &str| {
            session_socket(std::path::Path::new(own), me, s)
                .map(|p| p.to_string_lossy().into_owned())
        };
        let named = "/h/sessions/s1/herdr.sock";
        assert_eq!(sock(named, "s1", "s1").as_deref(), Some(named));
        assert_eq!(
            sock(named, "s1", "s2").as_deref(),
            Some("/h/sessions/s2/herdr.sock")
        );
        assert_eq!(
            sock(named, "s1", "default").as_deref(),
            Some("/h/herdr.sock")
        );
        let default = "/h/herdr.sock";
        assert_eq!(
            sock(default, "default", "default").as_deref(),
            Some(default)
        );
        assert_eq!(
            sock(default, "default", "s1").as_deref(),
            Some("/h/sessions/s1/herdr.sock")
        );
    }

    #[test]
    fn a_pane_another_agent_holds_is_refused_your_own_passes() {
        let w = |s: Option<&str>| HerdrWrite {
            session: s.map(str::to_string),
            target: "dv".into(),
            cmd: "herdr pane send-text dv hi".into(),
        };
        let held = |pane: &str| json!({ "agent": { "name": "dv", "pane_id": pane } });
        let me = Some(("s1", "wA:p1"));
        assert_eq!(
            pane_holder(me, &w(None), &held("wB:p1")),
            Some(("dv".to_string(), "wB:p1".to_string()))
        );
        assert_eq!(pane_holder(me, &w(None), &held("wA:p1")), None);
        assert_eq!(pane_holder(me, &w(Some("s1")), &held("wA:p1")), None);
        // the same pane id in another session is another pane
        assert!(pane_holder(me, &w(Some("other")), &held("wA:p1")).is_some());
        // a caller with no pane id of its own owns no pane
        assert!(pane_holder(None, &w(None), &held("wA:p1")).is_some());
        // a pane no agent holds (a dev server's shell) passes
        assert_eq!(pane_holder(me, &w(None), &json!({})), None);
        assert_eq!(pane_holder(me, &w(None), &json!({ "agent": null })), None);

        let msg = pane_refusal(&w(None), "dv", "wB:p1");
        assert!(msg.contains("agent-fleet tell dv TYPE"), "{msg}");
        assert!(msg.contains("Blocked: herdr pane send-text dv hi"), "{msg}");
    }
}
