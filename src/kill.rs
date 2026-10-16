//! `agent-fleet kill` and the Bash hook that sends agents to it. A process is killed only when it
//! is the caller's own or an orphan doctor proved; never an agent, a multiplexer pane, the fleet's
//! own daemons, or anything above one of them.

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
const AGENTS: &[&str] = &["claude", "devin", "codex"];
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

/// Whether `me` may signal `pid`. `orphan` asks whether doctor proved `pid` an orphan.
pub fn judge(
    t: &Table,
    me: u32,
    pid: u32,
    orphan: impl FnOnce() -> bool,
) -> std::result::Result<(), String> {
    if !t.contains_key(&pid) {
        return Err(format!("pid {pid}: no such process"));
    }
    if let Some(why) = protected(t, me, pid) {
        return Err(why);
    }
    let mine = agent_of(t, me);
    // Outside any agent (a person's shell, ssh), what that shell started is the caller's own.
    let shell = parent(t, me).filter(|_| mine.is_none());
    match agent_of(t, pid) {
        Some(a) if Some(a) == mine => Ok(()),
        Some(a) => Err(format!(
            "pid {pid} ({}) belongs to another agent's process tree: {} {a}",
            name(t, pid),
            name(t, a)
        )),
        None if shell.is_some_and(|sh| ancestors(t, pid).contains(&sh)) => Ok(()),
        None if orphan() => Ok(()),
        None => Err(format!(
            "pid {pid} ({}) is not yours: no process of yours started it and doctor has not \
             proved it an orphan",
            name(t, pid)
        )),
    }
}

/// The words of each simple command in a shell line, roughly: quotes group words, separators
/// (`;` `&` `|` newline, parentheses, backticks) split commands.
fn commands(line: &str) -> Vec<Vec<String>> {
    let (mut cmds, mut cmd, mut word) = (Vec::new(), Vec::new(), String::new());
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    let end_word = |cmd: &mut Vec<String>, word: &mut String, quoted: &mut bool| {
        if !word.is_empty() || *quoted {
            cmd.push(std::mem::take(word));
        }
        *quoted = false;
    };
    while let Some(c) = chars.next() {
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

/// The first raw kill in a shell line: `kill`, `pkill` or `killall` run as a command, also behind
/// `sudo`/`xargs`/`timeout`…, inside `sh -c`, `eval`, `find -exec` and `ssh <host> …`. `kill -0`
/// and `kill -l` signal nothing and pass. Best effort, not a sandbox: it guards against a slip.
pub fn raw_kill(line: &str) -> Option<String> {
    raw_kill_in(line, 0)
}

fn raw_kill_in(line: &str, depth: u32) -> Option<String> {
    if depth > 4 {
        return None;
    }
    for words in commands(line) {
        let mut w: &[String] = &words;
        loop {
            match w.first().map(|s| base(s)) {
                Some(f) if is_assignment(f) => w = &w[1..],
                Some(f) if WRAPPERS.contains(&f) => {
                    w = &w[1..];
                    while w.first().is_some_and(|a| {
                        a.starts_with('-') || is_assignment(a) || a.parse::<f64>().is_ok()
                    }) {
                        w = &w[1..];
                    }
                }
                _ => break,
            }
        }
        let Some(first) = w.first().map(|s| base(s)) else {
            continue;
        };
        if KILLERS.contains(&first) {
            let harmless = first == "kill"
                && w.get(1)
                    .is_some_and(|a| matches!(a.as_str(), "-0" | "-l" | "-L"));
            if !harmless {
                return Some(w.join(" "));
            }
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
        if let Some(hit) = inner.iter().find_map(|s| raw_kill_in(s, depth + 1)) {
            return Some(hit);
        }
    }
    None
}

pub const HOOK_REFUSAL: &str = "agent-fleet: raw kill, pkill and killall are blocked for fleet agents. \
Use `agent-fleet kill <pid>...` (find pids with pgrep): it refuses agents, their panes and \
everything above them, your own ancestors and other agents' processes, and kills your own \
processes and orphans `agent-fleet doctor` proved, TERM then KILL. `kill -0` passes.";

/// The PreToolUse hook for Claude Code and Devin CLI (same format): the tool call on stdin,
/// refused with exit 2 when it is a shell command running a raw kill. Anything it cannot read
/// passes.
pub fn hook(stdin: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdin).ok()?;
    // Claude Code's shell tool is `Bash`, Devin CLI's is `exec`; both put the line in `command`.
    if !matches!(v["tool_name"].as_str(), Some("Bash" | "exec")) {
        return None;
    }
    let hit = raw_kill(v["tool_input"]["command"].as_str()?)?;
    Some(format!("{HOOK_REFUSAL}\nBlocked: {hit}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let why = judge(&t, ME, pid, || true).unwrap_err();
            assert!(why.contains("your own ancestor"), "{pid}: {why}");
        }
        assert!(
            judge(&t, ME, 102, || true)
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
            let why = judge(&t, ME, pid, || true).unwrap_err();
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
        let why = judge(&t, ME, 400, || true).unwrap_err();
        assert!(why.contains("ancestor of an agent 401"), "{why}");
    }

    #[test]
    fn your_own_processes_pass_and_other_agents_and_strangers_do_not() {
        let t = fleet();
        assert_eq!(judge(&t, ME, 106, || false), Ok(()));
        assert_eq!(judge(&t, ME, 107, || false), Ok(()));
        let other = judge(&t, ME, 203, || true).unwrap_err();
        assert!(
            other.contains("another agent's process tree: devin-cli 202"),
            "{other}"
        );
        assert!(
            judge(&t, ME, 300, || false)
                .unwrap_err()
                .contains("not yours")
        );
        assert_eq!(judge(&t, ME, 300, || true), Ok(()), "a proved orphan");
        assert!(
            judge(&t, ME, 999, || true)
                .unwrap_err()
                .contains("no such process")
        );
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
        assert_eq!(judge(&t, 12, 13, || false), Ok(()));
        assert!(
            judge(&t, 12, 20, || false)
                .unwrap_err()
                .contains("not yours")
        );
        assert!(
            judge(&t, 12, 11, || false)
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
}
