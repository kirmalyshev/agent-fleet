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
    /// `owner/repo` list to look at; absent = the repos connected on this machine
    /// (`agent-fleet repo add`), filled in by `config_in`; empty = the check is off.
    #[serde(default)]
    pub repos: Option<Vec<String>>,
    /// Quiet this many days before a PR is an orphan.
    #[serde(default = "default_days")]
    pub days: u32,
    /// A PR touching one of these waits for a person. `dir/` is a prefix, anything else a file
    /// name (at any depth) or exact path.
    #[serde(default = "default_guarded")]
    pub guarded: Vec<String>,
    /// The host's watch loop runs the `doctor --apply` step itself (close superseded PRs, tell
    /// the root coordinator of the rest). Off unless set.
    #[serde(default)]
    pub apply: bool,
}

fn default_days() -> u32 {
    3
}

fn default_guarded() -> Vec<String> {
    ["AGENTS.md", "CLAUDE.md", "README.md", "docs/", "schema/"]
        .map(String::from)
        .into()
}

impl Config {
    /// The repos to look at; none when unset and not yet filled in.
    pub fn repos(&self) -> &[String] {
        self.repos.as_deref().unwrap_or_default()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos: None,
            days: default_days(),
            guarded: default_guarded(),
            apply: false,
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
        .kill_on_drop(true)
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
    for repo in cfg.repos() {
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
pub fn config(dir: &std::path::Path, instance: &str) -> Result<Option<Config>> {
    config_in(dir, crate::spawn::machine_dir(instance).as_deref())
}

/// `config` with the machine's config dir given. `repos` unset means the machine's connected
/// repos, in either file.
pub fn config_in(
    dir: &std::path::Path,
    machine: Option<&std::path::Path>,
) -> Result<Option<Config>> {
    let top = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    match crate::spawn::load_in(top.as_deref().unwrap_or(dir), machine) {
        Ok((mut c, _)) => {
            if c.orphan_pr.repos.is_none() {
                let connected = match machine {
                    Some(m) => crate::setup::registered(m)?,
                    None => BTreeMap::new(),
                };
                c.orphan_pr.repos = Some(connected.into_keys().collect());
            }
            Ok(Some(c.orphan_pr))
        }
        Err(e) if missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn missing(e: &anyhow::Error) -> bool {
    e.root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// The `--apply` outcome of one orphan, as a finding-detail suffix: a `Close` runs `gh pr close`,
/// a report adds nothing.
pub async fn close_note(bin: &str, o: &Orphan) -> String {
    match &o.action {
        Action::Close { by } => match close(bin, &o.repo, o.number, *by).await {
            Ok(()) => "; closed, branch kept".into(),
            Err(e) => format!("; close failed: {e:#}"),
        },
        Action::Report(_) => String::new(),
    }
}

/// What the watch loop does with the orphans of one scan.
#[derive(Debug, PartialEq)]
pub enum Act {
    Close(Orphan),
    /// A `msg.sent` to the root coordinator.
    Tell {
        to: String,
        key: String,
        text: String,
    },
}

/// A report is told again only after this long without a change.
pub const RETELL: Duration = Duration::days(7);

/// Close each `Close`; tell the root coordinator of each `Report` it was not told of within
/// `RETELL` (the log is the dedupe across restarts).
pub fn acts(
    orphans: Vec<Orphan>,
    s: &crate::state::State,
    events: &[crate::event::Event],
    now: DateTime<Utc>,
) -> Vec<Act> {
    let root = root_of(s);
    let mut out = Vec::new();
    for o in orphans {
        if matches!(o.action, Action::Close { .. }) {
            out.push(Act::Close(o));
            continue;
        }
        let key = format!("{}#{}", o.repo, o.number);
        if let Some(to) = root.clone()
            && !already_told(s, &to, &key)
            && !crate::watch::unsent(events, vec![to.clone()], "orphan-pr", &key, now - RETELL)
                .is_empty()
        {
            out.push(Act::Tell {
                text: format!("{key} has no live agent: {}", o.detail),
                to,
                key,
            });
        }
    }
    out
}

/// `owner/repo` of a git remote URL (`git@host:o/r.git`, `https://host/o/r`, `ssh://git@host/o/r`).
pub fn slug_of(url: &str) -> Option<String> {
    let path = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = path.rsplit_once(':').map_or(path, |(h, p)| {
        if p.contains('/') && !h.ends_with("http") && !h.ends_with("https") && !h.ends_with("ssh") {
            p
        } else {
            path
        }
    });
    let mut parts = path.rsplit('/');
    let (r, o) = (parts.next()?, parts.next()?);
    (!r.is_empty() && !o.is_empty() && !o.contains(':')).then(|| format!("{o}/{r}"))
}

/// The note a retire adds to its outcome line when the branch it retired still has an open PR:
/// `(PR number, line)`. `Ok(None)` when it has none; an error when that could not be found out.
pub async fn retire_note(bin: &str, repo: &str, branch: &str) -> Result<Option<(u64, String)>> {
    retire_note_within(bin, repo, branch, RETIRE_TIMEOUT).await
}

async fn retire_note_within(
    bin: &str,
    repo: &str,
    branch: &str,
    limit: std::time::Duration,
) -> Result<Option<(u64, String)>> {
    // The TUI waits on this: a hung `gh` must not freeze it (kill_on_drop ends the child).
    tokio::time::timeout(limit, retire_note_inner(bin, repo, branch))
        .await
        .map_err(|_| anyhow::anyhow!("gh did not answer in {}s", limit.as_secs_f32()))?
}

const RETIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

async fn retire_note_inner(bin: &str, repo: &str, branch: &str) -> Result<Option<(u64, String)>> {
    let url = tokio::process::Command::new("git")
        .args(["-C", repo, "remote", "get-url", "origin"])
        .kill_on_drop(true)
        .output()
        .await
        .context("run git remote get-url")?;
    if !url.status.success() {
        bail!("{repo} has no origin remote");
    }
    let slug =
        slug_of(&String::from_utf8_lossy(&url.stdout)).context("origin is not owner/repo")?;
    let v = gh(
        bin,
        &[
            "pr", "list", "-R", &slug, "--head", branch, "--state", "open", "--json", "number",
        ],
    )
    .await?;
    let Some(n) = v
        .as_array()
        .context("gh pr list: not a list")?
        .first()
        .map(|p| p["number"].as_u64().context("gh pr list: no number"))
        .transpose()?
    else {
        return Ok(None);
    };
    Ok(Some((
        n,
        format!("PR {slug}#{n} is still open: a coordinator item now"),
    )))
}

/// The configs the watch loop acts on: one per spawn checkout (`owner/repo` → path) whose
/// fleet.toml sets `apply`. Read from the checkouts, never from the process's directory: a
/// service manager starts the host in `/`. A checkout whose fleet.toml cannot be read is an error.
pub fn watch_configs(
    checkouts: &BTreeMap<String, std::path::PathBuf>,
    machine: Option<&std::path::Path>,
) -> Result<Vec<Config>> {
    let mut out = Vec::new();
    for (repo, dir) in checkouts {
        let c = config_in(dir, machine).with_context(|| format!("{repo}: fleet.toml"))?;
        out.extend(watch_enabled(c));
    }
    Ok(out)
}

/// The config the watch loop acts on: only a fleet.toml that sets `apply` and names repos.
pub fn watch_enabled(cfg: Option<Config>) -> Option<Config> {
    cfg.filter(|c| c.apply && !c.repos().is_empty())
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
        let dir = crate::tmpdir::plain_tempdir();
        let c = dir.path().join(".agent-fleet");
        std::fs::create_dir(&c).unwrap();
        std::fs::write(
            c.join("fleet.toml"),
            "brief = \"b.md\"\n[orphan_pr]\ndayz = 1\n",
        )
        .unwrap();
        assert!(config_in(dir.path(), None).is_err());
        std::fs::write(
            c.join("fleet.toml"),
            "brief = \"b.md\"\n[profiles.p]\nkind = \"claude\"\n[[rules]]\nprofile = \"p\"\n[orphan_pr]\nrepos = [\"o/r\"]\n",
        )
        .unwrap();
        assert_eq!(
            config_in(dir.path(), None).unwrap().unwrap().repos(),
            ["o/r"]
        );
    }

    #[test]
    fn apply_is_off_unless_set_and_slugs_come_from_any_remote_form() {
        assert!(!Config::default().apply);
        let c: Config = toml::from_str("repos = [\"o/r\"]\napply = true").unwrap();
        assert!(c.apply);
        for u in [
            "git@host.example:o/r.git\n",
            "https://host.example/o/r",
            "https://host.example/o/r.git/",
            "ssh://git@host.example/o/r.git",
        ] {
            assert_eq!(slug_of(u).as_deref(), Some("o/r"), "{u}");
        }
        assert_eq!(slug_of("r"), None);
    }

    #[test]
    fn the_watch_reads_each_checkouts_fleet_toml_not_the_working_directory() {
        let dir = crate::tmpdir::plain_tempdir();
        let mk = |name: &str, apply: bool| {
            let c = dir.path().join(name).join(".agent-fleet");
            std::fs::create_dir_all(&c).unwrap();
            std::fs::write(
                c.join("fleet.toml"),
                format!(
                    "brief = \"b.md\"\n[profiles.p]\nkind = \"claude\"\n[[rules]]\nprofile = \"p\"\n[orphan_pr]\nrepos = [\"o/{name}\"]\napply = {apply}\n"
                ),
            )
            .unwrap();
            dir.path().join(name)
        };
        let checkouts = BTreeMap::from([
            ("o/on".to_string(), mk("on", true)),
            ("o/off".to_string(), mk("off", false)),
            ("o/none".to_string(), dir.path().join("none")),
        ]);
        let got = watch_configs(&checkouts, None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].repos(), ["o/on"]);
        let broken = BTreeMap::from([("o/b".to_string(), {
            let d = mk("b", true);
            std::fs::write(d.join(".agent-fleet/fleet.toml"), "nope = 1").unwrap();
            d
        })]);
        assert!(watch_configs(&broken, None).is_err());
    }

    #[tokio::test]
    async fn a_hung_gh_gives_up_instead_of_freezing_the_retire() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
            && std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["remote", "add", "origin", "https://host.example/o/r"])
                .status()
                .unwrap()
                .success();
        assert!(ok);
        let bin = dir.path().join("gh");
        crate::write_script(&bin, "#!/bin/sh\nexec sleep 600\n");
        let e = retire_note_within(
            bin.to_str().unwrap(),
            repo.to_str().unwrap(),
            "s-1",
            std::time::Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(format!("{e:#}").contains("did not answer"), "{e:#}");
    }

    #[test]
    fn the_watch_acts_only_on_apply_with_repos() {
        let on = || Config {
            repos: Some(vec!["o/r".into()]),
            apply: true,
            ..Config::default()
        };
        assert!(watch_enabled(None).is_none());
        assert!(watch_enabled(Some(Config::default())).is_none());
        assert!(
            watch_enabled(Some(Config {
                apply: false,
                ..on()
            }))
            .is_none()
        );
        assert!(
            watch_enabled(Some(Config {
                repos: Some(vec![]),
                ..on()
            }))
            .is_none()
        );
        assert!(watch_enabled(Some(on())).is_some());
    }

    #[test]
    fn a_pending_tell_blocks_a_re_tell_even_past_a_week() {
        let mut s = with_root();
        s.msgs.insert(
            "m".into(),
            crate::state::Msg {
                from: "x".into(),
                to: "agent:h/s/root".into(),
                kind: "orphan-pr".into(),
                reference: "o/r#2".into(),
                text: "t".into(),
                since: now(),
            },
        );
        let old = [told("o/r#2", now() - Duration::days(9))];
        assert!(
            acts(
                vec![orphan(2, Action::Report("a draft waits for a person"))],
                &s,
                &old,
                now()
            )
            .is_empty()
        );
    }

    fn orphan(n: u64, action: Action) -> Orphan {
        Orphan {
            repo: "o/r".into(),
            number: n,
            age: Duration::days(5),
            detail: "s: 5d quiet".into(),
            action,
        }
    }

    fn with_root() -> crate::state::State {
        let mut s = crate::state::State::default();
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
        s
    }

    fn told(key: &str, at: DateTime<Utc>) -> crate::event::Event {
        use crate::event::*;
        let mut e = Envelope::new(
            Actor {
                kind: ActorKind::Host,
                id: "host:h".into(),
            },
            "p",
            Known::MsgSent(MsgSent {
                to: "agent:h/s/root".into(),
                kind: "orphan-pr".into(),
                reference: key.into(),
                text: "t".into(),
                closes: Vec::new(),
            }),
        );
        e.ts = at;
        e.into_event()
    }

    #[test]
    fn the_watch_closes_what_is_superseded_and_tells_the_root_once_a_week() {
        let s = with_root();
        let mk = || {
            vec![
                orphan(1, Action::Close { by: Some(9) }),
                orphan(2, Action::Report("a draft waits for a person")),
            ]
        };
        let a = acts(mk(), &s, &[], now());
        assert!(matches!(&a[0], Act::Close(o) if o.number == 1));
        assert!(matches!(&a[1], Act::Tell { key, .. } if key == "o/r#2"));
        // Told two days ago: quiet. Told eight days ago: again.
        let two = [told("o/r#2", now() - Duration::days(2))];
        assert_eq!(acts(mk(), &s, &two, now()).len(), 1);
        let eight = [told("o/r#2", now() - Duration::days(8))];
        assert_eq!(acts(mk(), &s, &eight, now()).len(), 2);
        // No root coordinator: nobody to tell, the close still runs.
        let none = acts(mk(), &crate::state::State::default(), &[], now());
        assert_eq!(none.len(), 1);
    }

    #[tokio::test]
    async fn a_retired_branch_with_an_open_pr_gets_a_note_and_one_without_gets_none() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "git@host.example:o/r.git"]);
        let bin = dir.path().join("gh");
        crate::write_script(
            &bin,
            "#!/bin/sh\ncase \"$*\" in\n  *\"-R o/r --head s-1 \"*) echo '[{\"number\":7}]' ;;\n  *) echo '[]' ;;\nesac\n",
        );
        let (gh, repo) = (bin.to_str().unwrap(), repo.to_str().unwrap());
        let (n, line) = retire_note(gh, repo, "s-1").await.unwrap().unwrap();
        assert_eq!(n, 7);
        assert!(line.contains("o/r#7 is still open"), "{line}");
        assert!(retire_note(gh, repo, "s-2").await.unwrap().is_none());
        assert!(retire_note("/nonexistent/gh", repo, "s-1").await.is_err());
        assert!(retire_note(gh, "/nonexistent", "s-1").await.is_err());
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
        crate::write_script(
            &path,
            &format!(
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
        );
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn scan_reads_gh_and_close_keeps_the_branch() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(dir.path());
        let cfg = Config {
            repos: Some(vec!["o/r".into()]),
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
            repos: Some(vec!["o/r".into()]),
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
            repos: Some(vec!["o/r".into()]),
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
        crate::write_script(
            &bad,
            "#!/bin/sh\ncase \"$6\" in open) echo '[{\"number\":1}]' ;; *) echo '[]' ;; esac\n",
        );
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
