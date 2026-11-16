//! `dev ls` and `dev clean`: the worktrees and local branches of every repository
//! connected on this machine, and which of them are finished.
//!
//! A local branch is stale when its tip is safe somewhere else: it is the head of one of your
//! merged or closed PRs (`pr`), an ancestor of the default branch (`in main`), or contained in
//! `origin/<branch>` (`pushed`). A branch with an open PR is never stale, and local-only commits
//! are never deleted. Only `pr` retires a worktree: one just made off main is `in main` too.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::scan::Procs;
use crate::state::{State, worktree_key};
use crate::tui::Fleet;

/// One of your PRs, as `gh pr list --json headRefName,state,headRefOid,number` gives it.
#[derive(Debug, Clone, Deserialize)]
pub struct Pr {
    #[serde(rename = "headRefName")]
    pub branch: String,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    #[serde(rename = "headRefOid")]
    pub tip: String,
    pub number: u64,
}

/// Why `branch` at `tip` may be deleted, or `None` to keep it. `in_main`: the tip is an ancestor
/// of the default branch; `pushed`: `origin/<branch>` contains it.
pub fn stale(
    branch: &str,
    tip: &str,
    prs: &[Pr],
    in_main: bool,
    pushed: bool,
) -> Option<&'static str> {
    let mine = || prs.iter().filter(|p| p.branch == branch);
    if mine().any(|p| p.state == "OPEN") {
        None
    } else if mine().any(|p| p.tip == tip) {
        // Before `in main`: a PR merged with a merge commit has its tip in main too, and is done.
        Some("pr")
    } else if in_main {
        Some("in main")
    } else if pushed {
        Some("pushed")
    } else {
        None
    }
}

/// Why the worktree at `path` must stay even when its branch is finished; `None` when it may go.
/// `lock` is already said (`lock_note`); `guarded` are processes clean must not stop.
fn blocked(
    main: bool,
    here: bool,
    lock: Option<String>,
    agents: &[String],
    dirty: usize,
    guarded: &[String],
) -> Option<String> {
    if main {
        Some("main checkout".into())
    } else if lock.is_some() {
        lock
    } else if here {
        Some("you are in it".into())
    } else if !agents.is_empty() {
        let names: Vec<&str> = agents.iter().map(|a| crate::host::short(a)).collect();
        Some(format!("{} works in it", names.join(", ")))
    } else if dirty > 0 {
        Some(format!("{dirty} uncommitted files"))
    } else if !guarded.is_empty() {
        Some(format!(
            "{} runs in it (an agent's or yours)",
            guarded.join(", ")
        ))
    } else {
        None
    }
}

/// `path` → lock reason (maybe empty) of each locked worktree in `git worktree list --porcelain`.
fn locks(list: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for block in list.split("\n\n") {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "));
        let lock = block
            .lines()
            .find_map(|l| (l == "locked").then_some("").or(l.strip_prefix("locked ")));
        if let (Some(p), Some(r)) = (path, lock) {
            out.insert(p.to_string(), r.to_string());
        }
    }
    out
}

/// What keeps a locked worktree. A lock whose reason names a pid that is gone (Claude Code locks
/// a subagent's as `claude agent <id> (pid N start S)`) is stale: the note gives the unlock
/// command, and clean never unlocks or forces by itself.
fn lock_note(reason: &str, repo: &Path, path: &str, alive: impl Fn(u32) -> bool) -> String {
    let pid = reason.split("pid ").nth(1).and_then(|r| {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    match pid {
        Some(pid) if !alive(pid) => format!(
            "locked, stale (pid {pid} gone): git -C {} worktree unlock {path}",
            repo.display()
        ),
        _ if reason.is_empty() => "locked".into(),
        _ => format!("locked: {reason}"),
    }
}

/// A process clean stops before it retires a worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct Leftover {
    pub pid: u32,
    /// Tells the process from a later one that reused its pid.
    pub started: Option<DateTime<Utc>>,
    pub command: String,
}

fn inside(cwd: &str, dir: &str) -> bool {
    cwd == dir || cwd.starts_with(&format!("{dir}/"))
}

/// `dir` with its symlinks resolved, as a process's cwd is reported (macOS's `/var` is
/// `/private/var`); as given when it cannot be resolved.
fn real(dir: &str) -> String {
    std::fs::canonicalize(dir).map_or_else(|_| dir.to_string(), |p| p.to_string_lossy().into())
}

/// The processes whose cwd is in `path`: the leftovers clean stops, and the names of those it
/// must not: a multiplexer and its panes, anything an agent program runs (`kill::AGENTS`, at any
/// depth), and the caller and its ancestors (`mine`).
fn leftovers(procs: &Procs, path: &str, mine: &HashSet<u32>) -> (Vec<Leftover>, Vec<String>) {
    let path = &real(path);
    let agents: HashSet<u32> = procs
        .0
        .iter()
        .filter(|(_, p)| crate::kill::AGENTS.contains(&p.name.as_str()))
        .map(|(&pid, _)| pid)
        .collect();
    let (mut stop, mut guarded) = (Vec::new(), BTreeSet::new());
    for (&pid, p) in &procs.0 {
        if !p.cwd.as_deref().is_some_and(|c| inside(c, path)) {
            continue;
        }
        if crate::scan::is_terminal(procs, p)
            || procs.under_any(pid, &agents)
            || mine.contains(&pid)
        {
            guarded.insert(p.name.clone());
        } else {
            stop.push(Leftover {
                pid,
                started: p.started,
                command: p.args.clone().unwrap_or_else(|| p.name.clone()),
            });
        }
    }
    stop.sort_by_key(|l| l.pid);
    (stop, guarded.into_iter().collect())
}

/// `pid` and its ancestors in `procs`.
fn lineage(procs: &Procs, pid: u32) -> HashSet<u32> {
    let mut out = HashSet::from([pid]);
    let mut at = pid;
    while let Some(p) = procs.0.get(&at).map(|p| p.ppid) {
        if p == 0 || !out.insert(p) {
            break;
        }
        at = p;
    }
    out
}

/// `lsof -Fpn` output → pid → path of each `n` line (one per pid with `-d cwd`).
fn parse_lsof_cwds(s: &str) -> BTreeMap<u32, String> {
    let (mut out, mut pid) = (BTreeMap::new(), None);
    for l in s.lines() {
        if let Some(p) = l.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(n), Some(p)) = (l.strip_prefix('n'), pid) {
            out.insert(p, n.to_string());
        }
    }
    out
}

/// `procs` with every cwd it can have: /proc gives them on Linux; elsewhere (macOS `ps` has
/// none) `lsof`'s answer fills them in. `None` when no cwd can be read: then no worktree is
/// known to be empty.
fn with_cwds(mut procs: Procs, lsof: impl FnOnce() -> Option<String>) -> Option<Procs> {
    if procs.0.values().any(|p| p.cwd.is_some()) {
        return Some(procs);
    }
    let cwds = parse_lsof_cwds(&lsof()?);
    if cwds.is_empty() {
        return None;
    }
    for (pid, cwd) in cwds {
        if let Some(p) = procs.0.get_mut(&pid) {
            p.cwd = Some(cwd);
        }
    }
    Some(procs)
}

/// Every process on this machine with its cwd, or `None` when cwds cannot be read.
fn table() -> Option<Procs> {
    with_cwds(Procs::read(), || {
        let out = std::process::Command::new(crate::scan::lsof_path())
            .args(["-w", "-a", "-d", "cwd", "-Fpn"])
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    })
}

/// Seconds between TERM and KILL, as `agent-fleet kill`'s default.
const GRACE: Duration = Duration::from_secs(5);

/// TERM, then KILL after `grace`, each of `them` still in `dir`: the table is re-read before each
/// signal, and a pid whose start time or cwd changed (reused) is never hit. `Err` when one
/// survives: the worktree then stays.
async fn stop(them: &[Leftover], dir: &str, grace: Duration) -> Result<String, String> {
    let dir = &real(dir);
    let still = |procs: &Procs, l: &Leftover| {
        procs.0.get(&l.pid).is_some_and(|p| {
            p.started == l.started && p.cwd.as_deref().is_some_and(|c| inside(c, dir))
        })
    };
    let signal = |pid: u32, sig| unsafe { libc::kill(pid as i32, sig) };
    let read = || table().ok_or_else(|| "processes unknown: no cwd readable".to_string());
    let now = read()?;
    let them: Vec<&Leftover> = them.iter().filter(|l| still(&now, l)).collect();
    for l in &them {
        // SAFETY: kill only sends a signal.
        signal(l.pid, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + grace;
    let mut left = them.clone();
    while !left.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = read()?;
        left.retain(|l| still(&now, l));
    }
    for l in &left {
        signal(l.pid, libc::SIGKILL);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let now = read()?;
    if let Some(l) = left.iter().find(|l| still(&now, l)) {
        return Err(format!("pid {} survived KILL", l.pid));
    }
    let said: Vec<String> = them
        .iter()
        .map(|l| {
            let how = if left.iter().any(|k| k.pid == l.pid) {
                "KILL"
            } else {
                "TERM"
            };
            format!("{} {how}", l.pid)
        })
        .collect();
    Ok(format!("stopped {}", said.join(", ")))
}

/// The repositories to look at: the machine's `[repos]`, the main checkouts of the worktrees
/// `host` reports (the `host --repo` ones and those agents work in) and `here`, the checkout you
/// are in; or only `only` of them.
pub fn repos(
    registered: BTreeMap<String, PathBuf>,
    s: &State,
    host: &str,
    here: Option<PathBuf>,
    only: Option<&str>,
) -> Result<Vec<(String, PathBuf)>> {
    let mut seen: BTreeSet<PathBuf> = registered.values().cloned().collect();
    let mut out: Vec<(String, PathBuf)> = registered.into_iter().collect();
    let reported = s.worktrees.values().filter(|w| w.state.host == host);
    for dir in reported.map(|w| PathBuf::from(&w.state.repo)).chain(here) {
        if seen.insert(dir.clone()) {
            let slug = crate::spawn::slug_of(&dir).unwrap_or_else(|| dir.display().to_string());
            out.push((slug, dir));
        }
    }
    if let Some(only) = only {
        out.retain(|(slug, _)| slug == only);
        if out.is_empty() {
            bail!("{only} is not connected here: agent-fleet repo add {only} <checkout>");
        }
    }
    Ok(out)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn ancestor(dir: &Path, tip: &str, of: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", tip, of]).is_some()
}

/// Your PRs in `slug`: one `gh pr list`. `AGENT_FLEET_GH` names the binary.
fn prs(slug: &str) -> Result<Vec<Pr>> {
    let gh = std::env::var("AGENT_FLEET_GH").unwrap_or_else(|_| "gh".into());
    let out = std::process::Command::new(&gh)
        .args([
            "pr", "list", "-R", slug, "--state", "all", "--author", "@me",
        ])
        .args([
            "--limit",
            "1000",
            "--json",
            "headRefName,state,headRefOid,number",
        ])
        .output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// A worktree and what `clean` does with it.
struct Row {
    path: String,
    branch: Option<String>,
    pr: String,
    ahead: String,
    dirty: usize,
    /// `retire (pr)`, `keep: <why>`, `keep (<reason>)` or `-`.
    verdict: String,
    reason: Option<&'static str>,
    retire: bool,
    /// The processes clean stops before it retires this worktree.
    stops: Vec<Leftover>,
    /// Its `dev` slot, ports and running services (`dev::line`).
    dev: Option<String>,
}

/// One repository, read: its worktrees and the stale branches no worktree has out.
struct Scan {
    rows: Vec<Row>,
    delete: Vec<(String, &'static str)>,
    kept: usize,
    /// What could not be read, said once above the rows.
    notes: Vec<String>,
}

/// `procs`: `None` when no cwd could be read; every worktree is then kept.
fn scan(
    slug: &str,
    dir: &Path,
    s: &State,
    host: &str,
    here: &Path,
    procs: Option<&Procs>,
    groups: &Procs,
) -> Result<Scan> {
    let mut notes = Vec::new();
    if git(dir, &["fetch", "origin", "--prune", "--quiet"]).is_none() {
        notes.push("fetch failed, origin may be stale".into());
    }
    let base = git(dir, &["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .unwrap_or_else(|| "origin/main".into());
    let default = base.trim_start_matches("origin/").to_string();
    let prs = prs(slug).unwrap_or_else(|e| {
        notes.push(format!("gh failed, PR checks skipped: {e:#}"));
        Vec::new()
    });
    let Some(list) = git(dir, &["worktree", "list", "--porcelain"]) else {
        bail!("{} is not a git checkout", dir.display());
    };
    let workers = crate::doctor::workers(s);
    let mine = procs.map_or_else(HashSet::new, |p| lineage(p, std::process::id()));
    let locks = locks(&list);
    let why = |b: &str, tip: &str| {
        let remote = format!("refs/remotes/origin/{b}");
        let pushed = git(dir, &["rev-parse", "-q", "--verify", &remote]).is_some()
            && ancestor(dir, tip, &remote);
        stale(b, tip, &prs, ancestor(dir, tip, &base), pushed)
    };
    let mut rows = Vec::new();
    for (i, (path, tip, branch)) in crate::scan::parse_worktrees(&list).into_iter().enumerate() {
        let key = worktree_key(host, &path);
        let (stops, guarded) = match procs {
            Some(procs) => leftovers(procs, &path, &mine),
            None => (vec![], vec![]),
        };
        let lock = locks
            .get(&path)
            .map(|r| lock_note(r, dir, &path, crate::scan::alive));
        let agents = workers.get(&key).cloned().unwrap_or_default();
        let dirty =
            git(Path::new(&path), &["status", "--porcelain"]).map_or(0, |o| o.lines().count());
        let block = blocked(
            i == 0,
            here.starts_with(&path),
            lock,
            &agents,
            dirty,
            &guarded,
        );
        let block = block.or_else(|| procs.is_none().then(|| "processes unknown".to_string()));
        let reason = branch.as_deref().and_then(|b| why(b, &tip));
        let (verdict, retire) = match (reason, block) {
            (Some("pr"), None) => ("retire (pr)".into(), true),
            (_, Some(why)) => (format!("keep: {why}"), false),
            (Some(r), None) => (format!("keep ({r})"), false),
            (None, None) => ("-".into(), false),
        };
        let pr = branch
            .as_deref()
            .and_then(|b| prs.iter().find(|p| p.branch == b))
            .map_or("-".into(), |p| {
                format!("#{} {}", p.number, p.state.to_lowercase())
            });
        let ahead = branch
            .as_deref()
            .and_then(|b| git(dir, &["rev-list", "--count", &format!("{base}..{b}")]))
            .unwrap_or_else(|| "-".into());
        let dev = crate::dev::line(Path::new(&path), i == 0, groups);
        rows.push(Row {
            path,
            branch,
            pr,
            ahead,
            dirty,
            verdict,
            reason,
            retire,
            stops: if retire { stops } else { vec![] },
            dev,
        });
    }
    let out: BTreeSet<&str> = rows.iter().filter_map(|r| r.branch.as_deref()).collect();
    let (mut delete, mut kept) = (Vec::new(), 0);
    let refs = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads",
        ],
    )
    .unwrap_or_default();
    for (b, tip) in refs.lines().filter_map(|l| l.split_once(' ')) {
        if b == default || out.contains(b) {
            continue;
        }
        match why(b, tip) {
            Some(r) => delete.push((b.to_string(), r)),
            None => kept += 1,
        }
    }
    Ok(Scan {
        rows,
        delete,
        kept,
        notes,
    })
}

/// `dev ls` (`clean` false) or `dev clean [--execute]`, over `repos`; the report.
pub async fn run(
    fleet: &impl Fleet,
    repos: &[(String, PathBuf)],
    s: &State,
    clean: bool,
    execute: bool,
) -> String {
    let here = std::env::current_dir().unwrap_or_default();
    let host = fleet.host();
    let gh = std::env::var("AGENT_FLEET_GH").unwrap_or_else(|_| "gh".into());
    let procs = table();
    // Process groups and start times, which every platform reports: what `dev` records.
    let groups = Procs::read();
    let mut out = String::new();
    if procs.is_none() {
        out += "processes unknown (no cwd from /proc or lsof): every worktree is kept\n";
    }
    // One line per process clean stops, under its worktree's line.
    let stops = |r: &Row| -> String {
        r.stops
            .iter()
            .map(|l| {
                let cmd: String = l.command.chars().take(100).collect();
                format!("      stop {} {cmd}\n", l.pid)
            })
            .collect()
    };
    for (slug, dir) in repos {
        out += &format!("{slug} {}\n", dir.display());
        let sc = match scan(slug, dir, s, &host, &here, procs.as_ref(), &groups) {
            Ok(sc) => sc,
            Err(e) => {
                out += &format!("  skipped: {e:#}\n");
                continue;
            }
        };
        for n in &sc.notes {
            out += &format!("  {n}\n");
        }
        if !clean {
            out += &format!(
                "  {:<28} {:<14} {:>5} {:>5} {:<30} WORKTREE\n",
                "BRANCH", "PR", "AHEAD", "DIRTY", "CLEAN"
            );
            for r in &sc.rows {
                out += &format!(
                    "  {:<28} {:<14} {:>5} {:>5} {:<30} {}\n",
                    r.branch.as_deref().unwrap_or("-"),
                    r.pr,
                    r.ahead,
                    r.dirty,
                    r.verdict,
                    r.path
                );
                if let Some(d) = &r.dev {
                    out += &format!("      {d}\n");
                }
                out += &stops(r);
            }
            continue;
        }
        let retire: Vec<&Row> = sc.rows.iter().filter(|r| r.retire).collect();
        for r in &sc.rows {
            if r.retire {
                let b = r.branch.as_deref().unwrap_or("-");
                out += &format!("  retire {} [{b}: pr]\n", r.path);
                if let Some(d) = &r.dev {
                    out += &format!("      dev down --release: {d}\n");
                }
                out += &stops(r);
            } else if r.reason == Some("pr") {
                out += &format!("  keep worktree {}: {}\n", r.path, &r.verdict[6..]);
            }
        }
        for (b, why) in &sc.delete {
            out += &format!("  delete {b} ({why})\n");
        }
        out += &format!(
            "  {} worktrees, {} branches; {} kept (open PR or local-only commits)\n",
            retire.len(),
            sc.delete.len(),
            sc.kept
        );
        if !execute {
            continue;
        }
        let repo = dir.to_string_lossy();
        git(dir, &["worktree", "prune"]);
        let mut branches: Vec<&str> = sc.delete.iter().map(|(b, _)| b.as_str()).collect();
        for r in retire {
            // Its dev services first, through their own process groups; then what is left.
            match crate::dev::release(Path::new(&r.path)) {
                Ok(Some(line)) => out += &format!("  {line}\n"),
                Ok(None) => {}
                Err(e) => {
                    out += &format!("  could not release {}'s slot: {e:#}; kept\n", r.path);
                    continue;
                }
            }
            if !r.stops.is_empty() {
                match stop(&r.stops, &r.path, GRACE).await {
                    Ok(line) => out += &format!("  {line}\n"),
                    Err(e) => {
                        out += &format!("  could not stop all in {}: {e}; kept\n", r.path);
                        continue;
                    }
                }
            }
            match crate::tui::retire(&repo, &r.path).await {
                Ok(()) => {
                    out += &format!(
                        "  {}\n",
                        crate::tui::removed_line(fleet, &gh, &repo, &r.path, r.branch.as_deref())
                            .await
                    );
                    branches.extend(r.branch.as_deref());
                }
                Err(e) => out += &format!("  could not remove {}: {e}; branch kept\n", r.path),
            }
        }
        for b in branches {
            // `git branch <b> <sha>` restores one: the sha is in the line it prints.
            match git(dir, &["branch", "-D", b]) {
                Some(line) => out += &format!("  {line}\n"),
                None => out += &format!("  could not delete {b}\n"),
            }
        }
    }
    if clean && !execute {
        out += "dry run; agent-fleet dev clean --execute to do it\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentState, WorktreeState};
    use crate::state::{Agent, Usage, Worktree};
    use chrono::Utc;

    fn pr(branch: &str, state: &str, tip: &str) -> Pr {
        Pr {
            branch: branch.into(),
            state: state.into(),
            tip: tip.into(),
            number: 1,
        }
    }

    #[test]
    fn the_rules() {
        let prs = [
            pr("merged", "MERGED", "t1"),
            pr("closed", "CLOSED", "t2"),
            pr("open", "OPEN", "t3"),
            pr("open", "MERGED", "t3"),
            pr("moved", "MERGED", "old"),
        ];
        assert_eq!(stale("merged", "t1", &prs, false, false), Some("pr"));
        assert_eq!(
            stale("merged", "t1", &prs, true, true),
            Some("pr"),
            "merge commit"
        );
        assert_eq!(stale("closed", "t2", &prs, false, false), Some("pr"));
        assert_eq!(
            stale("open", "t3", &prs, true, true),
            None,
            "an open PR is kept"
        );
        assert_eq!(
            stale("moved", "new", &prs, false, false),
            None,
            "commits after the PR"
        );
        assert_eq!(stale("moved", "new", &prs, false, true), Some("pushed"));
        assert_eq!(stale("fresh", "t", &prs, true, false), Some("in main"));
        assert_eq!(
            stale("local", "t", &prs, false, false),
            None,
            "local-only commits"
        );
    }

    #[test]
    fn what_keeps_a_worktree() {
        let none: [String; 0] = [];
        assert_eq!(blocked(false, false, None, &none, 0, &none), None);
        assert_eq!(
            blocked(true, false, None, &none, 0, &none).unwrap(),
            "main checkout"
        );
        assert_eq!(
            blocked(false, true, None, &none, 0, &none).unwrap(),
            "you are in it"
        );
        assert_eq!(
            blocked(false, false, None, &none, 2, &none).unwrap(),
            "2 uncommitted files"
        );
        let lock = Some("locked".to_string());
        assert_eq!(
            blocked(false, true, lock, &none, 2, &none).unwrap(),
            "locked",
            "lock first"
        );
        let bun = ["bun".to_string()];
        assert_eq!(
            blocked(false, false, None, &none, 0, &bun).unwrap(),
            "bun runs in it (an agent's or yours)"
        );
    }

    #[test]
    fn a_lock_is_said_and_a_stale_one_gets_its_unlock_command() {
        let list = "worktree /r\nHEAD a\nbranch refs/heads/main\n\n\
                    worktree /w/sub\nHEAD b\nbranch refs/heads/x\n\
                    locked claude agent a1 (pid 42 start 9)\n\n\
                    worktree /w/bare\nHEAD c\ndetached\nlocked\n";
        let l = locks(list);
        assert_eq!(l.len(), 2, "{l:?}");
        assert_eq!(l["/w/bare"], "");
        let reason = &l["/w/sub"];
        let repo = Path::new("/r");
        assert_eq!(
            lock_note(reason, repo, "/w/sub", |_| false),
            "locked, stale (pid 42 gone): git -C /r worktree unlock /w/sub"
        );
        assert_eq!(
            lock_note(reason, repo, "/w/sub", |p| p == 42),
            "locked: claude agent a1 (pid 42 start 9)"
        );
        assert_eq!(lock_note("", repo, "/w/bare", |_| false), "locked");
    }

    fn proc(ppid: u32, name: &str, cwd: &str) -> crate::scan::Proc {
        crate::scan::Proc {
            ppid,
            cpu_ms: 0,
            rss_kb: 0,
            name: name.into(),
            cwd: Some(cwd.into()),
            deleted: false,
            args: Some(format!("{name} --flag")),
            started: None,
            pgid: None,
        }
    }

    #[test]
    fn leftovers_are_stopped_but_never_an_agents_a_panes_or_the_callers() {
        let procs = Procs(std::collections::HashMap::from([
            (1, proc(0, "init", "/")),
            (10, proc(1, "bun", "/w/done/web")), // left behind: stopped
            (20, proc(1, "herdr", "/w/done")),
            (21, proc(20, "zsh", "/w/done")), // a pane's shell
            (30, proc(1, "claude", "/w/other")),
            (31, proc(30, "bash", "/w/other")),
            (32, proc(31, "cargo", "/w/done")), // an agent elsewhere runs it here
            (40, proc(1, "sh", "/w/done")),     // the caller's ancestor
            (41, proc(40, "agent-fleet", "/w/done")),
            (50, proc(1, "vite", "/w/donee")), // a sibling directory
        ]));
        let mine = lineage(&procs, 41);
        assert_eq!(mine, HashSet::from([41, 40, 1]));
        let (stop, guarded) = leftovers(&procs, "/w/done", &mine);
        let pids: Vec<u32> = stop.iter().map(|l| l.pid).collect();
        assert_eq!(pids, [10]);
        assert_eq!(stop[0].command, "bun --flag");
        assert_eq!(guarded, ["agent-fleet", "cargo", "herdr", "sh", "zsh"]);
        let none: [String; 0] = [];
        assert_eq!(
            blocked(false, false, None, &none, 0, &guarded[1..2]).unwrap(),
            "cargo runs in it (an agent's or yours)"
        );
    }

    #[test]
    fn cwds_come_from_lsof_where_the_table_has_none_and_unknown_is_none() {
        let mut bare = proc(1, "bun", "/x");
        bare.cwd = None;
        let table = || Procs(std::collections::HashMap::from([(7, bare.clone())]));
        let lsof = "p7\nfcwd\nn/w/done\np8\nfcwd\nn/w/other\n";
        assert_eq!(parse_lsof_cwds(lsof).len(), 2);
        let got = with_cwds(table(), || Some(lsof.into())).unwrap();
        assert_eq!(got.0[&7].cwd.as_deref(), Some("/w/done"));
        assert!(with_cwds(table(), || None).is_none(), "no lsof");
        assert!(
            with_cwds(table(), || Some(String::new())).is_none(),
            "lsof saw nothing"
        );
        let linux = Procs(std::collections::HashMap::from([(7, proc(1, "bun", "/w"))]));
        assert!(with_cwds(linux, || panic!("not asked")).is_some());
    }

    #[tokio::test]
    async fn a_leftover_is_stopped_and_a_reused_pid_is_not_hit() {
        let dir = tempfile::tempdir().unwrap();
        // The worktree path names a symlink; the process's cwd is the resolved directory.
        let (target, link) = (dir.path().join("real"), dir.path().join("link"));
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let path = link.to_str().unwrap().to_string();
        // Orphaned, as a leftover is: not under the agent that may be running this test.
        let out = std::process::Command::new("sh")
            .args(["-c", "sleep 60 >/dev/null 2>&1 & echo $!"])
            .current_dir(&path)
            .output()
            .unwrap();
        let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        let procs = table().expect("cwds readable");
        let (them, _) = leftovers(&procs, &path, &HashSet::new());
        assert!(them.iter().any(|l| l.pid == pid), "{them:?}");
        let mut reused = them.clone();
        for l in &mut reused {
            l.started = l.started.map(|t| t - chrono::Duration::seconds(5));
        }
        assert_eq!(stop(&reused, &path, GRACE).await.unwrap(), "stopped ");
        assert!(crate::scan::alive(pid), "another start time: not hit");
        let line = stop(&them, &path, GRACE).await.unwrap();
        assert!(line.contains(&format!("{pid} TERM")), "{line}");
    }

    fn state_with(path: &str, cwd: &str) -> State {
        let now = Utc::now();
        let mut s = State::default();
        let w = Worktree {
            state: WorktreeState {
                host: "h".into(),
                path: path.into(),
                repo: "/r/main".into(),
                branch: Some("b".into()),
                head: "abc".into(),
                unpushed: 0,
                dirty: 0,
                dirty_submodules: 0,
                files: vec![],
                last_commit: now,
                in_main: true,
                herdr: true,
            },
            since: now,
        };
        s.worktrees.insert(worktree_key("h", path), w);
        s.agents.insert(
            "agent:h/s/c-1".into(),
            Agent {
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Working,
                since: now,
                worked: None,
                finished: None,
                usage: Some(Usage {
                    rss_kb: None,
                    cpu_pct: None,
                    procs: None,
                    cwd: Some(cwd.into()),
                    ts: now,
                }),
            },
        );
        s
    }

    #[test]
    fn a_live_agents_worktree_is_kept_and_named() {
        let s = state_with("/w/c-1", "/w/c-1/src");
        let agents = crate::doctor::workers(&s)
            .remove(&worktree_key("h", "/w/c-1"))
            .unwrap();
        assert_eq!(
            blocked(false, false, None, &agents, 0, &[]).unwrap(),
            "c-1 works in it"
        );
        let s = state_with("/w/c-1", "/elsewhere");
        assert!(crate::doctor::workers(&s).is_empty());
    }

    #[test]
    fn repos_are_the_connected_ones_and_the_hosts() {
        let s = state_with("/w/c-1", "/w/c-1");
        let reg = BTreeMap::from([("o/a".to_string(), PathBuf::from("/r/a"))]);
        let here = Some(PathBuf::from("/r/here"));
        let all = repos(reg.clone(), &s, "h", here.clone(), None).unwrap();
        let dirs: Vec<&Path> = all.iter().map(|(_, d)| d.as_path()).collect();
        assert_eq!(
            dirs,
            [
                Path::new("/r/a"),
                Path::new("/r/main"),
                Path::new("/r/here")
            ]
        );
        assert_eq!(
            repos(reg.clone(), &s, "other", None, None).unwrap().len(),
            1
        );
        assert_eq!(
            repos(reg.clone(), &s, "h", None, Some("o/a"))
                .unwrap()
                .len(),
            1
        );
        assert!(repos(reg, &s, "h", here, Some("o/b")).is_err());
    }
}
