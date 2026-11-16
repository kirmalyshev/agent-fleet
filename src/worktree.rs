//! `worktree ls` and `worktree clean`: the worktrees and local branches of every repository
//! connected on this machine, and which of them are finished.
//!
//! A local branch is stale when its tip is safe somewhere else: it is the head of one of your
//! merged or closed PRs (`pr`), an ancestor of the default branch (`in main`), or contained in
//! `origin/<branch>` (`pushed`). A branch with an open PR is never stale, and local-only commits
//! are never deleted. Only `pr` retires a worktree: one just made off main is `in main` too.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;

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
fn blocked(
    main: bool,
    here: bool,
    dirty: usize,
    agents: &[String],
    procs: &[String],
) -> Option<String> {
    if main {
        Some("main checkout".into())
    } else if here {
        Some("you are in it".into())
    } else if !agents.is_empty() {
        let names: Vec<&str> = agents.iter().map(|a| crate::host::short(a)).collect();
        Some(format!("{} works in it", names.join(", ")))
    } else if dirty > 0 {
        Some(format!("{dirty} uncommitted files"))
    } else if !procs.is_empty() {
        Some(format!("{} runs in it", procs.join(", ")))
    } else {
        None
    }
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
}

/// One repository, read: its worktrees and the stale branches no worktree has out.
struct Scan {
    rows: Vec<Row>,
    delete: Vec<(String, &'static str)>,
    kept: usize,
    /// What could not be read, said once above the rows.
    notes: Vec<String>,
}

fn scan(slug: &str, dir: &Path, s: &State, host: &str, here: &Path) -> Result<Scan> {
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
    let why = |b: &str, tip: &str| {
        let remote = format!("refs/remotes/origin/{b}");
        let pushed = git(dir, &["rev-parse", "-q", "--verify", &remote]).is_some()
            && ancestor(dir, tip, &remote);
        stale(b, tip, &prs, ancestor(dir, tip, &base), pushed)
    };
    let mut rows = Vec::new();
    for (i, (path, tip, branch)) in crate::scan::parse_worktrees(&list).into_iter().enumerate() {
        let key = worktree_key(host, &path);
        let procs = s
            .worktree_procs
            .get(&key)
            .filter(|p| p.procs.procs > 0)
            .map_or(vec![], |p| p.procs.names.clone());
        let agents = workers.get(&key).cloned().unwrap_or_default();
        let dirty =
            git(Path::new(&path), &["status", "--porcelain"]).map_or(0, |o| o.lines().count());
        let block = blocked(i == 0, here.starts_with(&path), dirty, &agents, &procs);
        let reason = branch.as_deref().and_then(|b| why(b, &tip));
        let (verdict, retire) = match (reason, block) {
            (Some("pr"), None) => ("retire (pr)".into(), true),
            (Some(_), Some(why)) => (format!("keep: {why}"), false),
            (Some(r), None) => (format!("keep ({r})"), false),
            (None, _) => ("-".into(), false),
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
        rows.push(Row {
            path,
            branch,
            pr,
            ahead,
            dirty,
            verdict,
            reason,
            retire,
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

/// `worktree ls` (`clean` false) or `worktree clean [--execute]`, over `repos`; the report.
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
    let mut out = String::new();
    for (slug, dir) in repos {
        out += &format!("{slug} {}\n", dir.display());
        let sc = match scan(slug, dir, s, &host, &here) {
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
            }
            continue;
        }
        let retire: Vec<&Row> = sc.rows.iter().filter(|r| r.retire).collect();
        for r in &sc.rows {
            if r.retire {
                let b = r.branch.as_deref().unwrap_or("-");
                out += &format!("  retire {} [{b}: pr]\n", r.path);
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
        out += "dry run; agent-fleet worktree clean --execute to do it\n";
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
        assert_eq!(blocked(false, false, 0, &none, &none), None);
        assert_eq!(
            blocked(true, false, 0, &none, &none).unwrap(),
            "main checkout"
        );
        assert_eq!(
            blocked(false, true, 0, &none, &none).unwrap(),
            "you are in it"
        );
        assert_eq!(
            blocked(false, false, 2, &none, &none).unwrap(),
            "2 uncommitted files"
        );
        let procs = ["bun".to_string()];
        assert_eq!(
            blocked(false, false, 0, &none, &procs).unwrap(),
            "bun runs in it"
        );
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
            blocked(false, false, 0, &agents, &[]).unwrap(),
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
