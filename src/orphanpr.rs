//! Pull requests no live agent owns: an open PR in a fleet repo whose head branch is no live
//! agent's and that has sat untouched for `days`. `classify` is pure; `fetch` and `close` run `gh`.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// `[orphan_pr]` in fleet.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `owner/repo` list to look at; empty = the check is off.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Quiet this many days before a PR is an orphan.
    #[serde(default = "default_days")]
    pub days: i64,
    /// A PR touching one of these waits for a person. `dir/` is a prefix, anything else a file
    /// name (at any depth) or exact path.
    #[serde(default = "default_guarded")]
    pub guarded: Vec<String>,
}

fn default_days() -> i64 {
    3
}

fn default_guarded() -> Vec<String> {
    ["AGENTS.md", "CLAUDE.md", "README.md", "docs/", "schema/"]
        .map(String::from)
        .into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            days: default_days(),
            guarded: default_guarded(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pr {
    pub number: u64,
    pub head: String,
    pub draft: bool,
    pub updated: DateTime<Utc>,
    /// Issues the PR closes.
    pub closes: Vec<u64>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub repo: String,
    pub open: Vec<Pr>,
    /// Merged PRs: number → the issues they closed.
    pub merged: BTreeMap<u64, Vec<u64>>,
    /// Issue number → `OPEN` or `CLOSED`.
    pub issues: BTreeMap<u64, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A newer merged PR closes the same issue: close it with a comment, keep the branch.
    Close { by: u64 },
    /// Tell the root coordinator; why a person has to look.
    Report(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub repo: String,
    pub number: u64,
    pub age: Duration,
    pub detail: String,
    pub action: Action,
}

fn guarded(files: &[String], patterns: &[String]) -> bool {
    files.iter().any(|f| {
        patterns.iter().any(|p| {
            if p.ends_with('/') {
                f.starts_with(p.as_str())
            } else {
                f == p || f.rsplit('/').next() == Some(p.as_str())
            }
        })
    })
}

/// The open PRs of `snap` that no live agent's branch owns and that are `cfg.days` quiet.
pub fn classify(
    snap: &Snapshot,
    live: &BTreeSet<String>,
    cfg: &Config,
    now: DateTime<Utc>,
) -> Vec<Orphan> {
    let mut out = Vec::new();
    for pr in &snap.open {
        let age = now - pr.updated;
        if live.contains(&pr.head) || age < Duration::days(cfg.days) {
            continue;
        }
        let by = snap
            .merged
            .iter()
            .find(|(n, closed)| **n > pr.number && closed.iter().any(|i| pr.closes.contains(i)))
            .map(|(n, _)| *n);
        let action = if pr.draft {
            Action::Report("a draft waits for a person")
        } else if guarded(&pr.files, &cfg.guarded) {
            Action::Report("touches a guarded surface, waits for a person")
        } else if let Some(by) = by {
            Action::Close { by }
        } else {
            Action::Report("no newer merged PR closes its issue")
        };
        let issues = if pr.closes.is_empty() {
            "closes no issue".to_string()
        } else {
            let v: Vec<String> = pr
                .closes
                .iter()
                .map(|i| {
                    format!(
                        "#{i} {}",
                        snap.issues.get(i).map_or("?", |s| s).to_lowercase()
                    )
                })
                .collect();
            format!("closes {}", v.join(", "))
        };
        let newer = by
            .map(|n| format!("; #{n} merged since"))
            .unwrap_or_default();
        out.push(Orphan {
            repo: snap.repo.clone(),
            number: pr.number,
            age,
            detail: format!("{}: {}d quiet, {issues}{newer}", pr.head, age.num_days()),
            action,
        });
    }
    out
}

async fn gh(bin: &str, args: &[&str]) -> Result<Value> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("run gh {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).with_context(|| format!("gh {}: bad JSON", args.join(" ")))
}

fn closes(v: &Value) -> Vec<u64> {
    v["closingIssuesReferences"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["number"].as_u64())
        .collect()
}

/// What `repo` has open, merged and closed, read with `gh` (`AGENT_FLEET_GH` names the binary).
pub async fn fetch(bin: &str, repo: &str) -> Result<Snapshot> {
    let open = gh(
        bin,
        &[
            "pr",
            "list",
            "-R",
            repo,
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "number,headRefName,isDraft,updatedAt,closingIssuesReferences,files",
        ],
    )
    .await?;
    let merged = gh(
        bin,
        &[
            "pr",
            "list",
            "-R",
            repo,
            "--state",
            "merged",
            "--limit",
            "100",
            "--json",
            "number,closingIssuesReferences",
        ],
    )
    .await?;
    let issues = gh(
        bin,
        &[
            "issue",
            "list",
            "-R",
            repo,
            "--state",
            "all",
            "--limit",
            "300",
            "--json",
            "number,state",
        ],
    )
    .await?;
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    Ok(Snapshot {
        repo: repo.to_string(),
        open: list(&open)
            .iter()
            .filter_map(|p| {
                Some(Pr {
                    number: p["number"].as_u64()?,
                    head: p["headRefName"].as_str()?.to_string(),
                    draft: p["isDraft"].as_bool().unwrap_or(false),
                    updated: p["updatedAt"].as_str()?.parse().ok()?,
                    closes: closes(p),
                    files: p["files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|f| f["path"].as_str().map(str::to_string))
                        .collect(),
                })
            })
            .collect(),
        merged: list(&merged)
            .iter()
            .filter_map(|p| Some((p["number"].as_u64()?, closes(p))))
            .collect(),
        issues: list(&issues)
            .iter()
            .filter_map(|i| Some((i["number"].as_u64()?, i["state"].as_str()?.to_string())))
            .collect(),
    })
}

/// Close PR `n` with a comment; the branch stays.
pub async fn close(bin: &str, repo: &str, n: u64, by: u64) -> Result<()> {
    let comment = format!(
        "Closed by agent-fleet doctor: no live agent owns this branch, and #{by} merged since \
         and closes the same issue. The branch is kept."
    );
    let out = tokio::process::Command::new(bin)
        .args([
            "pr",
            "close",
            &n.to_string(),
            "-R",
            repo,
            "--comment",
            &comment,
        ])
        .output()
        .await
        .context("run gh pr close")?;
    if !out.status.success() {
        bail!(
            "gh pr close {n} -R {repo}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Head branches of the worktrees live agents work in.
pub fn live_branches(s: &crate::state::State) -> BTreeSet<String> {
    s.agents
        .values()
        .filter_map(|a| crate::doctor::worktree_of(s, a))
        .filter_map(|(_, w)| w.state.branch.clone())
        .collect()
}

/// Every orphan in the configured repos. A repo `gh` cannot read is an error line, not a
/// silent pass.
pub async fn scan(
    bin: &str,
    s: &crate::state::State,
    cfg: &Config,
    now: DateTime<Utc>,
) -> (Vec<Orphan>, Vec<String>) {
    let live = live_branches(s);
    let (mut found, mut errs) = (Vec::new(), Vec::new());
    for repo in &cfg.repos {
        match fetch(bin, repo).await {
            Ok(snap) => found.extend(classify(&snap, &live, cfg, now)),
            Err(e) => errs.push(format!("{repo}: {e:#}")),
        }
    }
    (found, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-16T12:00:00Z".parse().unwrap()
    }

    fn pr(n: u64, head: &str, days_old: i64, closes: &[u64]) -> Pr {
        Pr {
            number: n,
            head: head.into(),
            draft: false,
            updated: now() - Duration::days(days_old),
            closes: closes.to_vec(),
            files: vec!["src/a.rs".into()],
        }
    }

    fn snap(open: Vec<Pr>) -> Snapshot {
        Snapshot {
            repo: "o/r".into(),
            open,
            merged: BTreeMap::from([(9, vec![5])]),
            issues: BTreeMap::from([(5, "CLOSED".into()), (6, "OPEN".into())]),
        }
    }

    fn run(s: &Snapshot, live: &[&str]) -> Vec<Orphan> {
        let live = live.iter().map(|s| s.to_string()).collect();
        classify(s, &live, &Config::default(), now())
    }

    #[test]
    fn a_live_agents_branch_is_no_orphan() {
        assert!(run(&snap(vec![pr(1, "s-1", 30, &[6])]), &["s-1"]).is_empty());
    }

    #[test]
    fn a_recent_pr_is_no_orphan() {
        assert!(run(&snap(vec![pr(1, "s-1", 2, &[6])]), &[]).is_empty());
    }

    #[test]
    fn a_dead_stale_pr_is_reported_with_age_and_issue_state() {
        let o = run(&snap(vec![pr(1, "s-1", 4, &[6])]), &[]);
        assert_eq!(o.len(), 1);
        assert_eq!(
            o[0].action,
            Action::Report("no newer merged PR closes its issue")
        );
        assert_eq!(o[0].age, Duration::days(4));
        assert!(o[0].detail.contains("4d"), "{}", o[0].detail);
        assert!(o[0].detail.contains("#6 open"), "{}", o[0].detail);
    }

    #[test]
    fn a_pr_a_newer_merged_pr_supersedes_is_closed() {
        let o = run(&snap(vec![pr(1, "s-1", 4, &[5])]), &[]);
        assert_eq!(o[0].action, Action::Close { by: 9 });
        assert!(o[0].detail.contains("#5 closed"), "{}", o[0].detail);
    }

    #[test]
    fn an_older_merged_pr_does_not_supersede() {
        let mut s = snap(vec![pr(20, "s-1", 4, &[5])]);
        s.merged = BTreeMap::from([(9, vec![5])]);
        assert!(matches!(run(&s, &[])[0].action, Action::Report(_)));
    }

    /// A fake `gh` answering each call from canned JSON, logging `pr close` calls.
    fn fake_gh(dir: &std::path::Path) -> String {
        let log = dir.join("calls");
        let path = dir.join("gh");
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
case "$1 $2 $6" in
  "pr list open") echo '[{{"number":1,"headRefName":"s-1","isDraft":false,"updatedAt":"2026-11-01T00:00:00Z","closingIssuesReferences":[{{"number":5}}],"files":[{{"path":"src/a.rs"}}]}},{{"number":2,"headRefName":"s-2","isDraft":false,"updatedAt":"2026-11-01T00:00:00Z","closingIssuesReferences":[{{"number":6}}],"files":[{{"path":"src/b.rs"}}]}}]' ;;
  "pr list merged") echo '[{{"number":9,"closingIssuesReferences":[{{"number":5}}]}}]' ;;
  "issue list all") echo '[{{"number":5,"state":"CLOSED"}},{{"number":6,"state":"OPEN"}}]' ;;
  "pr close"*) echo "$@" >> {log:?} ;;
  *) echo "unexpected: $@" >&2; exit 1 ;;
esac
"#
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn scan_reads_gh_and_close_keeps_the_branch() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(dir.path());
        let cfg = Config {
            repos: vec!["o/r".into()],
            ..Config::default()
        };
        let s = crate::state::State::default();
        let (found, errs) = scan(&bin, &s, &cfg, "2026-11-16T12:00:00Z".parse().unwrap()).await;
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].action, Action::Close { by: 9 });
        assert!(matches!(found[1].action, Action::Report(_)));
        close(&bin, "o/r", 1, 9).await.unwrap();
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(calls.starts_with("pr close 1 -R o/r --comment"), "{calls}");
        assert!(!calls.contains("--delete-branch"), "{calls}");
    }

    #[tokio::test]
    async fn an_unreadable_repo_is_an_error_not_a_pass() {
        let cfg = Config {
            repos: vec!["o/r".into()],
            ..Config::default()
        };
        let (found, errs) = scan(
            "/nonexistent/gh",
            &crate::state::State::default(),
            &cfg,
            Utc::now(),
        )
        .await;
        assert!(found.is_empty());
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn a_draft_or_guarded_pr_is_reported_never_closed() {
        let mut d = pr(1, "s-1", 4, &[5]);
        d.draft = true;
        let mut g = pr(2, "s-2", 4, &[5]);
        g.files = vec!["src/AGENTS.md".into()];
        let mut p = pr(3, "s-3", 4, &[5]);
        p.files = vec!["docs/PROTOCOL.md".into()];
        for o in run(&snap(vec![d, g, p]), &[]) {
            assert!(matches!(o.action, Action::Report(_)), "{o:?}");
        }
    }
}
