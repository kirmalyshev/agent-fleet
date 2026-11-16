//! `dev agents` and `dev prs`: open PRs with their health (checks, mergeable), and the fleet's
//! agents working on them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::state::State;

/// An open PR, as `gh pr list --json number,title,headRefName,url,mergeable,statusCheckRollup`
/// gives it.
#[derive(Debug, Deserialize)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    #[serde(rename = "headRefName")]
    pub branch: String,
    pub url: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    #[serde(default)]
    pub mergeable: String,
    #[serde(rename = "statusCheckRollup", default)]
    pub checks: Option<Vec<Check>>,
}

/// A check run (`status`, `conclusion`) or a commit status (`state`).
#[derive(Debug, Default, Deserialize)]
pub struct Check {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Health {
    Ok,
    /// A check still runs.
    Pending,
    /// GitHub has not worked out mergeability yet (it does on the first ask).
    Unknown,
    Failing,
    Conflict,
}

impl Health {
    fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
            Self::Failing => "failing",
            Self::Conflict => "conflict",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            Self::Ok => "32",
            Self::Pending | Self::Unknown => "33",
            Self::Failing | Self::Conflict => "31",
        }
    }
}

/// Conflicting, else a failed check, else a check still running, else mergeability not yet
/// known, else ok.
pub fn health(pr: &OpenPr) -> Health {
    let checks = pr.checks.iter().flatten();
    let failed = |c: &Check| {
        matches!(
            c.conclusion.as_deref(),
            Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
        ) || matches!(c.state.as_deref(), Some("FAILURE" | "ERROR"))
    };
    let running = |c: &Check| {
        c.status.as_deref().is_some_and(|s| s != "COMPLETED")
            || matches!(c.state.as_deref(), Some("PENDING" | "EXPECTED"))
    };
    if pr.mergeable == "CONFLICTING" {
        Health::Conflict
    } else if checks.clone().any(failed) {
        Health::Failing
    } else if checks.clone().any(running) {
        Health::Pending
    } else if pr.mergeable == "UNKNOWN" {
        Health::Unknown
    } else {
        Health::Ok
    }
}

/// `#12 failing`, padded to `width`, in the health's colour when `colour`.
fn cell(pr: Option<&OpenPr>, width: usize, colour: bool) -> String {
    let Some(pr) = pr else {
        return format!("{:<width$}", "-");
    };
    let h = health(pr);
    let text = format!("{:<width$}", format!("#{} {}", pr.number, h.word()));
    if colour {
        format!("\x1b[{}m{text}\x1b[0m", h.colour())
    } else {
        text
    }
}

/// The open PRs of `slug` (only yours when `mine`): one `gh pr list`. `AGENT_FLEET_GH` names the
/// binary.
pub fn open_prs(slug: &str, mine: bool) -> Result<Vec<OpenPr>> {
    let gh = std::env::var("AGENT_FLEET_GH").unwrap_or_else(|_| "gh".into());
    let mut c = std::process::Command::new(&gh);
    c.args([
        "pr", "list", "-R", slug, "--state", "open", "--limit", "200",
    ])
    .args([
        "--json",
        "number,title,headRefName,url,mergeable,statusCheckRollup",
    ]);
    if mine {
        c.args(["--author", "@me"]);
    }
    let out = c.output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn agent_state(s: &State, id: &str) -> String {
    s.agents
        .get(id)
        .and_then(|a| serde_json::to_value(a.state).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `dev agents`: each agent working in a worktree of the checkout `repo` on `host`, with its
/// slot (`slots`: worktree path → slot; the main checkout is 0) and its branch's open PR.
pub fn agents(
    s: &State,
    host: &str,
    repo: &str,
    prs: &[OpenPr],
    slots: &BTreeMap<String, u32>,
    colour: bool,
) -> String {
    let mut out = format!(
        "{:<24} {:<8} {:>4} {:<28} {:<14} WORKTREE\n",
        "AGENT", "STATE", "SLOT", "BRANCH", "PR"
    );
    let mut any = false;
    for (key, ids) in crate::doctor::workers(s) {
        let Some(w) = s.worktrees.get(&key).map(|w| &w.state) else {
            continue;
        };
        if w.host != host || w.repo != repo {
            continue;
        }
        let slot = if w.path == repo {
            "0".into()
        } else {
            slots.get(&w.path).map_or("-".into(), u32::to_string)
        };
        let branch = w.branch.as_deref().unwrap_or("-");
        let pr = prs.iter().find(|p| Some(&p.branch) == w.branch.as_ref());
        for id in ids {
            any = true;
            out += &format!(
                "{:<24} {:<8} {slot:>4} {branch:<28} {} {}\n",
                crate::host::short(&id),
                agent_state(s, &id),
                cell(pr, 14, colour),
                w.path
            );
        }
    }
    if !any {
        out += "(no agents in this repository)\n";
    }
    out
}

/// `dev prs`: per repository (`slug`, checkout, its open PRs or why they are unknown), each PR
/// with its health and the agents on it: those working in a worktree of that checkout on `host`
/// with the PR's branch, and the owners of open tasks whose url is the PR.
pub fn prs(
    s: &State,
    host: &str,
    repos: &[(String, PathBuf, Result<Vec<OpenPr>>)],
    colour: bool,
) -> String {
    let workers = crate::doctor::workers(s);
    let mut out = String::new();
    for (slug, dir, prs) in repos {
        let prs = match prs {
            Ok(p) if p.is_empty() => continue,
            Ok(p) => p,
            Err(e) => {
                out += &format!("{slug}\n  gh failed: {e:#}\n");
                continue;
            }
        };
        out += &format!("{slug}\n");
        let dir = dir.to_string_lossy();
        for pr in prs {
            let mut on: Vec<&str> = workers
                .iter()
                .filter(|(key, _)| {
                    s.worktrees.get(*key).is_some_and(|w| {
                        w.state.host == host
                            && w.state.repo == dir
                            && w.state.branch.as_ref() == Some(&pr.branch)
                    })
                })
                .flat_map(|(_, ids)| ids.iter().map(String::as_str))
                .collect();
            on.extend(
                s.tasks
                    .iter()
                    .filter(|(_, t)| t.url.as_ref() == Some(&pr.url))
                    .filter_map(|(id, _)| s.owner_of(id)),
            );
            on.sort_unstable();
            on.dedup();
            let on: Vec<&str> = on.into_iter().map(crate::host::short).collect();
            let on = if on.is_empty() {
                "-".into()
            } else {
                on.join(",")
            };
            out += &format!(
                "  {} {:<28} {on:<20} {}\n",
                cell(Some(pr), 14, colour),
                pr.branch,
                pr.title
            );
        }
    }
    if out.is_empty() {
        out += "(no open PRs of yours)\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentState, WorktreeState};
    use crate::state::{Agent, Usage, Worktree, worktree_key};
    use chrono::Utc;

    fn pr(n: u64, branch: &str, mergeable: &str, checks: &str) -> OpenPr {
        serde_json::from_str(&format!(
            r#"{{"number":{n},"title":"t{n}","headRefName":"{branch}",
               "url":"https://github.com/o/r/pull/{n}","mergeable":"{mergeable}",
               "statusCheckRollup":{checks}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn health_reads_checks_and_mergeable() {
        let done = r#"[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
                       {"__typename":"StatusContext","state":"SUCCESS"}]"#;
        assert_eq!(health(&pr(1, "b", "MERGEABLE", done)), Health::Ok);
        assert_eq!(health(&pr(1, "b", "MERGEABLE", "null")), Health::Ok);
        assert_eq!(health(&pr(1, "b", "UNKNOWN", done)), Health::Unknown);
        assert_eq!(health(&pr(1, "b", "CONFLICTING", done)), Health::Conflict);
        let running = r#"[{"status":"IN_PROGRESS","conclusion":""}]"#;
        assert_eq!(health(&pr(1, "b", "MERGEABLE", running)), Health::Pending);
        let failed = r#"[{"status":"IN_PROGRESS","conclusion":""},
                         {"status":"COMPLETED","conclusion":"FAILURE"}]"#;
        assert_eq!(health(&pr(1, "b", "MERGEABLE", failed)), Health::Failing);
        let status = r#"[{"state":"ERROR"}]"#;
        assert_eq!(health(&pr(1, "b", "MERGEABLE", status)), Health::Failing);
        let c = cell(Some(&pr(7, "b", "CONFLICTING", "[]")), 14, true);
        assert_eq!(c, "\x1b[31m#7 conflict   \x1b[0m");
    }

    fn state() -> State {
        let now = Utc::now();
        let mut s = State::default();
        for (path, branch) in [("/r/main", "main"), ("/w/c-1", "c-1"), ("/x/other", "c-1")] {
            let repo = if path == "/x/other" {
                "/x/main"
            } else {
                "/r/main"
            };
            s.worktrees.insert(
                worktree_key("h", path),
                Worktree {
                    state: WorktreeState {
                        host: "h".into(),
                        path: path.into(),
                        repo: repo.into(),
                        branch: Some(branch.into()),
                        head: "abc".into(),
                        unpushed: 0,
                        dirty: 0,
                        dirty_submodules: 0,
                        files: vec![],
                        last_commit: now,
                        in_main: false,
                        herdr: true,
                    },
                    since: now,
                },
            );
        }
        for (name, cwd) in [
            ("c-1", "/w/c-1/src"),
            ("lead", "/r/main"),
            ("o", "/x/other"),
        ] {
            s.agents.insert(
                format!("agent:h/s/{name}"),
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
        }
        s
    }

    #[test]
    fn agents_of_this_repo_with_slot_branch_and_pr() {
        let s = state();
        let prs = [pr(5, "c-1", "MERGEABLE", "[]")];
        let slots = BTreeMap::from([("/w/c-1".to_string(), 3)]);
        let out = agents(&s, "h", "/r/main", &prs, &slots, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(
            lines[1].starts_with("lead") && lines[1].contains(" 0 main"),
            "{out}"
        );
        assert!(
            lines[2].starts_with("c-1")
                && lines[2].contains(" 3 c-1")
                && lines[2].contains("#5 ok"),
            "{out}"
        );
        // Another repository's agent is not listed, nor are another host's.
        assert!(!out.contains("/x/other"), "{out}");
        assert!(agents(&s, "g", "/r/main", &prs, &slots, false).contains("no agents"));
    }

    #[test]
    fn prs_name_the_agent_on_each() {
        let s = state();
        let repos = vec![
            (
                "o/r".to_string(),
                PathBuf::from("/r/main"),
                Ok(vec![
                    pr(5, "c-1", "MERGEABLE", "[]"),
                    pr(6, "nobody", "UNKNOWN", "[]"),
                ]),
            ),
            ("o/empty".to_string(), PathBuf::from("/e"), Ok(vec![])),
            (
                "o/x".to_string(),
                PathBuf::from("/x"),
                Err(anyhow::anyhow!("no gh")),
            ),
        ];
        let out = prs(&s, "h", &repos, false);
        assert!(out.contains("#5 ok") && out.contains("c-1  "), "{out}");
        assert!(out.contains("#6 unknown") && out.contains(" -  "), "{out}");
        // The other repository's agent on a branch of the same name is not this PR's.
        assert!(!out.contains(" o "), "{out}");
        assert!(!out.contains("o/empty"), "{out}");
        assert!(out.contains("o/x\n  gh failed: no gh"), "{out}");
    }
}
