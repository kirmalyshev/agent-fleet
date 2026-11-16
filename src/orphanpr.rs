//! Pull requests no live agent owns: an open PR in a fleet repo whose head branch is no live
//! agent's and that has sat untouched for `days`. `classify` is pure; `fetch` and `close` run `gh`.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// `gh pr list` cuts `files` at this many; a PR at the cap may hide a guarded file.
const FILES_CAP: usize = 100;
const OPEN_LIMIT: usize = 100;

/// `[orphan_pr]` in fleet.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `owner/repo` list to look at; empty = the check is off.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Quiet this many days before a PR is an orphan.
    #[serde(default = "default_days")]
    pub days: u32,
    /// A PR touching one of these waits for a person. `dir/` is a prefix, anything else a file
    /// name (at any depth) or exact path.
    #[serde(default = "default_guarded")]
    pub guarded: Vec<String>,
}

fn default_days() -> u32 {
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
    /// It closes an issue in another repo (or one `gh` did not place): never covered.
    pub foreign: bool,
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
    /// A newer merged PR closes the same issue (`by`), or every issue it closes is closed already
    /// (`None`): close it with a comment, keep the branch.
    Close { by: Option<u64> },
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
        if live.contains(&pr.head) || age < Duration::days(cfg.days.into()) {
            continue;
        }
        let newer = |i: &u64| {
            snap.merged
                .iter()
                .find(|(n, closed)| **n > pr.number && closed.contains(i))
                .map(|(n, _)| *n)
        };
        let closed = |i: &u64| snap.issues.get(i).is_some_and(|s| s == "CLOSED");
        // Every issue it closes must be covered, and none may live in another repo.
        let covered = !pr.closes.is_empty()
            && !pr.foreign
            && pr.closes.iter().all(|i| closed(i) || newer(i).is_some());
        let by = pr.closes.iter().find_map(newer);
        let action = if pr.draft {
            Action::Report("a draft waits for a person")
        } else if pr.files.len() >= FILES_CAP || guarded(&pr.files, &cfg.guarded) {
            Action::Report("touches a guarded surface, waits for a person")
        } else if covered {
            Action::Close { by }
        } else {
            Action::Report("a newer merged PR or a closed issue does not cover all its issues")
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

/// The issues in `repo` that `v` closes, and whether it closes any other.
fn closes(v: &Value, repo: &str) -> (Vec<u64>, bool) {
    let (mut here, mut foreign) = (Vec::new(), false);
    for i in v["closingIssuesReferences"]
        .as_array()
        .into_iter()
        .flatten()
    {
        // `gh` gives `repository: {name, owner: {login}}`, not `nameWithOwner`.
        let r = &i["repository"];
        let at = match (r["owner"]["login"].as_str(), r["name"].as_str()) {
            (Some(o), Some(n)) => Some(format!("{o}/{n}")),
            _ => None,
        };
        match (i["number"].as_u64(), at) {
            (Some(n), Some(r)) if r.eq_ignore_ascii_case(repo) => here.push(n),
            _ => foreign = true,
        }
    }
    (here, foreign)
}

/// What `repo` has open (at most `OPEN_LIMIT`, else an error), the last 100 merged and 300 issues,
/// read with `gh`. A row missing a field is an error: a PR is never judged on a guess.
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
    let list = |what: &str, v: &Value| -> Result<Vec<Value>> {
        v.as_array()
            .cloned()
            .with_context(|| format!("gh {what}: not a list"))
    };
    let open = list("pr list --state open", &open)?;
    if open.len() >= OPEN_LIMIT {
        bail!(
            "{} has {OPEN_LIMIT} or more open PRs: the list may be cut short",
            repo
        );
    }
    // ponytail: merged and issues cap at their limits; past them a PR is reported, not closed.
    let row = |p: &Value| -> Option<Pr> {
        Some(Pr {
            number: p["number"].as_u64()?,
            head: p["headRefName"].as_str()?.to_string(),
            draft: p["isDraft"].as_bool()?,
            updated: p["updatedAt"].as_str()?.parse().ok()?,
            closes: closes(p, repo).0,
            foreign: closes(p, repo).1,
            files: p["files"]
                .as_array()?
                .iter()
                .map(|f| f["path"].as_str().map(str::to_string))
                .collect::<Option<_>>()?,
        })
    };
    let mut prs = Vec::new();
    for p in &open {
        prs.push(row(p).with_context(|| format!("gh pr list: unreadable PR {}", p["number"]))?);
    }
    Ok(Snapshot {
        repo: repo.to_string(),
        open: prs,
        merged: list("pr list --state merged", &merged)?
            .iter()
            .filter_map(|p| Some((p["number"].as_u64()?, closes(p, repo).0)))
            .collect(),
        issues: list("issue list", &issues)?
            .iter()
            .filter_map(|i| Some((i["number"].as_u64()?, i["state"].as_str()?.to_string())))
            .collect(),
    })
}

/// Close PR `n` with a comment; the branch stays.
pub async fn close(bin: &str, repo: &str, n: u64, by: Option<u64>) -> Result<()> {
    let why = match by {
        Some(by) => format!("#{by} merged since and closes the same issue"),
        None => "the issue it closes is closed already".to_string(),
    };
    let comment = format!(
        "Closed by agent-fleet doctor: no live agent owns this branch, and {why}. The branch is kept."
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

/// Head branches of the worktrees live agents work in, and whether some agent has none we can
/// name (no cwd report, no worktree, detached HEAD): then any PR might be its.
pub fn live_branches(s: &crate::state::State) -> (BTreeSet<String>, bool) {
    let mut set = BTreeSet::new();
    let mut unknown = false;
    for a in s.agents.values() {
        match crate::doctor::worktree_of(s, a).and_then(|(_, w)| w.state.branch.clone()) {
            Some(b) => {
                set.insert(b);
            }
            None => unknown = true,
        }
    }
    (set, unknown)
}

/// Every orphan in the configured repos, and `(repo, error)` for each repo `gh` could not
/// read: a failure is a line, never a silent pass. With an agent whose branch is unknown, no
/// PR is closed.
pub async fn scan(
    bin: &str,
    s: &crate::state::State,
    cfg: &Config,
    now: DateTime<Utc>,
) -> (Vec<Orphan>, Vec<(String, String)>) {
    let (live, unknown) = live_branches(s);
    let (mut found, mut errs) = (Vec::new(), Vec::new());
    for repo in &cfg.repos {
        match fetch(bin, repo).await {
            Ok(snap) => found.extend(classify(&snap, &live, cfg, now)),
            Err(e) => errs.push((repo.clone(), format!("{e:#}"))),
        }
    }
    if unknown {
        for o in &mut found {
            if matches!(o.action, Action::Close { .. }) {
                o.action = Action::Report("an agent's branch is unknown, so nothing is closed");
            }
        }
    }
    (found, errs)
}

/// `[orphan_pr]` of the fleet.toml in `dir`'s checkout (or the machine's own). No fleet.toml
/// anywhere means the check is off: `Ok(None)`. One that cannot be read or parsed is an error.
pub fn config(dir: &std::path::Path) -> Result<Option<Config>> {
    let top = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    match crate::spawn::load(top.as_deref().unwrap_or(dir)) {
        Ok((c, _)) => Ok(Some(c.orphan_pr)),
        Err(e) if missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn missing(e: &anyhow::Error) -> bool {
    e.root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// The root coordinator's agent id.
pub fn root_of(s: &crate::state::State) -> Option<String> {
    s.roles
        .iter()
        .find(|(_, r)| r.level == crate::event::Level::RootCoordinator)
        .map(|(id, _)| id.clone())
}

/// A message to `root` about `key` is still pending (delivery removes it from the state).
pub fn already_told(s: &crate::state::State, root: &str, key: &str) -> bool {
    s.msgs.values().any(|m| m.to == root && m.reference == key)
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
            foreign: false,
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
            Action::Report("a newer merged PR or a closed issue does not cover all its issues")
        );
        assert_eq!(o[0].age, Duration::days(4));
        assert!(o[0].detail.contains("4d"), "{}", o[0].detail);
        assert!(o[0].detail.contains("#6 open"), "{}", o[0].detail);
    }

    #[test]
    fn a_pr_a_newer_merged_pr_supersedes_is_closed() {
        let o = run(&snap(vec![pr(1, "s-1", 4, &[5])]), &[]);
        assert_eq!(o[0].action, Action::Close { by: Some(9) });
        assert!(o[0].detail.contains("#5 closed"), "{}", o[0].detail);
    }

    #[test]
    fn a_pr_whose_every_issue_is_closed_is_closed_with_no_successor() {
        let mut s = snap(vec![pr(20, "s-1", 4, &[5]), pr(21, "s-2", 4, &[5, 6])]);
        s.merged.clear();
        let o = run(&s, &[]);
        assert_eq!(o[0].action, Action::Close { by: None });
        assert!(matches!(o[1].action, Action::Report(_)), "#6 is open");
    }

    #[test]
    fn one_covered_issue_of_two_is_not_enough() {
        // PR closes 5 and 6; a newer merged PR closes only 5, and 6 is open.
        let o = run(&snap(vec![pr(1, "s-1", 4, &[5, 6])]), &[]);
        assert!(matches!(o[0].action, Action::Report(_)), "{o:?}");
    }

    #[test]
    fn an_issue_in_another_repo_is_never_covered() {
        let mut p = pr(1, "s-1", 4, &[5]);
        p.foreign = true;
        assert!(matches!(
            run(&snap(vec![p]), &[])[0].action,
            Action::Report(_)
        ));
    }

    #[test]
    fn closing_refs_are_matched_on_repo_and_number() {
        // The shape `gh pr list --json closingIssuesReferences` prints (ids and names anonymised).
        let v: Value = serde_json::from_str(
            r#"{"closingIssuesReferences":[
              {"id":"I_a","number":5,"repository":{"id":"R_a","name":"r","owner":{"id":"U_a","login":"o"}},"url":"https://github.com/o/r/issues/5"},
              {"id":"I_b","number":5,"repository":{"id":"R_b","name":"y","owner":{"id":"U_b","login":"x"}},"url":"https://github.com/x/y/issues/5"},
              {"number":7}]}"#,
        )
        .unwrap();
        assert_eq!(closes(&v, "o/r"), (vec![5], true));
        assert_eq!(closes(&serde_json::json!({}), "o/r"), (vec![], false));
    }

    #[test]
    fn no_fleet_toml_is_off_and_a_broken_one_is_an_error() {
        let gone = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context("read fleet.toml");
        assert!(missing(&gone));
        assert!(!missing(&anyhow::anyhow!("fleet.toml: bad")));
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join(".agent-fleet");
        std::fs::create_dir(&c).unwrap();
        std::fs::write(
            c.join("fleet.toml"),
            "brief = \"b.md\"\n[orphan_pr]\ndayz = 1\n",
        )
        .unwrap();
        assert!(config(dir.path()).is_err());
        std::fs::write(
            c.join("fleet.toml"),
            "brief = \"b.md\"\n[profiles.p]\nkind = \"claude\"\n[[rules]]\nprofile = \"p\"\n[orphan_pr]\nrepos = [\"o/r\"]\n",
        )
        .unwrap();
        assert_eq!(config(dir.path()).unwrap().unwrap().repos, ["o/r"]);
    }

    #[test]
    fn an_older_merged_pr_does_not_supersede() {
        let mut s = snap(vec![pr(20, "s-1", 4, &[6])]);
        s.merged = BTreeMap::from([(9, vec![6])]);
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
  "pr list open") echo '[{{"number":1,"headRefName":"s-1","isDraft":false,"updatedAt":"2026-11-01T00:00:00Z","closingIssuesReferences":[{{"id":"I_x","number":5,"repository":{{"id":"R_x","name":"r","owner":{{"id":"U_x","login":"o"}}}},"url":"https://github.com/o/r/issues/5"}}],"files":[{{"path":"src/a.rs"}}]}},{{"number":2,"headRefName":"s-2","isDraft":false,"updatedAt":"2026-11-01T00:00:00Z","closingIssuesReferences":[{{"id":"I_y","number":6,"repository":{{"id":"R_x","name":"r","owner":{{"id":"U_x","login":"o"}}}},"url":"https://github.com/o/r/issues/6"}}],"files":[{{"path":"src/b.rs"}}]}}]' ;;
  "pr list merged") echo '[{{"number":9,"closingIssuesReferences":[{{"id":"I_x","number":5,"repository":{{"id":"R_x","name":"r","owner":{{"id":"U_x","login":"o"}}}},"url":"https://github.com/o/r/issues/5"}}]}}]' ;;
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
        assert_eq!(found[0].action, Action::Close { by: Some(9) });
        assert!(matches!(found[1].action, Action::Report(_)));
        close(&bin, "o/r", 1, Some(9)).await.unwrap();
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

    fn state_with(agents: &[(&str, Option<&str>, Option<&str>)]) -> crate::state::State {
        // (agent id, cwd, branch of the worktree at /w/<id>)
        use crate::event::WorktreeState;
        use crate::state::{Agent, Usage, Worktree, worktree_key};
        let mut s = crate::state::State::default();
        for (id, cwd, branch) in agents {
            s.agents.insert(
                id.to_string(),
                Agent {
                    host: "h".into(),
                    kind: None,
                    pane: None,
                    state: crate::event::AgentState::Working,
                    since: now(),
                    worked: None,
                    finished: None,
                    usage: Some(Usage {
                        rss_kb: None,
                        cpu_pct: None,
                        procs: None,
                        cwd: cwd.map(String::from),
                        ts: now(),
                    }),
                },
            );
            let path = format!("/w/{id}");
            s.worktrees.insert(
                worktree_key("h", &path),
                Worktree {
                    state: WorktreeState {
                        host: "h".into(),
                        path,
                        repo: "/r".into(),
                        branch: branch.map(String::from),
                        head: "abc".into(),
                        unpushed: 0,
                        dirty: 0,
                        dirty_submodules: 0,
                        files: vec![],
                        last_commit: now(),
                        in_main: false,
                        herdr: false,
                    },
                    since: now(),
                },
            );
        }
        s
    }

    #[test]
    fn a_live_agents_worktree_branch_is_owned() {
        let s = state_with(&[("a", Some("/w/a/src"), Some("s-1"))]);
        let (set, unknown) = live_branches(&s);
        assert_eq!((set.contains("s-1"), unknown), (true, false));
    }

    #[test]
    fn an_agent_with_no_nameable_branch_makes_the_set_unknown() {
        for s in [
            state_with(&[("a", None, Some("s-1"))]),
            state_with(&[("a", Some("/w/a"), None)]),
            state_with(&[("a", Some("/elsewhere"), Some("s-1"))]),
        ] {
            assert!(live_branches(&s).1);
        }
    }

    #[tokio::test]
    async fn with_an_unknown_agent_branch_nothing_is_closed_and_a_live_branch_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(dir.path());
        let cfg = Config {
            repos: vec!["o/r".into()],
            ..Config::default()
        };
        let live = state_with(&[("a", Some("/w/a"), Some("s-1"))]);
        let (found, _) = scan(&bin, &live, &cfg, "2026-11-16T12:00:00Z".parse().unwrap()).await;
        assert_eq!(found.len(), 1, "s-1 is owned: {found:?}");
        let blind = state_with(&[("a", None, None)]);
        let (found, _) = scan(&bin, &blind, &cfg, "2026-11-16T12:00:00Z".parse().unwrap()).await;
        assert_eq!(found.len(), 2);
        assert!(
            found.iter().all(|o| matches!(o.action, Action::Report(_))),
            "{found:?}"
        );
    }

    #[test]
    fn guarded_matches_prefix_name_and_exact_path_only() {
        let g = |f: &str, p: &str| guarded(&[f.to_string()], &[p.to_string()]);
        assert!(g("docs/a.md", "docs/"));
        assert!(!g("mydocs/a.md", "docs/"));
        assert!(g("src/AGENTS.md", "AGENTS.md"));
        assert!(!g("src/NOTAGENTS.md", "AGENTS.md"));
        assert!(g("src/x.rs", "src/x.rs"));
        assert!(!g("src/y.rs", "src/x.rs"));
    }

    #[test]
    fn the_age_boundary_and_a_capped_file_list() {
        assert_eq!(run(&snap(vec![pr(1, "s-1", 3, &[6])]), &[]).len(), 1);
        let mut big = pr(2, "s-2", 4, &[5]);
        big.files = (0..FILES_CAP).map(|i| format!("f{i}")).collect();
        assert!(matches!(
            run(&snap(vec![big]), &[])[0].action,
            Action::Report(_)
        ));
        let none = pr(3, "s-3", 4, &[]);
        assert!(
            run(&snap(vec![none]), &[])[0]
                .detail
                .contains("closes no issue")
        );
    }

    #[test]
    fn the_coordinator_is_told_once_until_it_answers() {
        use crate::state::Msg;
        let mut s = crate::state::State::default();
        assert_eq!(root_of(&s), None);
        s.roles.insert(
            "agent:h/s/root".into(),
            crate::state::Role {
                level: crate::event::Level::RootCoordinator,
                lead: None,
                domain: None,
                by: "x".into(),
                since: now(),
            },
        );
        assert_eq!(root_of(&s).as_deref(), Some("agent:h/s/root"));
        assert!(!already_told(&s, "agent:h/s/root", "o/r#1"));
        s.msgs.insert(
            "m1".into(),
            Msg {
                from: "x".into(),
                to: "agent:h/s/root".into(),
                kind: "orphan-pr".into(),
                reference: "o/r#1".into(),
                text: "t".into(),
                since: now(),
            },
        );
        assert!(already_told(&s, "agent:h/s/root", "o/r#1"));
        assert!(!already_told(&s, "agent:h/s/root", "o/r#2"));
    }

    #[tokio::test]
    async fn a_malformed_pr_row_or_a_failed_close_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("gh");
        std::fs::write(
            &bad,
            "#!/bin/sh\ncase \"$6\" in open) echo '[{\"number\":1}]' ;; *) echo '[]' ;; esac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = fetch(bad.to_str().unwrap(), "o/r").await.unwrap_err();
        assert!(format!("{e:#}").contains("unreadable PR"), "{e:#}");
        assert!(close("/nonexistent/gh", "o/r", 1, Some(9)).await.is_err());
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
