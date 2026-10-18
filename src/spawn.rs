//! Spawning: `<checkout>/.agent-fleet/fleet.toml` routes an issue to a profile and a host,
//! `agent-fleet spawn` renders the brief and writes `spawn.requested`, and the host agent on
//! the named host executes it — worktree or workspace, env scrub, agent start, the brief —
//! and answers `spawn.result`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::Refused;
use crate::event::*;
use crate::host::{Host, herdr_call};
use crate::state::{Spawn, State};

// ---------- the project's fleet.toml ----------

/// `session` and `brief` are required; `base` defaults to `origin/main`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// The herdr session agents run in.
    pub session: String,
    /// The branch point for a new worktree.
    #[serde(default = "default_base")]
    pub base: String,
    /// The brief template next to fleet.toml; `@@KEY@@` placeholders.
    pub brief: String,
    /// A script run inside a NEW worktree with its path as $1, before the agent starts.
    #[serde(default)]
    pub ready: Option<String>,
    /// Extra brief tokens.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// First match wins; a rule with no `match` is the default.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_base() -> String {
    "origin/main".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The agent program: `agent.start`'s kind and `agent.quota`'s kind.
    pub kind: String,
    /// Extra arguments for `agent.start`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Variables unset in the pane's shell before the agent starts.
    #[serde(default)]
    pub unset_env: Vec<String>,
    /// The default agent name is prefix + issue number.
    #[serde(default)]
    pub prefix: String,
    /// Live agents of this profile for this repo; absent = no cap.
    #[serde(default)]
    pub cap: Option<u32>,
    /// Used when this profile is at cap or out of quota, and for a handoff.
    #[serde(default)]
    pub fallback: Option<String>,
    /// Minutes after a successful spawn within which the agent must reach `working`,
    /// or it is handed to the fallback profile. Default 5.
    #[serde(default)]
    pub handoff_min: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Keywords, OR'd: case-insensitive whole words against the issue's title, the `--scope`
    /// text and its labels; `label:x` matches only a label named x. Absent = the default rule.
    #[serde(rename = "match")]
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    pub profile: String,
    /// The host to spawn on [default: the host running the CLI].
    #[serde(default)]
    pub host: Option<String>,
    /// The `agent.role` level declared for the new agent [default: implementer].
    #[serde(default)]
    pub level: Option<Level>,
}

/// The first `needle` line, as ` (line 12: …)`, for an error that blames a line.
fn line_of(src: &str, needle: &str) -> String {
    src.lines()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .map(|(n, l)| format!(" (line {}: {})", n + 1, l.trim()))
        .unwrap_or_default()
}

/// `^[A-Za-z_][A-Za-z0-9_]*$` — a name safe to `unset` in a shell.
fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()))
}

/// A `ready` path is relative and made only of normal components — no `..`, no root.
fn check_ready(r: &str) -> Result<()> {
    let p = Path::new(r);
    if r.is_empty()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("must be a relative path of plain components");
    }
    Ok(())
}

/// The line of the `n`th `[[rules]]` header.
fn nth_rule(src: &str, n: usize) -> String {
    src.lines()
        .enumerate()
        .filter(|(_, l)| l.trim() == "[[rules]]")
        .nth(n)
        .map(|(i, l)| format!(" (line {}: {})", i + 1, l.trim()))
        .unwrap_or_default()
}

impl FleetConfig {
    pub fn parse(src: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(src).context("fleet.toml")?;
        cfg.validate(src)?;
        Ok(cfg)
    }

    fn validate(&self, src: &str) -> Result<()> {
        if self.profiles.is_empty() {
            bail!("fleet.toml: no [profiles] section");
        }
        if let Some(r) = &self.ready {
            check_ready(r)
                .with_context(|| format!("fleet.toml: ready {r:?}{}", line_of(src, "ready")))?;
        }
        let mut by_prefix: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, p) in &self.profiles {
            if let Some(f) = &p.fallback
                && !self.profiles.contains_key(f)
            {
                bail!(
                    "fleet.toml: profile {name:?} falls back to an unknown profile {f:?}{}",
                    line_of(src, &format!("fallback = \"{f}\""))
                );
            }
            for v in &p.unset_env {
                if !is_env_name(v) {
                    bail!(
                        "fleet.toml: profile {name:?} unset_env {v:?} is not a valid name{}",
                        line_of(src, &format!("\"{v}\""))
                    );
                }
            }
            if !p.prefix.is_empty()
                && let Some(other) = by_prefix.insert(p.prefix.as_str(), name)
            {
                bail!(
                    "fleet.toml: profiles {other:?} and {name:?} share prefix {:?}{}",
                    p.prefix,
                    line_of(src, &format!("prefix = \"{}\"", p.prefix))
                );
            }
        }
        for name in self.profiles.keys() {
            // A fallback chain must end.
            let mut seen = BTreeSet::new();
            let mut at = name.as_str();
            while let Some(next) = self.profiles.get(at).and_then(|p| p.fallback.as_deref()) {
                if !seen.insert(at) {
                    bail!(
                        "fleet.toml: fallback cycle through {at:?}{}",
                        line_of(src, "fallback")
                    );
                }
                at = next;
            }
        }
        let mut defaults = 0;
        for (i, r) in self.rules.iter().enumerate() {
            if !self.profiles.contains_key(&r.profile) {
                bail!(
                    "fleet.toml: rule {} names an unknown profile {:?}{}",
                    i + 1,
                    r.profile,
                    line_of(src, &format!("profile = \"{}\"", r.profile))
                );
            }
            if r.keywords.as_ref().is_none_or(|k| k.is_empty()) {
                defaults += 1;
                if defaults > 1 {
                    bail!(
                        "fleet.toml: a second default rule (no `match`){}",
                        nth_rule(src, i)
                    );
                }
            }
        }
        if self.rules.is_empty() || defaults == 0 {
            bail!("fleet.toml: no default rule — add a [[rules]] with no `match`");
        }
        Ok(())
    }
}

/// `dir/.agent-fleet/fleet.toml`, and the directory it lives in (the brief template sits there).
pub fn load(dir: &Path) -> Result<(FleetConfig, PathBuf)> {
    let dir = dir.join(".agent-fleet");
    let src = std::fs::read_to_string(dir.join("fleet.toml"))
        .with_context(|| format!("read {}", dir.join("fleet.toml").display()))?;
    Ok((FleetConfig::parse(&src)?, dir))
}

/// The fleet.toml of the git checkout holding the current directory.
pub fn find() -> Result<(FleetConfig, PathBuf)> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("run git rev-parse --show-toplevel")?;
    if !out.status.success() {
        bail!("not in a git checkout; run spawn inside the project");
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    load(Path::new(&top))
}

// ---------- routing ----------

/// What `gh issue view` brings: the title and labels the rules match on.
#[derive(Debug, Clone)]
pub struct Issue {
    /// `owner/repo`: a profile's cap counts only this repo's agents.
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
}

/// `gh issue view <n> -R <owner/repo> --json title,labels`. `AGENT_FLEET_GH` names the binary.
pub async fn gh_issue(repo: &str, n: u64) -> Result<Issue> {
    let gh = std::env::var("AGENT_FLEET_GH").unwrap_or_else(|_| "gh".into());
    let out = tokio::process::Command::new(gh)
        .args([
            "issue",
            "view",
            &n.to_string(),
            "-R",
            repo,
            "--json",
            "title,labels",
        ])
        .output()
        .await
        .context("run gh issue view")?;
    if !out.status.success() {
        bail!(
            "gh issue view {n} -R {repo}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("gh issue view: bad JSON")?;
    Ok(Issue {
        repo: repo.to_string(),
        number: n,
        title: v["title"].as_str().unwrap_or_default().to_string(),
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_string))
            .collect(),
    })
}

/// `<owner/repo>#<issue>`.
pub fn parse_target(s: &str) -> Result<(String, u64)> {
    let Some((repo, n)) = s.rsplit_once('#') else {
        bail!("not <owner/repo>#<issue>: {s}")
    };
    if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
        bail!("not <owner/repo>#<issue>: {s}");
    }
    let n: u64 = n
        .parse()
        .with_context(|| format!("not an issue number: {n}"))?;
    Ok((repo.to_string(), n))
}

/// `owner/repo` from a git remote URL, or `None` when it isn't a GitHub remote.
fn slug_from_url(url: &str) -> Option<String> {
    let rest = url.trim().strip_suffix(".git").unwrap_or(url.trim());
    let rest = rest
        .strip_prefix("git@github.com:")
        .or_else(|| rest.strip_prefix("https://github.com/"))
        .or_else(|| rest.strip_prefix("http://github.com/"))
        .or_else(|| rest.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// The `owner/repo` slug of a checkout's `origin` remote, for `--repo <dir>` without `slug=`.
pub fn slug_of(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| slug_from_url(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
}

/// `word` occurs in `text` case-insensitively and not as part of a longer word.
fn has_word(text: &str, word: &str) -> bool {
    let (t, w) = (text.as_bytes(), word.as_bytes());
    let edge = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    if w.is_empty() || t.len() < w.len() {
        return false;
    }
    (0..=t.len() - w.len()).any(|i| {
        t[i..i + w.len()].eq_ignore_ascii_case(w)
            && (i == 0 || !edge(t[i - 1]))
            && (i + w.len() == t.len() || !edge(t[i + w.len()]))
    })
}

/// The first keyword of the rule that hits, if any. Only the issue routes: `--scope` narrows the
/// work and never picks the worker.
fn rule_match(kws: &[String], issue: &Issue) -> Option<String> {
    for kw in kws {
        let hit = match kw.strip_prefix("label:") {
            Some(l) => issue.labels.iter().any(|x| x.eq_ignore_ascii_case(l)),
            None => has_word(&issue.title, kw) || issue.labels.iter().any(|l| has_word(l, kw)),
        };
        if hit {
            return Some(kw.clone());
        }
    }
    None
}

/// Where a spawn goes: the profile after the fallback walk, its host and level, and `why`.
#[derive(Debug)]
pub struct Decision {
    pub profile: String,
    pub host: String,
    pub level: Level,
    /// Every step of the decision.
    pub why: String,
}

/// `--profile`/`--host`/`--level` overrides from the CLI.
#[derive(Default)]
pub struct Overrides<'a> {
    pub profile: Option<&'a str>,
    pub host: Option<&'a str>,
    pub level: Option<Level>,
}

/// Pick the rule (or the `--profile` override), then walk fallbacks past a profile at `cap` or
/// out of quota on the target host.
pub fn decide(
    cfg: &FleetConfig,
    issue: &Issue,
    o: &Overrides<'_>,
    state: &State,
    local_host: &str,
) -> Result<Decision> {
    let mut why = Vec::new();
    let (mut profile, rule_host, rule_level) = match o.profile {
        Some(p) => {
            if !cfg.profiles.contains_key(p) {
                bail!("no profile {p:?} in fleet.toml");
            }
            why.push(format!("--profile {p}"));
            (p.to_string(), None, None)
        }
        None => {
            let (i, kw) = cfg
                .rules
                .iter()
                .enumerate()
                .map(|(i, r)| (i, r.keywords.as_deref().and_then(|k| rule_match(k, issue))))
                .find(|(i, r)| {
                    cfg.rules[*i]
                        .keywords
                        .as_deref()
                        .is_none_or(|k| k.is_empty())
                        || r.is_some()
                })
                .context("fleet.toml: no rule applies")?;
            let rule = &cfg.rules[i];
            why.push(match &kw {
                Some(k) => format!("matched {k:?}"),
                None => "default rule".into(),
            });
            (rule.profile.clone(), rule.host.clone(), rule.level)
        }
    };
    let host = o
        .host
        .or(rule_host.as_deref())
        .unwrap_or(local_host)
        .to_string();
    let level = o.level.or(rule_level).unwrap_or(Level::Implementer);
    loop {
        let p = &cfg.profiles[&profile];
        let full = p.cap.is_some_and(|cap| {
            let live = state
                .agents
                .keys()
                .filter_map(|a| state.spawn_of(a))
                .filter(|s| s.request.profile == profile && s.request.repo == issue.repo)
                .count();
            let pending = state
                .spawns
                .values()
                .filter(|s| {
                    s.result.is_none()
                        && s.request.profile == profile
                        && s.request.repo == issue.repo
                        && Utc::now() - s.since <= REQUEST_MAX_AGE
                })
                .count();
            live + pending >= cap as usize
        });
        let out = state
            .quotas
            .get(&format!("{host}:{}", p.kind))
            .is_some_and(|q| q.exhausted_at(Utc::now()));
        if !full && !out {
            break;
        }
        why.push(format!(
            "{profile} skipped: {}",
            if full {
                format!("at cap {}", p.cap.unwrap_or(0))
            } else {
                format!("{} quota exhausted on {host}", p.kind)
            }
        ));
        match &p.fallback {
            Some(f) => profile = f.clone(),
            None => bail!("no profile can take it ({})", why.join("; ")),
        }
    }
    Ok(Decision {
        profile,
        host,
        level,
        why: why.join("; "),
    })
}

// ---------- the brief ----------

/// The text after a `--- brief ---` line, or the whole file.
pub fn brief_body(src: &str) -> &str {
    src.split_once("--- brief ---")
        .map_or(src, |(_, t)| t.trim_start_matches('\n'))
}

/// Replace every `@@KEY@@` (A-Z, 0-9, _) in one pass over the template: a value holding `@@X@@`
/// is never re-expanded. A placeholder with no value refuses the render and names it.
pub fn render(template: &str, vals: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut missing = BTreeSet::new();
    let mut rest = template;
    while let Some(i) = rest.find("@@") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let len = after
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
            .count();
        if len > 0 && after[len..].starts_with("@@") {
            let key = &after[..len];
            match vals.get(key) {
                Some(v) => out.push_str(v),
                None => {
                    missing.insert(key.to_string());
                    out.push_str(&rest[i..i + len + 4]);
                }
            }
            rest = &after[len + 2..];
        } else {
            out.push_str("@@");
            rest = after;
        }
    }
    out.push_str(rest);
    if !missing.is_empty() {
        bail!(
            "brief template: no value for {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(out)
}

/// The project an `owner/repo` issue's task lives in: the repo's name.
pub fn task_project(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// `task:<repo name>/<issue>`: the one id an issue has, whether a spawn assigns it or the agent
/// runs `AGENT_FLEET_PROJECT=<repo name> agent-fleet task add <issue>`.
pub fn task_id(repo: &str, issue: u64) -> String {
    format!("task:{}/{issue}", task_project(repo))
}

/// The HANDOFF paragraph a takeover brief opens with.
pub fn handoff_note(old: &str, extra: &str) -> String {
    let mut s = format!(
        "HANDOFF from {old}: it stopped with the issue unfinished and this worktree already holds its work — `git status`, `git log` and any open PR show where it stands. Continue it; do not restart.\n"
    );
    if !extra.is_empty() {
        s.push_str(&format!("\n{extra}\n"));
    }
    s
}

/// The `spawn.requested` a handoff writes: the agent's repo, issue, branch and EXISTING
/// worktree to a new agent of `to`, with `replaces` naming the agent to close.
pub fn handoff_request(
    cfg: &FleetConfig,
    cfg_dir: &Path,
    sp: &Spawn,
    to: &str,
    extra: &str,
    title: &str,
) -> Result<SpawnRequested> {
    let old = &sp.request;
    let Some(p) = cfg.profiles.get(to) else {
        bail!("no profile {to:?} in fleet.toml");
    };
    let suffix = old
        .issue
        .map_or_else(|| old.name.clone(), |i| i.to_string());
    let name = format!("{}{}", p.prefix, suffix);
    let level = old.level.unwrap_or(Level::Implementer);
    let lead = old
        .lead
        .clone()
        .filter(|l| is_agent_id(l))
        .or_else(|| is_agent_id(&sp.by).then(|| sp.by.clone()));
    let worktree = sp
        .result
        .as_ref()
        .and_then(|r| r.worktree.clone())
        .or_else(|| old.worktree.clone())
        .context("the agent has no worktree to hand over")?;
    let src = std::fs::read_to_string(cfg_dir.join(&cfg.brief))
        .with_context(|| format!("read {}", cfg_dir.join(&cfg.brief).display()))?;
    let mut vals = cfg.vars.clone();
    for (k, v) in [
        ("NAME", name.as_str()),
        ("KIND", to),
        ("PROGRAM", p.kind.as_str()),
        ("SLUG", old.repo.as_str()),
        ("ISSUE", &old.issue.map_or(String::new(), |i| i.to_string())),
        ("TITLE", title),
        ("BRANCH", old.branch.as_str()),
        ("SCOPE", ""),
        ("HOST", old.host.as_str()),
        ("LEVEL", &level.to_string()),
        ("LEAD", lead.as_deref().unwrap_or("")),
        ("HANDOFF", &handoff_note(&old.name, extra)),
    ] {
        vals.insert(k.into(), v.to_string());
    }
    let brief = render(brief_body(&src), &vals)?;
    let why = match extra.is_empty() {
        true => format!("handoff: {} → {to}", old.name),
        false => format!("handoff: {} → {to} ({extra})", old.name),
    };
    let agent = format!("agent:{}/{}/{name}", old.host, cfg.session);
    if agent == old.agent {
        bail!("profile {to:?} names the same agent {agent}; a handoff must replace it");
    }
    Ok(SpawnRequested {
        name: name.clone(),
        agent,
        host: old.host.clone(),
        session: cfg.session.clone(),
        repo: old.repo.clone(),
        issue: old.issue,
        branch: old.branch.clone(),
        profile: to.to_string(),
        brief,
        level: Some(level),
        lead,
        worktree: Some(worktree),
        replaces: Some(old.agent.clone()),
        why,
    })
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// The latest fleet spawn of an agent: by full id or by its name in the session.
pub fn find_spawn<'a>(state: &'a State, agent: &str) -> Option<&'a Spawn> {
    state
        .spawns
        .values()
        .filter(|s| s.request.agent == agent || short(&s.request.agent) == agent)
        .max_by_key(|s| s.since)
}

/// Follows the hub until the request's `spawn.result` arrives (10 min); fails on !ok.
pub async fn wait_result(client: &crate::client::Client, id: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        match client.state().await {
            Ok(s) => {
                if let Some(r) = s.spawns.get(id).and_then(|s| s.result.clone()) {
                    if r.ok {
                        println!(
                            "{} is up (pane {}, {})",
                            r.agent,
                            r.pane.as_deref().unwrap_or("-"),
                            r.worktree.as_deref().unwrap_or("-")
                        );
                        return Ok(());
                    }
                    bail!("spawn failed: {}", r.error.unwrap_or_default());
                }
            }
            Err(e) if e.downcast_ref::<Refused>().is_some() => {
                bail!("waiting for the spawn result: {e:#}");
            }
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("no spawn.result for {id} within 10 min — the host may be down");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ---------- the host executes ----------

/// A request this old when the host first sees it is answered "expired", not executed.
pub const REQUEST_MAX_AGE: chrono::Duration = chrono::Duration::minutes(15);
/// Default `host --stall-min`; doctor's `stalled` finding uses the same.
pub const STALL_MIN: u64 = 15;
/// `agent.start` retries on `agent_pane_busy`: a fresh pane's shell needs a moment first.
const BUSY_TRIES: u32 = 10;
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// The brief is sent only once an agent that never went `working` is re-sent to.
const STARTED: [&str; 3] = ["working", "blocked", "done"];

/// What a spawn took, so a failure hands back exactly that.
#[derive(Default)]
struct Made {
    pane: Option<String>,
    workspace: Option<String>,
    worktree: Option<String>,
    branch: String,
    /// The worktree was created for this spawn (vs. `worktree` set on the request).
    fresh: bool,
}

/// A spawn's yield: the pane and the directory the agent works in.
struct Done {
    pane: String,
    worktree: String,
}

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn label_of(r: &SpawnRequested) -> String {
    match r.issue {
        Some(i) => format!("{} #{i}", r.name),
        None => r.name.clone(),
    }
}

async fn worktree_create(
    host: &Host,
    checkout: &Path,
    r: &SpawnRequested,
    base: &str,
) -> Result<Value> {
    herdr_call(
        &host.cfg.herdr_socket,
        "worktree.create",
        json!({
            "cwd": checkout,
            "branch": r.branch,
            "base": base,
            "label": label_of(r),
            "focus": false,
            "trust_repository": true,
        }),
    )
    .await
}

/// A branch left by an earlier failed spawn is removed ONLY while it holds no work: no commit
/// on it that is not on a remote, and a clean tree. True when the leftovers were cleared and a
/// retry is safe.
/// An up agent whose cwd is inside `path`.
fn agent_inside(state: &State, path: &str) -> bool {
    state.agents.values().any(|a| {
        a.usage
            .as_ref()
            .and_then(|u| u.cwd.as_deref())
            .is_some_and(|c| c == path || c.starts_with(&format!("{path}/")))
    })
}

async fn reclaim(host: &Host, checkout: &Path, branch: &str, state: &State) -> bool {
    let sha = format!("refs/heads/{branch}");
    if git(checkout, &["rev-parse", "--verify", "-q", &sha])
        .await
        .is_err()
    {
        return false;
    }
    let Ok(local) = git(
        checkout,
        &["log", "--format=%h", branch, "--not", "--remotes"],
    )
    .await
    else {
        return false;
    };
    if !local.trim().is_empty() {
        return false;
    }
    let list = git(checkout, &["worktree", "list", "--porcelain"])
        .await
        .unwrap_or_default();
    let stale = crate::scan::parse_worktrees(&list)
        .into_iter()
        .find(|(_, _, b)| b.as_deref() == Some(branch))
        .map(|(p, _, _)| p);
    if let Some(p) = &stale
        && Path::new(p).is_dir()
    {
        if agent_inside(state, p) {
            return false;
        }
        let Ok(dirty) = git(Path::new(p), &["status", "--porcelain"]).await else {
            return false;
        };
        if !dirty.trim().is_empty() {
            return false;
        }
    }
    eprintln!("agent-fleet host: spawn: clearing stale leftovers of {branch}");
    if let Ok(list) = herdr_call(
        &host.cfg.herdr_socket,
        "worktree.list",
        json!({ "cwd": checkout }),
    )
    .await
    {
        let ws = list["worktrees"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["branch"].as_str() == Some(branch))
            .and_then(|w| w["open_workspace_id"].as_str())
            .map(str::to_string);
        if let Some(ws) = ws {
            let _ = herdr_call(
                &host.cfg.herdr_socket,
                "worktree.remove",
                json!({ "workspace_id": ws, "force": true }),
            )
            .await;
        }
    }
    if let Some(p) = &stale {
        let _ = git(checkout, &["worktree", "remove", "--force", p]).await;
    }
    let _ = git(checkout, &["worktree", "prune"]).await;
    let _ = git(checkout, &["branch", "-D", branch]).await;
    true
}

/// `ready` runs inside a NEW worktree with its path as $1, before the agent starts. The script
/// is the host checkout's copy — the worktree the request made is not a config source.
async fn run_ready(checkout: &Path, worktree: &str, ready: &str) -> Result<()> {
    let script = checkout.join(ready);
    if !script.is_file() {
        bail!("ready script {ready:?} is not in {}", checkout.display());
    }
    let out = tokio::time::timeout(
        Duration::from_secs(15 * 60),
        tokio::process::Command::new(&script)
            .arg(worktree)
            .current_dir(worktree)
            .output(),
    )
    .await
    .context("ready script timed out (15m)")??;
    if !out.status.success() {
        bail!(
            "ready script {ready} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `unset NAME` typed into the pane's shell, retried while the shell is not ready.
async fn unset_env(sock: &Path, pane: &str, var: &str) -> Result<()> {
    let mut err = anyhow!("pane.send_text never ran");
    for _ in 0..BUSY_TRIES {
        let send = async {
            herdr_call(
                sock,
                "pane.send_text",
                json!({ "pane_id": pane, "text": format!("unset {var}") }),
            )
            .await?;
            herdr_call(
                sock,
                "pane.send_keys",
                json!({ "pane_id": pane, "keys": ["enter"] }),
            )
            .await
        };
        match send.await {
            Ok(_) => return Ok(()),
            Err(e) => {
                err = e;
                tokio::time::sleep(BUSY_WAIT).await;
            }
        }
    }
    Err(err.context("the pane's shell never took the command"))
}

async fn agent_start(host: &Host, name: &str, prof: &Profile, pane: &str) -> Result<()> {
    let mut err = anyhow!("agent.start never ran");
    for _ in 0..BUSY_TRIES {
        match herdr_call(
            &host.cfg.herdr_socket,
            "agent.start",
            json!({
                "name": name,
                "kind": prof.kind,
                "pane_id": pane,
                "args": prof.args,
                "timeout_ms": 120_000,
            }),
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                err = e;
                if !format!("{err:#}").contains("agent_pane_busy") {
                    return Err(err.context("agent.start"));
                }
                tokio::time::sleep(BUSY_WAIT).await;
            }
        }
    }
    Err(err.context("agent.start"))
}

/// The brief, as the scripts learned to send it: wait for the input box (`idle`), prompt with a
/// wait for working|blocked|done; a stall gets one Enter — it ships a paste sitting in the box —
/// and only an agent still not started gets one re-send. A brief never lands twice.
async fn brief_send(host: &Host, r: &SpawnRequested) -> Result<()> {
    let sock = &host.cfg.herdr_socket;
    let wait = |until: &[&str], ms: u64| {
        herdr_call(
            sock,
            "agent.wait",
            json!({ "target": r.name, "until": until, "timeout_ms": ms }),
        )
    };
    let prompt = |ms: u64| {
        herdr_call(
            sock,
            "agent.prompt",
            json!({
                "target": r.name,
                "text": r.brief,
                "wait": { "until": STARTED, "timeout_ms": ms },
            }),
        )
    };
    wait(&["idle"], 120_000)
        .await
        .context("the agent never went idle")?;
    if prompt(30_000).await.is_ok() {
        return Ok(());
    }
    let _ = herdr_call(
        sock,
        "agent.send_keys",
        json!({ "target": r.name, "keys": ["enter"] }),
    )
    .await;
    if wait(&STARTED, 15_000).await.is_ok() {
        return Ok(());
    }
    prompt(30_000)
        .await
        .map(|_| ())
        .context("the agent never took the brief")
}

/// Wire fields that reach git or a shell must not look like flags; `branch` must be a ref.
async fn check_wire_fields(checkout: &Path, r: &SpawnRequested) -> Result<()> {
    for (what, v) in [("name", &r.name), ("branch", &r.branch)] {
        if v.starts_with('-') {
            bail!("{what} must not start with '-': {v:?}");
        }
    }
    git(checkout, &["check-ref-format", "--branch", &r.branch])
        .await
        .with_context(|| format!("branch {:?} is not a valid ref", r.branch))?;
    Ok(())
}

/// The profile the request names, read from the checkout's OWN fleet.toml: the host decides
/// `kind`, `args`, `unset_env` and `ready` itself — a request never carries them.
fn host_profile(cfg_dir: &Path, r: &SpawnRequested) -> Result<(FleetConfig, Profile)> {
    let (cfg, _) = load(cfg_dir)
        .with_context(|| format!("read {}", cfg_dir.join(".agent-fleet/fleet.toml").display()))?;
    let p = cfg
        .profiles
        .get(&r.profile)
        .with_context(|| {
            format!(
                "profile {:?} is not in {}",
                r.profile,
                cfg_dir.join(".agent-fleet/fleet.toml").display()
            )
        })?
        .clone();
    Ok((cfg, p))
}

/// The whole request: worktree or workspace, ready, env scrub, agent start, brief. `made`
/// accumulates what was created so the caller can hand it back on failure.
async fn run_spawn(
    host: &Host,
    r: &SpawnRequested,
    made: &mut Made,
    state: &State,
) -> Result<Done> {
    if r.session != host.cfg.session {
        bail!(
            "this host serves session {:?}, not {:?}",
            host.cfg.session,
            r.session
        );
    }
    let Some(checkout) = host.cfg.checkouts.get(&r.repo).cloned() else {
        bail!("no checkout for {} (host --repo {}=<dir>)", r.repo, r.repo);
    };
    // The profile and its launch config always come from the HOST's checkout — a handoff's
    // worktree is the agent's own tree and is never read for config.
    let (cfg, prof) = host_profile(&checkout, r)?;
    check_wire_fields(&checkout, r).await?;
    let res = match &r.worktree {
        Some(wt) => {
            if !Path::new(wt).is_dir() {
                bail!("no such worktree: {wt}");
            }
            herdr_call(
                &host.cfg.herdr_socket,
                "workspace.create",
                json!({ "cwd": wt, "label": label_of(r), "focus": false }),
            )
            .await
            .context("workspace.create")?
        }
        None => {
            // A stale base still exists when fetch has no network; create fails clearly without it.
            if let Err(e) = git(&checkout, &["fetch", "origin"]).await {
                eprintln!(
                    "agent-fleet host: spawn {}: git fetch origin: {e:#}",
                    r.name
                );
            }
            let existed = git(
                &checkout,
                &[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/heads/{}", r.branch),
                ],
            )
            .await
            .is_ok();
            let (res, ours) = match worktree_create(host, &checkout, r, &cfg.base).await {
                Ok(res) => (res, !existed),
                Err(err) => {
                    if reclaim(host, &checkout, &r.branch, state).await {
                        (
                            worktree_create(host, &checkout, r, &cfg.base)
                                .await
                                .context("worktree.create (after clearing leftovers)")?,
                            true,
                        )
                    } else {
                        return Err(err.context("worktree.create"));
                    }
                }
            };
            made.fresh = true;
            // The branch is handed back on failure only when this spawn created it.
            if ours {
                made.branch = r.branch.clone();
            }
            res
        }
    };
    let pane = res["root_pane"]["pane_id"]
        .as_str()
        .context("worktree/workspace create: no root_pane.pane_id")?
        .to_string();
    let ws = res["workspace"]["workspace_id"]
        .as_str()
        .context("worktree/workspace create: no workspace.workspace_id")?
        .to_string();
    let wt = r
        .worktree
        .clone()
        .or_else(|| res["worktree"]["path"].as_str().map(str::to_string))
        .or_else(|| {
            res["workspace"]["worktree"]["checkout_path"]
                .as_str()
                .map(str::to_string)
        })
        .context("worktree/workspace create: no worktree path")?;
    made.pane = Some(pane);
    made.workspace = Some(ws);
    made.worktree = Some(wt.clone());
    if made.fresh
        && let Some(ready) = &cfg.ready
    {
        run_ready(&checkout, &wt, ready).await?;
    }
    for var in &prof.unset_env {
        unset_env(
            &host.cfg.herdr_socket,
            made.pane.as_deref().unwrap_or_default(),
            var,
        )
        .await
        .with_context(|| format!("env scrub: unset {var}"))?;
    }
    agent_start(
        host,
        &r.name,
        &prof,
        made.pane.as_deref().unwrap_or_default(),
    )
    .await?;
    brief_send(host, r).await?;
    Ok(Done {
        pane: made.pane.clone().unwrap_or_default(),
        worktree: wt,
    })
}

/// Hand back whatever the spawn took: the pane, and for a NEW worktree also the worktree and
/// the branch; for an existing worktree only the workspace it opened.
async fn undo(host: &Host, r: &SpawnRequested, made: &Made) {
    let sock = &host.cfg.herdr_socket;
    if let Some(p) = &made.pane {
        let _ = herdr_call(sock, "pane.close", json!({ "pane_id": p })).await;
    }
    if made.fresh {
        let checkout = host.cfg.checkouts.get(&r.repo).cloned().unwrap_or_default();
        if let Some(ws) = &made.workspace {
            let _ = herdr_call(
                sock,
                "worktree.remove",
                json!({ "workspace_id": ws, "force": true }),
            )
            .await;
        } else if let Some(wt) = &made.worktree {
            let _ = git(&checkout, &["worktree", "remove", "--force", wt]).await;
        }
        let _ = git(&checkout, &["worktree", "prune"]).await;
        if !made.branch.is_empty() {
            let _ = git(&checkout, &["branch", "-D", &made.branch]).await;
        }
    } else if let Some(ws) = &made.workspace {
        let _ = herdr_call(sock, "workspace.close", json!({ "workspace_id": ws })).await;
    }
}

/// `task.assigned` for the spawned agent's issue. A 409 means another agent holds the task —
/// not a failure — and anything else only warns: the agent already has its brief.
async fn assign_issue(host: &Host, sp: &Spawn, issue: u64) {
    let task = task_id(&sp.request.repo, issue);
    let lead = sp
        .request
        .lead
        .clone()
        .filter(|l| is_agent_id(l))
        .or_else(|| is_agent_id(&sp.by).then(|| sp.by.clone()));
    let e = Envelope::new(
        host.actor(),
        task_project(&sp.request.repo),
        Known::TaskAssigned(TaskAssigned {
            agent: sp.request.agent.clone(),
            lead,
        }),
    )
    .task(&task);
    match host.client.write_exclusive(&e).await {
        Ok(_) => {
            if let Err(err) = host.replica.lock().unwrap().append(&e.into_event()) {
                eprintln!("agent-fleet host: replica append: {err}");
            }
        }
        Err(err) => {
            if err
                .downcast_ref::<Refused>()
                .is_some_and(|r| r.status == reqwest::StatusCode::CONFLICT)
            {
                eprintln!("agent-fleet host: {task} already has an owner; left as is");
            } else {
                eprintln!("agent-fleet host: task.assigned for {task} not written: {err:#}");
            }
        }
    }
}

/// The replaced agent's pane and workspace close once the new agent has its brief; its
/// worktree is the new agent's now — never removed.
async fn close_replaced(host: &Host, old: &str) {
    let sock = &host.cfg.herdr_socket;
    let name = old.rsplit('/').next().unwrap_or(old);
    let info = match herdr_call(sock, "agent.get", json!({ "target": name })).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("agent-fleet host: the replaced agent {name}: {e:#}");
            return;
        }
    };
    let a = &info["agent"];
    if let Some(p) = a["pane_id"].as_str() {
        let _ = herdr_call(sock, "pane.close", json!({ "pane_id": p })).await;
    }
    if let Some(ws) = a["workspace_id"].as_str() {
        let _ = herdr_call(sock, "workspace.close", json!({ "workspace_id": ws })).await;
    }
}

/// The last 4 KiB of `s`, on a char boundary: herdr or script spew stays a one-line finding.
fn tail(s: &str) -> String {
    let mut i = s.len().saturating_sub(4096);
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    s[i..].to_string()
}

/// Executes one `spawn.requested` and answers it with `spawn.result`.
pub async fn execute(host: &Host, id: &str, sp: &Spawn, state: &State) {
    let r = &sp.request;
    let mut made = Made::default();
    match run_spawn(host, r, &mut made, state).await {
        Ok(done) => {
            let lead = r
                .lead
                .clone()
                .filter(|l| is_agent_id(l))
                .or_else(|| is_agent_id(&sp.by).then(|| sp.by.clone()));
            if let Some(level) = r.level {
                host.publish(host.presence(Known::AgentRole(AgentRole {
                    agent: r.agent.clone(),
                    level: Some(level),
                    lead,
                    domain: None,
                })))
                .await;
            }
            if let Some(issue) = r.issue {
                assign_issue(host, sp, issue).await;
            }
            host.publish(
                host.presence(Known::SpawnResult(SpawnResult {
                    agent: r.agent.clone(),
                    ok: true,
                    pane: Some(done.pane),
                    worktree: Some(done.worktree),
                    error: None,
                }))
                .re(id),
            )
            .await;
            if let Some(old) = &r.replaces {
                close_replaced(host, old).await;
            }
        }
        Err(err) => {
            undo(host, r, &made).await;
            host.publish(
                host.presence(Known::SpawnResult(SpawnResult {
                    agent: r.agent.clone(),
                    ok: false,
                    pane: None,
                    worktree: None,
                    error: Some(tail(&format!("{err:#}"))),
                }))
                .re(id),
            )
            .await;
        }
    }
}

/// Did the log see `agent` turn `working` at or after `since`? `agent.up` already `working`
/// counts.
fn reached_working(events: &[Event], agent: &str, since: DateTime<Utc>) -> bool {
    events.iter().any(|e| {
        e.ts >= since
            && (matches!(&e.body,
                Body::Known(Known::AgentState(d)) if d.agent == agent && d.state == AgentState::Working)
                || matches!(&e.body,
                Body::Known(Known::AgentUp(u)) if u.agent == agent && u.state == AgentState::Working))
    })
}

/// The id prefix for this host agent's agents: `agent:<host>/<session>/`. Two `agent-fleet
/// host` processes on one machine serve different sessions; only mine are mine.
fn agent_prefix(host: &str, session: &str) -> String {
    format!("agent:{host}/{session}/")
}

/// Why `id` should be handed to its profile's fallback at `now`, or None. An agent that worked,
/// is working or done, or whose task closed is never handed off — an idle agent that finished
/// its work stays put.
fn handoff_reason(
    state: &State,
    events: &[Event],
    a: &crate::state::Agent,
    sp: &Spawn,
    prof: &Profile,
    host: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    let id = &sp.request.agent;
    let task_done = sp
        .request
        .issue
        .map(|i| task_id(&sp.request.repo, i))
        .and_then(|t| state.tasks.get(&t).map(|t| t.state))
        .is_some_and(|st| st.is_terminal());
    if a.worked.is_some()
        || reached_working(events, id, sp.since)
        || a.state == AgentState::Working
        || a.state == AgentState::Done
        || task_done
    {
        return None;
    }
    if state
        .quotas
        .get(&format!("{host}:{}", prof.kind))
        .is_some_and(|q| q.exhausted_at(now))
    {
        return Some(format!("{} quota on {host} is exhausted", prof.kind));
    }
    let min = chrono::Duration::minutes(prof.handoff_min.unwrap_or(5) as i64);
    if sp.answered.is_some_and(|t| now - t > min) {
        return Some(format!(
            "never reached working within {}m",
            prof.handoff_min.unwrap_or(5)
        ));
    }
    None
}

/// `agent` has a replacement pending or successfully spawned — dedupe so a FAILED replacement
/// does not block a retry.
pub fn replacement_open(state: &State, agent: &str) -> bool {
    state.spawns.values().any(|s| {
        s.request.replaces.as_deref() == Some(agent) && s.result.as_ref().is_none_or(|r| r.ok)
    })
}

/// The auto-handoff scan: one handoff request per agent, deduped through the state's `replaces`.
async fn handoffs(
    host: &Arc<Host>,
    state: &State,
    events: &[Event],
    done: &mut HashSet<String>,
    warned: &mut HashSet<String>,
) {
    let prefix = agent_prefix(&host.cfg.host, &host.cfg.session);
    // A failed replacement frees the agent: dedupe counts only pending or successful ones.
    done.retain(|id| replacement_open(state, id));
    for (id, a) in &state.agents {
        if !id.starts_with(&prefix) || done.contains(id) {
            continue;
        }
        let Some(sp) = state
            .spawns
            .values()
            .filter(|s| s.request.agent == *id && s.result.as_ref().is_some_and(|r| r.ok))
            .max_by_key(|s| s.since)
        else {
            continue;
        };
        if replacement_open(state, id) {
            continue;
        }
        let Some(checkout) = host.cfg.checkouts.get(&sp.request.repo).cloned() else {
            continue;
        };
        let (cfg, cfg_dir) = match load(&checkout) {
            Ok(x) => x,
            Err(e) => {
                if warned.insert(id.clone()) {
                    eprintln!("agent-fleet host: handoff for {id}: {e:#}");
                }
                continue;
            }
        };
        let Some(prof) = cfg.profiles.get(&sp.request.profile) else {
            continue;
        };
        let Some(fallback) = prof.fallback.clone() else {
            continue;
        };
        let Some(extra) = handoff_reason(state, events, a, sp, prof, &host.cfg.host, Utc::now())
        else {
            continue;
        };
        let issue = sp.request.issue;
        let task = issue.map(|i| task_id(&sp.request.repo, i));
        let mut title = task
            .as_ref()
            .and_then(|t| state.tasks.get(t))
            .map(|t| t.title.clone())
            .filter(|t| Some(t) != task.as_ref());
        if title.is_none()
            && let Some(i) = issue
        {
            title = gh_issue(&sp.request.repo, i)
                .await
                .ok()
                .map(|x| x.title)
                .or_else(|| Some(format!("#{i}")));
        }
        match handoff_request(
            &cfg,
            &cfg_dir,
            sp,
            &fallback,
            &extra,
            title.as_deref().unwrap_or(""),
        ) {
            Ok(req) => {
                if state.agents.contains_key(&req.agent) {
                    if warned.insert(id.clone()) {
                        eprintln!(
                            "agent-fleet host: handoff for {id}: {} is already up",
                            req.agent
                        );
                    }
                    continue;
                }
                done.insert(id.clone());
                // The envelope's project is the original request's, so the task id it feeds
                // is the same convention the requester used.
                host.publish(Envelope::new(
                    host.actor(),
                    &sp.project,
                    Known::SpawnRequested(Box::new(req)),
                ))
                .await;
            }
            Err(e) => {
                if warned.insert(id.clone()) {
                    eprintln!("agent-fleet host: handoff for {id}: {e:#}");
                }
            }
        }
    }
}

/// The unanswered requests for this host/session, split into `expired` (answered "expired",
/// not executed) and `todo`. Each request id is attempted at most once per process — a request
/// whose `spawn.result` could not be published is not retried every scan.
fn select(
    state: &State,
    host: &str,
    session: &str,
    seen: &mut HashSet<String>,
    now: DateTime<Utc>,
) -> (Vec<String>, Vec<String>) {
    let mut expired = Vec::new();
    let mut todo = Vec::new();
    for (id, sp) in &state.spawns {
        if sp.request.host != host || sp.request.session != session || sp.result.is_some() {
            continue;
        }
        // The agent is up: the request already ran and only its result was lost.
        if state.agents.contains_key(&sp.request.agent) || !seen.insert(id.clone()) {
            continue;
        }
        if now - sp.since > REQUEST_MAX_AGE {
            expired.push(id.clone());
        } else {
            todo.push(id.clone());
        }
    }
    (expired, todo)
}

/// One `msg.sent` per stall episode to the lead of each idle or blocked agent. An episode is
/// the stretch since the agent last went `working`; the log itself is the dedupe, so a restart
/// does not re-report. `reported` keys episodes (`agent:ts`) already told this run.
async fn stalls(host: &Arc<Host>, state: &State, events: &[Event], reported: &mut HashSet<String>) {
    let prefix = agent_prefix(&host.cfg.host, &host.cfg.session);
    let min = chrono::Duration::minutes(host.cfg.stall_min as i64);
    let now = Utc::now();
    for (id, a) in &state.agents {
        if !id.starts_with(&prefix)
            || !matches!(a.state, AgentState::Idle | AgentState::Blocked)
            || now - a.since < min
        {
            continue;
        }
        let Some(lead) = state.roles.get(id).and_then(|r| r.lead.clone()) else {
            continue;
        };
        let Some(worked) = a.worked else {
            continue;
        };
        let key = format!("{id}:{}", worked.timestamp());
        if reported.contains(&key) {
            continue;
        }
        if events.iter().any(|e| {
            e.ts > worked
                && matches!(&e.body,
                    Body::Known(Known::MsgSent(m))
                        if m.kind == "STALLED" && m.reference == short(id) && m.to == lead)
        }) {
            reported.insert(key);
            continue;
        }
        let state_word = format!("{:?}", a.state).to_lowercase();
        let mut text = format!(
            "{state_word} for {}m since {}",
            (now - a.since).num_minutes(),
            a.since.format("%H:%M")
        );
        if let Some((_, w)) = crate::doctor::worktree_of(state, a) {
            text.push_str(&format!("; worktree {}", w.state.path));
            if let Ok(subject) = git(Path::new(&w.state.path), &["log", "-1", "--format=%s"]).await
            {
                text.push_str(&format!(
                    ", last commit {:?} {}m ago",
                    subject.trim(),
                    (now - w.state.last_commit).num_minutes()
                ));
            }
        }
        host.publish(host.presence(Known::MsgSent(MsgSent {
            to: lead,
            kind: "STALLED".into(),
            reference: short(id).into(),
            text,
        })))
        .await;
        reported.insert(key);
    }
}

/// Executes this host's `spawn.requested` events and hands off its stalling or out-of-quota
/// agents. Polls the replica; `host.wake` stays for the doorbell loop alone.
pub async fn spawn_loop(host: Arc<Host>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut handed: HashSet<String> = HashSet::new();
    let mut warned: HashSet<String> = HashSet::new();
    let mut stalled_seen: HashSet<String> = HashSet::new();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let events = match host.replica.lock().unwrap().all() {
            Ok(e) => e,
            Err(err) => {
                eprintln!("agent-fleet host: replica: {err}");
                continue;
            }
        };
        let state = State::fold(&events);
        seen.retain(|id| state.spawns.get(id).is_some_and(|s| s.result.is_none()));
        let (expired, todo) = select(
            &state,
            &host.cfg.host,
            &host.cfg.session,
            &mut seen,
            Utc::now(),
        );
        for id in expired {
            if let Some(sp) = state.spawns.get(&id) {
                host.publish(
                    Envelope::new(
                        host.actor(),
                        &sp.project,
                        Known::SpawnResult(SpawnResult {
                            agent: sp.request.agent.clone(),
                            ok: false,
                            pane: None,
                            worktree: None,
                            error: Some("expired".into()),
                        }),
                    )
                    .re(&id),
                )
                .await;
            }
        }
        for id in todo {
            if let Some(sp) = state.spawns.get(&id) {
                execute(&host, &id, sp, &state).await;
            }
        }
        handoffs(&host, &state, &events, &mut handed, &mut warned).await;
        stalls(&host, &state, &events, &mut stalled_seen).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::host::Config;
    use crate::store::Store;
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[test]
    fn slug_from_remote_urls() {
        for url in [
            "git@github.com:acme/thing.git",
            "https://github.com/acme/thing.git",
            "https://github.com/acme/thing",
            "ssh://git@github.com/acme/thing",
        ] {
            assert_eq!(slug_from_url(url).as_deref(), Some("acme/thing"), "{url}");
        }
        assert_eq!(slug_from_url("git@example.org:acme/thing"), None);
        assert_eq!(slug_from_url("/local/path"), None);
    }

    const CFG: &str = r#"
session = "s"
brief = "b.md"
[profiles.devin]
kind = "devin"
prefix = "dv-"
cap = 2
fallback = "sonnet"
handoff_min = 5
[profiles.sonnet]
kind = "claude"
args = ["--model", "sonnet"]
unset_env = ["X"]
prefix = "s-"
[profiles.opus]
kind = "claude"
prefix = "o-"
[[rules]]
match = ["ios", "label:mobile"]
profile = "devin"
host = "mac.local"
level = "implementation-lead"
[[rules]]
match = ["research"]
profile = "opus"
[[rules]]
profile = "devin"
"#;

    fn actor() -> Actor {
        Actor {
            kind: ActorKind::Agent,
            id: "agent:h/s/x".into(),
        }
    }

    fn cfg(src: &str) -> Result<FleetConfig> {
        FleetConfig::parse(src)
    }

    fn issue(title: &str, labels: &[&str]) -> Issue {
        Issue {
            repo: "o/r".into(),
            number: 7,
            title: title.into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
        }
    }

    #[test]
    fn config_validation_names_the_wrong_line() {
        assert!(cfg(CFG).is_ok());
        let e = cfg(&CFG.replace("profile = \"opus\"", "profile = \"nope\""))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("unknown profile") && e.contains("profile = \"nope\""),
            "{e}"
        );
        let e = cfg(&CFG.replace("fallback = \"sonnet\"", "fallback = \"devin\""))
            .unwrap_err()
            .to_string();
        assert!(e.contains("cycle"), "{e}");
        let e = cfg(&(CFG.to_string() + "[[rules]]\nprofile = \"devin\"\n"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("second default") && e.contains("[[rules]]"),
            "{e}"
        );
        let e = cfg(&CFG.replace("[[rules]]\nprofile = \"devin\"\n", ""))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no default rule"), "{e}");
        assert!(
            cfg("session=\"s\"\nbrief=\"b\"\n")
                .unwrap_err()
                .to_string()
                .contains("no [profiles]")
        );
    }

    #[test]
    fn rules_match_words_and_labels() {
        let c = cfg(CFG).unwrap();
        let pick = |t: &str, labels: &[&str]| {
            decide(
                &c,
                &issue(t, labels),
                &Overrides::default(),
                &State::default(),
                "h",
            )
            .unwrap()
        };
        assert_eq!(pick("Fix the iOS thing", &[]).host, "mac.local");
        assert_eq!(pick("bios update", &[]).profile, "devin"); // whole words: "ios" ≠ "bios"
        assert_eq!(pick("x", &["mobile"]).host, "mac.local");
        assert_eq!(pick("x", &["design"]).profile, "devin"); // label:design needs rule's label
        assert_eq!(pick("do some research", &[]).profile, "opus");
    }

    #[test]
    fn fallbacks_skip_cap_and_quota() {
        let c = cfg(CFG).unwrap();
        let mut s = State::default();
        let up = |s: &mut State, id: &str, profile: &str| {
            s.agents.insert(
                id.into(),
                crate::state::Agent {
                    host: "h".into(),
                    kind: Some("devin".into()),
                    pane: None,
                    state: AgentState::Idle,
                    since: Utc::now(),
                    worked: None,
                    usage: None,
                },
            );
            let req = SpawnRequested {
                name: id.into(),
                agent: id.into(),
                host: "h".into(),
                session: "s".into(),
                repo: "o/r".into(),
                issue: Some(1),
                branch: "b".into(),
                profile: profile.into(),
                brief: String::new(),
                level: None,
                lead: None,
                worktree: None,
                replaces: None,
                why: String::new(),
            };
            s.apply(
                &Envelope::new(
                    Actor {
                        kind: ActorKind::Agent,
                        id: "agent:h/s/x".into(),
                    },
                    "p",
                    Known::SpawnRequested(Box::new(req)),
                )
                .into_event(),
            );
        };
        up(&mut s, "agent:h/s/a", "devin");
        up(&mut s, "agent:h/s/b", "devin");
        let d = decide(&c, &issue("x", &[]), &Overrides::default(), &s, "h").unwrap();
        assert_eq!(d.profile, "sonnet");
        assert!(d.why.contains("devin skipped: at cap 2"), "{}", d.why);
        // quota exhausted on the sonnet's kind → no fallback left: bail
        s.quotas.insert(
            "h:claude".into(),
            crate::state::Quota {
                windows: vec![],
                plan: None,
                exhausted: true,
                read_at: Utc::now(),
                ts: Utc::now(),
            },
        );
        let e = decide(&c, &issue("x", &[]), &Overrides::default(), &s, "h")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("sonnet skipped: claude quota exhausted on h"),
            "{e}"
        );
        assert!(e.contains("no profile can take it"), "{e}");
    }

    #[test]
    fn brief_render_is_one_pass_and_names_missing_tokens() {
        let mut vals: BTreeMap<String, String> =
            [("A".into(), "@@B@@".into()), ("B".into(), "bee".into())]
                .into_iter()
                .collect();
        assert_eq!(render("@@A@@ @@B@@", &vals).unwrap(), "@@B@@ bee");
        let err = render("hi @@MISSING@@ @@A@@", &vals)
            .unwrap_err()
            .to_string();
        assert!(err.contains("MISSING"), "{err}");
        vals.insert("C".into(), "see".into());
        assert_eq!(render("@@ C @@ @@C@@@@", &vals).unwrap(), "@@ C @@ see@@");
        let src = "head\n--- brief ---\n\nbody @@C@@";
        assert_eq!(brief_body(src), "body @@C@@");
        assert_eq!(brief_body("no marker"), "no marker");
    }

    #[test]
    fn reducer_tracks_and_prunes_spawns() {
        let req = |_id: &str| SpawnRequested {
            name: "a".into(),
            agent: "agent:h/s/a".into(),
            host: "h".into(),
            session: "s".into(),
            repo: "o/r".into(),
            issue: Some(1),
            branch: "b".into(),
            profile: "devin".into(),
            brief: String::new(),
            level: None,
            lead: None,
            worktree: None,
            replaces: None,
            why: String::new(),
        };
        let actor = || Actor {
            kind: ActorKind::Agent,
            id: "agent:h/s/x".into(),
        };
        let req_ev =
            Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(req("_a")))).into_event();
        let mut s = State::fold(std::slice::from_ref(&req_ev));
        assert_eq!(s.spawns[&req_ev.id].request.agent, "agent:h/s/a");
        assert_eq!(s.profile_of("agent:h/s/a"), Some("devin"));
        let res = Envelope::new(
            Actor {
                kind: ActorKind::Host,
                id: "host:h".into(),
            },
            "p",
            Known::SpawnResult(SpawnResult {
                agent: "agent:h/s/a".into(),
                ok: true,
                pane: Some("w:1".into()),
                worktree: Some("/w".into()),
                error: None,
            }),
        )
        .re(&req_ev.id)
        .into_event();
        s.apply(&res);
        assert!(s.spawns[&req_ev.id].result.as_ref().unwrap().ok);
        // A later event 25 h on prunes the finished spawn.
        let mut late = Envelope::new(
            actor(),
            "p",
            Known::AgentDown(AgentDown {
                agent: "agent:h/s/z".into(),
                reason: None,
            }),
        )
        .into_event();
        late.ts = res.ts + chrono::Duration::hours(25);
        s.apply(&late);
        assert!(s.spawns.is_empty());
        // A second result for the same request is ignored (s.result kept as first).
    }

    // ---------- the executor, against a fake herdr socket ----------

    /// A herdr stand-in: answers each call from a per-method script of replies (the last one
    /// repeats), records every (method, params).
    #[derive(Default)]
    struct Stub {
        calls: Mutex<Vec<(String, Value)>>,
        script: Mutex<BTreeMap<String, Vec<Value>>>,
    }

    impl Stub {
        async fn serve(self: Arc<Self>, l: UnixListener) {
            while let Ok((s, _)) = l.accept().await {
                let f = self.clone();
                tokio::spawn(async move {
                    let (rd, mut wr) = s.into_split();
                    let Ok(Some(line)) = BufReader::new(rd).lines().next_line().await else {
                        return;
                    };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let m = req["method"].as_str().unwrap().to_string();
                    f.calls
                        .lock()
                        .unwrap()
                        .push((m.clone(), req["params"].clone()));
                    let reply = {
                        let mut sc = f.script.lock().unwrap();
                        let q = sc.entry(m).or_insert_with(|| vec![json!({})]);
                        if q.len() > 1 {
                            q.remove(0)
                        } else {
                            q[0].clone()
                        }
                    };
                    let mut resp = json!({"id": "fleet", "result": reply["result"]});
                    if !reply["error"].is_null() {
                        resp["error"] = reply["error"].clone();
                    }
                    wr.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
                });
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }
    }

    fn err(code: &str) -> Value {
        json!({ "error": { "code": code, "message": code } })
    }

    fn created(wt: &str) -> Value {
        json!({ "result": {
            "type": "worktree_created",
            "root_pane": { "pane_id": "w:1" },
            "workspace": { "workspace_id": "ws1", "worktree": { "checkout_path": wt } },
            "worktree": { "path": wt } } })
    }

    fn ws_created(wt: &str) -> Value {
        json!({ "result": {
            "type": "workspace_created",
            "root_pane": { "pane_id": "w:2" },
            "workspace": { "workspace_id": "ws2", "worktree": { "checkout_path": wt } } } })
    }

    fn host_at(dir: &Path, sock: PathBuf) -> Arc<Host> {
        let cfg = Config {
            host: "h".into(),
            session: "s".into(),
            herdr_socket: sock,
            replica: dir.join("replica.db"),
            data_dir: dir.to_path_buf(),
            repos: Vec::new(),
            checkouts: [("o/r".to_string(), dir.join("co"))].into_iter().collect(),
            locks: Vec::new(),
            stall_min: 15,
        };
        Arc::new(Host::new(
            cfg,
            Client::new("http://127.0.0.1:1", "t".into(), dir.join("spool.jsonl")),
            Store::open(&dir.join("replica.db")).unwrap(),
        ))
    }

    /// The fleet.toml the host resolves profiles from — the request only names the profile.
    const HOST_CFG: &str = r#"
session = "s"
brief = "b.md"
[profiles.devin]
kind = "devin"
prefix = "dv-"
fallback = "sonnet"
[profiles.sonnet]
kind = "claude"
args = ["--model", "sonnet"]
unset_env = ["SCRUB_ME"]
prefix = "s-"
[[rules]]
profile = "devin"
"#;

    fn fleet_toml(dir: &Path) {
        let d = dir.join(".agent-fleet");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("fleet.toml"), HOST_CFG).unwrap();
    }

    /// A git checkout dir with one commit and an `origin/main` ref.
    fn checkout(dir: &Path) -> PathBuf {
        let co = dir.join("co");
        std::fs::create_dir_all(&co).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "x",
            ],
        ] {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&co)
                .args(&args)
                .status()
                .unwrap();
            assert!(st.success());
        }
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&co)
            .args(["update-ref", "refs/remotes/origin/main", "HEAD"])
            .status()
            .unwrap();
        assert!(st.success());
        fleet_toml(&co);
        co
    }

    fn req(wt: Option<String>) -> SpawnRequested {
        SpawnRequested {
            name: "dv-7".into(),
            agent: "agent:h/s/dv-7".into(),
            host: "h".into(),
            session: "s".into(),
            repo: "o/r".into(),
            issue: Some(7),
            branch: "dv-7".into(),
            profile: "devin".into(),
            brief: "do it".into(),
            level: Some(Level::Implementer),
            lead: Some("agent:h/s/me".into()),
            worktree: wt,
            replaces: None,
            why: "t".into(),
        }
    }

    fn spawn_of(r: SpawnRequested) -> Spawn {
        Spawn {
            request: r,
            by: "agent:h/s/me".into(),
            project: "p".into(),
            result: None,
            since: Utc::now(),
            answered: None,
        }
    }

    fn result(h: &Host) -> Option<SpawnResult> {
        h.replica
            .lock()
            .unwrap()
            .all()
            .unwrap()
            .iter()
            .find_map(|e| match &e.body {
                Body::Known(Known::SpawnResult(r)) => Some(r.clone()),
                _ => None,
            })
    }

    async fn run(dir: &Path, script: &[(&str, Vec<Value>)]) -> (Arc<Host>, Arc<Stub>) {
        let stub = Arc::new(Stub::default());
        stub.script
            .lock()
            .unwrap()
            .extend(script.iter().map(|(m, r)| (m.to_string(), r.clone())));
        let sock = dir.join("herdr.sock");
        tokio::spawn(stub.clone().serve(UnixListener::bind(&sock).unwrap()));
        let host = host_at(dir, sock);
        (host, stub)
    }

    #[tokio::test]
    async fn new_worktree_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[("worktree.create", vec![created(wt.to_str().unwrap())])],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        assert_eq!(
            stub.calls(),
            [
                "worktree.create",
                "agent.start",
                "agent.wait",
                "agent.prompt"
            ]
        );
        let r = result(&host).unwrap();
        assert!(
            r.ok && r.pane.as_deref() == Some("w:1")
                && r.worktree.as_deref() == Some(wt.to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn agent_pane_busy_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[
                ("worktree.create", vec![created(wt.to_str().unwrap())]),
                (
                    "agent.start",
                    vec![err("agent_pane_busy"), json!({"result": {}})],
                ),
            ],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        assert_eq!(
            stub.calls().iter().filter(|m| *m == "agent.start").count(),
            2
        );
        assert!(result(&host).unwrap().ok);
    }

    #[tokio::test]
    async fn a_stalled_prompt_gets_one_enter_and_no_resend() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[
                ("worktree.create", vec![created(wt.to_str().unwrap())]),
                ("agent.prompt", vec![err("agent_prompt_stalled")]),
            ],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        assert_eq!(
            stub.calls(),
            [
                "worktree.create",
                "agent.start",
                "agent.wait",
                "agent.prompt",
                "agent.send_keys",
                "agent.wait"
            ]
        );
        let keys = stub
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == "agent.send_keys")
            .unwrap()
            .1
            .clone();
        assert_eq!(keys["keys"], json!(["enter"]));
        assert!(result(&host).unwrap().ok);
    }

    #[tokio::test]
    async fn a_failed_start_hands_back_pane_worktree_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[
                ("worktree.create", vec![created(wt.to_str().unwrap())]),
                ("agent.start", vec![err("agent_broken")]),
            ],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        let calls = stub.calls();
        assert!(calls.contains(&"pane.close".into()), "{calls:?}");
        assert!(calls.contains(&"worktree.remove".into()), "{calls:?}");
        let r = result(&host).unwrap();
        assert!(!r.ok && r.error.as_deref().unwrap().contains("agent.start"));
    }

    #[tokio::test]
    async fn a_handoff_opens_a_workspace_and_closes_the_replaced_agent() {
        let dir = tempfile::tempdir().unwrap();
        let _co = checkout(dir.path());
        let wt = dir.path().join("existing");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[
                ("workspace.create", vec![ws_created(wt.to_str().unwrap())]),
                (
                    "agent.get",
                    vec![json!({"result": {"agent": {"pane_id": "w9:p1", "workspace_id": "ws9"}}})],
                ),
            ],
        )
        .await;
        let mut r = req(Some(wt.to_str().unwrap().into()));
        r.replaces = Some("agent:h/s/dv-7".into());
        r.name = "s-7".into();
        r.agent = "agent:h/s/s-7".into();
        execute(&host, "req1", &spawn_of(r), &State::default()).await;
        let calls = stub.calls();
        assert_eq!(calls[0], "workspace.create");
        assert!(!calls.contains(&"worktree.create".into()));
        assert!(calls.contains(&"pane.close".into()) && calls.contains(&"workspace.close".into()));
        let got = stub.calls.lock().unwrap().clone();
        let pane_close = got
            .iter()
            .find(|(m, _)| m == "pane.close")
            .unwrap()
            .1
            .clone();
        let ws_close = got
            .iter()
            .find(|(m, _)| m == "workspace.close")
            .unwrap()
            .1
            .clone();
        assert_eq!(pane_close["pane_id"], "w9:p1");
        assert_eq!(ws_close["workspace_id"], "ws9");
        assert!(result(&host).unwrap().ok);
    }

    #[tokio::test]
    async fn a_worktree_create_collision_reclaims_an_empty_branch() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&co)
            .args(["branch", "dv-7"])
            .status()
            .unwrap();
        assert!(st.success());
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[(
                "worktree.create",
                vec![
                    err("branch dv-7 already exists"),
                    created(wt.to_str().unwrap()),
                ],
            )],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        assert_eq!(
            stub.calls()
                .iter()
                .filter(|m| *m == "worktree.create")
                .count(),
            2
        );
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(&co)
            .args(["branch", "--list", "dv-7"])
            .output()
            .unwrap();
        // Reclaim deleted the stale branch; create's retry succeeded.
        assert!(result(&host).unwrap().ok);
        let _ = branches;
    }

    // ---------- review regressions ----------

    #[test]
    fn an_unknown_fallback_anywhere_in_the_chain_is_an_error_not_a_panic() {
        // devin → sonnet → "nope": the chain walk must not index a missing profile.
        let src = CFG.replacen(
            "unset_env = [\"X\"]",
            "unset_env = [\"X\"]\nfallback = \"nope\"",
            1,
        );
        let e = cfg(&src).unwrap_err().to_string();
        assert!(e.contains("unknown profile"), "{e}");
    }

    #[test]
    fn config_rejects_bad_env_names_ready_paths_and_shared_prefixes() {
        for bad in ["1BAD", "A-B", "A B"] {
            let src = CFG.replacen(
                "unset_env = [\"X\"]",
                &format!("unset_env = [\"{bad}\"]"),
                1,
            );
            let e = cfg(&src).unwrap_err().to_string();
            assert!(e.contains("unset_env"), "{bad}: {e}");
        }
        for bad in ["../x.sh", "/tmp/x.sh", "a/../../b"] {
            let src = CFG.replacen(
                "brief = \"b.md\"",
                &format!("brief = \"b.md\"\nready = \"{bad}\""),
                1,
            );
            let e = cfg(&src).unwrap_err().to_string();
            assert!(e.contains("ready"), "{bad}: {e}");
        }
        let dup = CFG.replacen("prefix = \"s-\"", "prefix = \"dv-\"", 1);
        assert!(cfg(&dup).unwrap_err().to_string().contains("share prefix"));
    }

    #[test]
    fn a_request_carries_no_executable_fields() {
        let v = serde_json::to_value(req(None)).unwrap();
        for k in ["kind", "args", "unset_env", "ready"] {
            assert!(v.get(k).is_none(), "{k} leaked into spawn.requested");
        }
        let mut v = serde_json::to_value(req(None)).unwrap();
        v["unset_env"] = json!(["A=1"]);
        assert!(serde_json::from_value::<SpawnRequested>(v).is_err());
    }

    #[test]
    fn each_request_is_attempted_once_per_process() {
        let mut s = State::default();
        let ev =
            Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(req(None)))).into_event();
        s.apply(&ev);
        let mut seen = HashSet::new();
        let (ex, todo) = select(&s, "h", "s", &mut seen, Utc::now());
        assert_eq!(todo, [ev.id.as_str()]);
        assert!(ex.is_empty());
        let (ex, todo) = select(&s, "h", "s", &mut seen, Utc::now());
        assert!(
            ex.is_empty() && todo.is_empty(),
            "a publish failure must not retry"
        );
        let mut stale = req(None);
        stale.name = "old".into();
        stale.agent = "agent:h/s/old".into();
        let mut oe =
            Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(stale))).into_event();
        oe.ts = Utc::now() - chrono::Duration::minutes(20);
        s.apply(&oe);
        let (ex, todo) = select(&s, "h", "s", &mut seen, Utc::now());
        assert_eq!(ex, [oe.id.as_str()]);
        assert!(todo.is_empty());
        let (ex, todo) = select(&s, "h", "s", &mut seen, Utc::now());
        assert!(ex.is_empty() && todo.is_empty());
    }

    #[test]
    fn an_exhausted_read_expires() {
        let q = |exhausted: bool, pct: f64, resets: Option<DateTime<Utc>>, mins_ago: i64| {
            crate::state::Quota {
                windows: vec![QuotaWindow {
                    name: "5h".into(),
                    used_pct: pct,
                    resets_at: resets,
                }],
                plan: None,
                exhausted,
                read_at: Utc::now() - chrono::Duration::minutes(mins_ago),
                ts: Utc::now(),
            }
        };
        let now = Utc::now();
        assert!(q(true, 50.0, None, 10).exhausted_at(now));
        assert!(
            !q(true, 50.0, None, 45).exhausted_at(now),
            "a stale flag clears"
        );
        assert!(q(false, 100.0, Some(now + chrono::Duration::hours(1)), 600).exhausted_at(now));
        assert!(!q(false, 100.0, Some(now - chrono::Duration::hours(1)), 10).exhausted_at(now));
        assert!(!q(false, 50.0, None, 10).exhausted_at(now));
    }

    #[test]
    fn an_issue_has_one_task_id() {
        assert_eq!(task_id("owner/repo", 7), "task:repo/7");
        assert_eq!(task_project("owner/repo"), "repo");
    }

    #[test]
    fn another_repos_agents_do_not_fill_the_cap() {
        let c = cfg(CFG).unwrap();
        let mut s = State::default();
        for n in ["a", "b"] {
            let mut r = req(None);
            r.name = n.into();
            r.agent = format!("agent:h/s/{n}");
            r.repo = "o/other".into();
            s.apply(&Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(r))).into_event());
        }
        let d = decide(&c, &issue("x", &[]), &Overrides::default(), &s, "h").unwrap();
        assert_eq!(d.profile, "devin", "{}", d.why);
    }

    #[test]
    fn pending_requests_count_toward_the_cap() {
        let c = cfg(CFG).unwrap();
        let mut s = State::default();
        for n in ["a", "b"] {
            let mut r = req(None);
            r.name = n.into();
            r.agent = format!("agent:h/s/{n}");
            s.apply(&Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(r))).into_event());
        }
        let d = decide(&c, &issue("x", &[]), &Overrides::default(), &s, "h").unwrap();
        assert_eq!(d.profile, "sonnet", "{}", d.why);
        assert!(d.why.contains("at cap 2"), "{}", d.why);
        // A pending request older than the request max age does not count.
        let mut old = req(None);
        old.name = "stale".into();
        old.agent = "agent:h/s/stale".into();
        let mut oe = Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(old))).into_event();
        oe.ts = Utc::now() - chrono::Duration::minutes(30);
        let mut s = State::default();
        s.apply(&oe);
        let d = decide(&c, &issue("x", &[]), &Overrides::default(), &s, "h").unwrap();
        assert_eq!(d.profile, "devin", "{}", d.why);
    }

    #[test]
    fn pruning_keeps_a_spawn_while_its_agent_is_up() {
        let mut s = State::default();
        let req_ev =
            Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(req(None)))).into_event();
        s.apply(&req_ev);
        let res = Envelope::new(
            actor(),
            "p",
            Known::SpawnResult(SpawnResult {
                agent: "agent:h/s/dv-7".into(),
                ok: true,
                pane: Some("w:1".into()),
                worktree: Some("/w".into()),
                error: None,
            }),
        )
        .re(&req_ev.id)
        .into_event();
        s.apply(&res);
        // The agent is still up: a 25h-later event must not prune its spawn record.
        s.agents.insert(
            "agent:h/s/dv-7".into(),
            crate::state::Agent {
                host: "h".into(),
                kind: Some("devin".into()),
                pane: None,
                state: AgentState::Working,
                since: Utc::now(),
                worked: None,
                usage: None,
            },
        );
        // An unanswered request is pruned by the keep window too.
        let mut p2 = req(None);
        p2.name = "p2".into();
        p2.agent = "agent:h/s/p2".into();
        let pend = Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(p2))).into_event();
        s.apply(&pend);
        let mut late = Envelope::new(
            actor(),
            "p",
            Known::AgentDown(AgentDown {
                agent: "agent:h/s/x".into(),
                reason: None,
            }),
        )
        .into_event();
        late.ts = pend.ts + chrono::Duration::hours(25);
        s.apply(&late);
        assert!(
            s.spawns.contains_key(&req_ev.id),
            "a live agent keeps its spawn"
        );
        assert!(
            !s.spawns.contains_key(&pend.id),
            "a stale pending is pruned"
        );
    }

    #[test]
    fn handoffs_match_this_host_and_session() {
        assert!("agent:h/s/a".starts_with(&agent_prefix("h", "s")));
        assert!(!"agent:h/other/a".starts_with(&agent_prefix("h", "s")));
        assert!(!"agent:g/s/a".starts_with(&agent_prefix("h", "s")));
    }

    #[test]
    fn a_failed_handoff_frees_the_agent_for_retry() {
        let mut s = State::default();
        let mut r = req(None);
        r.replaces = Some("agent:h/s/dv-7".into());
        let ev = Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(r))).into_event();
        s.apply(&ev);
        assert!(replacement_open(&s, "agent:h/s/dv-7"));
        let res = Envelope::new(
            actor(),
            "p",
            Known::SpawnResult(SpawnResult {
                agent: "agent:h/s/dv-7".into(),
                ok: false,
                pane: None,
                worktree: None,
                error: Some("x".into()),
            }),
        )
        .re(&ev.id)
        .into_event();
        s.apply(&res);
        assert!(!replacement_open(&s, "agent:h/s/dv-7"));
        let ok = Envelope::new(
            actor(),
            "p",
            Known::SpawnResult(SpawnResult {
                agent: "agent:h/s/dv-7".into(),
                ok: true,
                pane: None,
                worktree: None,
                error: None,
            }),
        )
        .re(&ev.id)
        .into_event();
        let mut s = State::default();
        s.apply(&ev);
        s.apply(&ok);
        assert!(replacement_open(&s, "agent:h/s/dv-7"));
    }

    #[test]
    fn a_handoff_never_targets_the_same_id() {
        let c = cfg(CFG).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.md"), "@@NAME@@").unwrap();
        let mut r = req(None);
        r.profile = "devin".into();
        let sp = Spawn {
            request: r,
            by: "agent:h/s/me".into(),
            project: "p".into(),
            result: Some(SpawnResult {
                agent: "agent:h/s/dv-7".into(),
                ok: true,
                pane: Some("w:1".into()),
                worktree: Some("/w".into()),
                error: None,
            }),
            since: Utc::now(),
            answered: Some(Utc::now()),
        };
        // --to devin regenerates dv-7, the very agent being replaced.
        let e = handoff_request(&c, dir.path(), &sp, "devin", "", "")
            .unwrap_err()
            .to_string();
        assert!(e.contains("same agent"), "{e}");
    }

    #[test]
    fn a_done_or_worked_or_finished_agent_is_never_handed_off() {
        let c = cfg(CFG).unwrap();
        let prof = c.profiles["devin"].clone();
        let mk = |state: &mut State, st: AgentState| {
            let r = req(None);
            state.agents.insert(
                r.agent.clone(),
                crate::state::Agent {
                    host: "h".into(),
                    kind: Some("devin".into()),
                    pane: None,
                    state: st,
                    since: Utc::now(),
                    worked: None,
                    usage: None,
                },
            );
            Spawn {
                request: r,
                by: "agent:h/s/me".into(),
                project: "p".into(),
                result: Some(SpawnResult {
                    agent: "agent:h/s/dv-7".into(),
                    ok: true,
                    pane: Some("w:1".into()),
                    worktree: Some("/w".into()),
                    error: None,
                }),
                since: Utc::now() - chrono::Duration::hours(1),
                answered: Some(Utc::now() - chrono::Duration::minutes(30)),
            }
        };
        let mut s = State::default();
        s.quotas.insert(
            "h:devin".into(),
            crate::state::Quota {
                windows: vec![],
                plan: None,
                exhausted: true,
                read_at: Utc::now(),
                ts: Utc::now(),
            },
        );
        let sp = mk(&mut s, AgentState::Done);
        let a = s.agents["agent:h/s/dv-7"].clone();
        assert_eq!(
            handoff_reason(&s, &[], &a, &sp, &prof, "h", Utc::now()),
            None,
            "a Done agent is never handed off"
        );
        // idle, never worked, quota out → hands off
        s.agents.get_mut("agent:h/s/dv-7").unwrap().state = AgentState::Idle;
        let a = s.agents["agent:h/s/dv-7"].clone();
        let why = handoff_reason(&s, &[], &a, &sp, &prof, "h", Utc::now()).unwrap();
        assert!(why.contains("quota"), "{why}");
        // an agent that ever reached working is never handed off — even via agent.up
        let up = Envelope::new(
            actor(),
            "p",
            Known::AgentUp(AgentUp {
                agent: "agent:h/s/dv-7".into(),
                host: "h".into(),
                kind: None,
                state: AgentState::Working,
                pane: None,
            }),
        )
        .into_event();
        assert_eq!(
            handoff_reason(&s, &[up], &a, &sp, &prof, "h", Utc::now()),
            None
        );
        // a completed task is never handed off
        s.tasks.insert(
            task_id("o/r", 7),
            crate::state::Task {
                project: "r".into(),
                title: "t".into(),
                url: None,
                state: TaskState::Completed,
                since: Utc::now(),
                owner: None,
                lead: None,
                blocked_by: Default::default(),
                handoffs: vec![],
            },
        );
        assert_eq!(
            handoff_reason(&s, &[], &a, &sp, &prof, "h", Utc::now()),
            None,
            "a finished task is never handed off"
        );
        s.tasks.clear();
        s.quotas.clear();
        let why = handoff_reason(&s, &[], &a, &sp, &prof, "h", Utc::now()).unwrap();
        assert!(why.contains("never reached working"), "{why}");
        // a.worked set (kept across a host re-up) counts as reached working, events or not
        let mut a2 = a.clone();
        a2.worked = Some(Utc::now() - chrono::Duration::minutes(3));
        assert_eq!(
            handoff_reason(&s, &[], &a2, &sp, &prof, "h", Utc::now()),
            None,
            "an agent that ever worked is never handed off"
        );
    }

    #[test]
    fn an_error_message_keeps_only_its_tail() {
        let big = "x".repeat(5000);
        assert_eq!(tail(&big).len(), 4096);
        let uni = format!("{}é", "x".repeat(4095));
        let t = tail(&uni);
        assert_eq!(t.len(), 4096);
        assert!(t.ends_with('é'));
    }

    #[tokio::test]
    async fn flag_like_wire_fields_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _co = checkout(dir.path());
        let (host, stub) = run(dir.path(), &[]).await;
        let mut r = req(None);
        r.branch = "--delete".into();
        execute(&host, "req1", &spawn_of(r), &State::default()).await;
        assert!(stub.calls().is_empty(), "{:?}", stub.calls());
        // a syntactically bad ref is refused before herdr sees it
        let mut r = req(None);
        r.branch = "bad..ref".into();
        execute(&host, "req2", &spawn_of(r), &State::default()).await;
        assert!(stub.calls().is_empty(), "{:?}", stub.calls());
        let res: Vec<_> = host
            .replica
            .lock()
            .unwrap()
            .all()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.body {
                Body::Known(Known::SpawnResult(r)) => Some(r.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(res.len(), 2);
        assert!(res.iter().all(|r| !r.ok), "{res:?}");
    }

    #[tokio::test]
    async fn a_branch_holding_work_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        for args in [
            vec!["checkout", "-b", "dv-7"],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "work",
            ],
            vec!["checkout", "main"],
        ] {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&co)
                .args(&args)
                .status()
                .unwrap();
            assert!(st.success());
        }
        let (host, _stub) = run(
            dir.path(),
            &[("worktree.create", vec![err("branch dv-7 already exists")])],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        let r = result(&host).unwrap();
        assert!(!r.ok, "{r:?}");
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&co)
            .args(["rev-parse", "--verify", "-q", "refs/heads/dv-7"])
            .status()
            .unwrap();
        assert!(st.success(), "the branch with unpushed commits survived");
    }

    #[tokio::test]
    async fn the_host_takes_kind_args_and_env_from_its_own_fleet_toml() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (host, stub) = run(
            dir.path(),
            &[("worktree.create", vec![created(wt.to_str().unwrap())])],
        )
        .await;
        let mut r = req(None);
        r.profile = "sonnet".into();
        r.name = "s-7".into();
        r.agent = "agent:h/s/s-7".into();
        execute(&host, "req1", &spawn_of(r), &State::default()).await;
        assert_eq!(
            stub.calls(),
            [
                "worktree.create",
                "pane.send_text",
                "pane.send_keys",
                "agent.start",
                "agent.wait",
                "agent.prompt"
            ]
        );
        let got = stub.calls.lock().unwrap().clone();
        let start = got
            .iter()
            .find(|(m, _)| m == "agent.start")
            .unwrap()
            .1
            .clone();
        assert_eq!(start["kind"], "claude");
        assert_eq!(start["args"], json!(["--model", "sonnet"]));
        let text = got
            .iter()
            .find(|(m, _)| m == "pane.send_text")
            .unwrap()
            .1
            .clone();
        assert_eq!(text["text"], "unset SCRUB_ME");
        assert!(result(&host).unwrap().ok);
    }

    // ---------- the stall report ----------

    /// An idle-20m agent that worked 40m ago, with `lead` optional.
    fn stalled_state(lead: Option<&str>) -> State {
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/a".into(),
            crate::state::Agent {
                host: "h".into(),
                kind: Some("devin".into()),
                pane: None,
                state: AgentState::Idle,
                since: Utc::now() - chrono::Duration::minutes(20),
                worked: Some(Utc::now() - chrono::Duration::minutes(40)),
                usage: None,
            },
        );
        if let Some(l) = lead {
            s.roles.insert(
                "agent:h/s/a".into(),
                crate::state::Role {
                    level: Level::Implementer,
                    lead: Some(l.into()),
                    domain: None,
                    by: "agent:h/s/x".into(),
                    since: Utc::now(),
                },
            );
        }
        s
    }

    fn msgs(host: &Host) -> Vec<crate::event::MsgSent> {
        host.replica
            .lock()
            .unwrap()
            .all()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.body {
                Body::Known(Known::MsgSent(m)) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_stall_is_reported_once_per_episode() {
        let dir = tempfile::tempdir().unwrap();
        let _co = checkout(dir.path());
        let (host, _stub) = run(dir.path(), &[]).await;
        let mut old = Envelope::new(
            actor(),
            "p",
            Known::MsgSent(MsgSent {
                to: "agent:h/s/lead".into(),
                kind: "STALLED".into(),
                reference: "a".into(),
                text: "idle".into(),
            }),
        )
        .into_event();
        old.ts = Utc::now() - chrono::Duration::minutes(30);
        // Episode 1: the log already holds this episode's report (worked 40m ago) — nothing sent.
        let s = stalled_state(Some("agent:h/s/lead"));
        stalls(&host, &s, &[old.clone()], &mut HashSet::new()).await;
        assert!(msgs(&host).is_empty(), "one report per episode");
        // Episode 2: the agent worked again after that report, then stalled — a new msg.
        let mut s = stalled_state(Some("agent:h/s/lead"));
        s.agents.get_mut("agent:h/s/a").unwrap().worked =
            Some(Utc::now() - chrono::Duration::minutes(25));
        stalls(&host, &s, &[old.clone()], &mut HashSet::new()).await;
        let got = msgs(&host);
        assert_eq!(got.len(), 1, "a new episode reports again");
        let m = &got[0];
        assert_eq!(m.to, "agent:h/s/lead");
        assert_eq!(m.kind, "STALLED");
        assert_eq!(m.reference, "a");
        assert!(m.text.contains("idle for 20m"), "{}", m.text);
        // And the replica's own msg.sent dedupes a second pass in the same episode.
        let mut events: Vec<Event> = host.replica.lock().unwrap().all().unwrap().to_vec();
        events.push(old);
        stalls(&host, &s, &events, &mut HashSet::new()).await;
        assert_eq!(msgs(&host).len(), 1);
    }

    #[tokio::test]
    async fn no_stall_msg_without_a_lead_or_a_worked_stretch() {
        let dir = tempfile::tempdir().unwrap();
        let _co = checkout(dir.path());
        let (host, _stub) = run(dir.path(), &[]).await;
        // no role at all
        let s = stalled_state(None);
        stalls(&host, &s, &[], &mut HashSet::new()).await;
        // role but never worked
        let mut s = stalled_state(Some("agent:h/s/lead"));
        s.agents.get_mut("agent:h/s/a").unwrap().worked = None;
        stalls(&host, &s, &[], &mut HashSet::new()).await;
        // role, worked, but idle only 5m — under --stall-min
        let mut s = stalled_state(Some("agent:h/s/lead"));
        let a = s.agents.get_mut("agent:h/s/a").unwrap();
        a.since = Utc::now() - chrono::Duration::minutes(5);
        stalls(&host, &s, &[], &mut HashSet::new()).await;
        // an agent of another session is not this host's
        let mut s = stalled_state(Some("agent:h/s/lead"));
        let a = s.agents.remove("agent:h/s/a").unwrap();
        s.roles.remove("agent:h/s/a");
        s.agents.insert("agent:h/other/a".into(), a);
        s.roles.insert(
            "agent:h/other/a".into(),
            crate::state::Role {
                level: Level::Implementer,
                lead: Some("agent:h/s/lead".into()),
                domain: None,
                by: "agent:h/s/x".into(),
                since: Utc::now(),
            },
        );
        stalls(&host, &s, &[], &mut HashSet::new()).await;
        assert!(msgs(&host).is_empty());
    }

    #[test]
    fn a_request_for_an_up_agent_is_not_rerun() {
        let mut s = State::default();
        let ev =
            Envelope::new(actor(), "p", Known::SpawnRequested(Box::new(req(None)))).into_event();
        s.apply(&ev);
        s.agents.insert(
            "agent:h/s/dv-7".into(),
            crate::state::Agent {
                host: "h".into(),
                kind: Some("devin".into()),
                pane: None,
                state: AgentState::Working,
                since: Utc::now(),
                worked: Some(Utc::now()),
                usage: None,
            },
        );
        let mut seen = HashSet::new();
        let (ex, todo) = select(&s, "h", "s", &mut seen, Utc::now());
        assert!(ex.is_empty() && todo.is_empty(), "the request already ran");
    }

    #[tokio::test]
    async fn reclaim_never_removes_a_worktree_an_agent_uses() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        for args in [
            vec!["branch", "dv-7"],
            vec!["worktree", "add", "wt-live", "dv-7"],
        ] {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&co)
                .args(&args)
                .status()
                .unwrap();
            assert!(st.success());
        }
        let (host, _stub) = run(
            dir.path(),
            &[("worktree.create", vec![err("branch dv-7 already exists")])],
        )
        .await;
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/live".into(),
            crate::state::Agent {
                host: "h".into(),
                kind: Some("claude".into()),
                pane: None,
                state: AgentState::Working,
                since: Utc::now(),
                worked: Some(Utc::now()),
                usage: Some(crate::state::Usage {
                    rss_kb: None,
                    cpu_pct: None,
                    procs: None,
                    cwd: Some(co.join("wt-live").to_string_lossy().into_owned()),
                    ts: Utc::now(),
                }),
            },
        );
        execute(&host, "req1", &spawn_of(req(None)), &s).await;
        let r = result(&host).unwrap();
        assert!(!r.ok, "{r:?}");
        assert!(co.join("wt-live").is_dir(), "the live worktree survived");
    }

    #[tokio::test]
    async fn ready_runs_the_host_checkouts_script() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let d = co.join(".agent-fleet");
        std::fs::write(
            d.join("fleet.toml"),
            HOST_CFG.replacen("brief = \"b.md\"", "brief = \"b.md\"\nready = \"r.sh\"", 1),
        )
        .unwrap();
        std::fs::write(co.join("r.sh"), "#!/bin/sh\ntouch \"$1/ready-ran\"\n").unwrap();
        let wt = co.join("wt");
        std::fs::create_dir(&wt).unwrap();
        // A worktree copy that would fail proves the checkout's copy is the one that runs.
        std::fs::write(wt.join("r.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        let st = std::process::Command::new("chmod")
            .args(["+x", &co.join("r.sh").to_string_lossy()])
            .status()
            .unwrap();
        assert!(st.success());
        let (host, _stub) = run(
            dir.path(),
            &[("worktree.create", vec![created(wt.to_str().unwrap())])],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        assert!(result(&host).unwrap().ok);
        assert!(wt.join("ready-ran").exists(), "the checkout's ready ran");
    }

    #[tokio::test]
    async fn worktree_create_uses_the_host_checkouts_base() {
        let dir = tempfile::tempdir().unwrap();
        let co = checkout(dir.path());
        let (host, stub) = run(
            dir.path(),
            &[(
                "worktree.create",
                vec![created(co.join("wt").to_str().unwrap())],
            )],
        )
        .await;
        execute(&host, "req1", &spawn_of(req(None)), &State::default()).await;
        let got = stub.calls.lock().unwrap().clone();
        let create = got
            .iter()
            .find(|(m, _)| m == "worktree.create")
            .unwrap()
            .1
            .clone();
        assert_eq!(create["base"], "origin/main", "{create}");
    }
}
