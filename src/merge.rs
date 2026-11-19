//! `agent-fleet merge <owner/repo>#<pr> <sha>`: the one way an approved PR merges. The reviewer's
//! `VERDICT: READY <sha>` must be the PR's last comment and the head still that sha; no pin may
//! move backwards; the repository's own gate (fleet.toml `[merge] gate`) or its required CI
//! checks must pass; then `gh pr merge --squash --match-head-commit`, never `--admin`, and
//! `merge.done` in the log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::event::Envelope;

/// fleet.toml's `[merge]`, read from the PR's base branch so a PR cannot change its own gate.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// A script in the repository, run in this machine's checkout with the head sha as $1.
    /// Absent: the repository's required CI checks decide.
    #[serde(default)]
    pub gate: Option<String>,
    /// The fleet host that runs the merge (and so the gate). Absent: the host the CLI runs on.
    #[serde(default)]
    pub host: Option<String>,
}

/// The `[merge]` table of a fleet.toml's text; the rest of the file is not this command's.
pub fn config_of(src: &str) -> Result<Config> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        merge: Config,
    }
    let c = toml::from_str::<File>(src)?.merge;
    if let Some(g) = &c.gate {
        crate::spawn::check_ready(g).map_err(|e| anyhow!("[merge] gate {g:?}: {e}"))?;
    }
    Ok(c)
}

/// `VERDICT: READY <head>` as the last non-blank line of the PR's last comment (`*` and
/// backticks around it are fine), with the head's full sha. Err says why not.
pub fn verdict(last_comment: Option<&str>, head: &str) -> Result<(), String> {
    let want = format!("VERDICT: READY {}", head.to_ascii_lowercase());
    let Some(c) = last_comment else {
        return Err(format!(
            "the PR has no comments; want `{want}` as its last one"
        ));
    };
    let last = c
        .lines()
        .rev()
        .map(|l| l.trim_matches(|ch: char| ch.is_whitespace() || ch == '*' || ch == '`'))
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    match last.strip_prefix("VERDICT: READY ") {
        Some(s) if s.to_ascii_lowercase() == want["VERDICT: READY ".len()..] => Ok(()),
        _ => Err(format!(
            "the PR's last comment ends {last:?}, not `{want}` (the head's full sha)"
        )),
    }
}

/// The (old, new) pins of a submodule change, from its patch in the PR's file list.
pub fn pin_change(patch: &str) -> Option<(String, String)> {
    let mut l = patch.lines();
    (l.next()? == "@@ -1 +1 @@").then_some(())?;
    let old = l.next()?.strip_prefix("-Subproject commit ")?;
    let new = l.next()?.strip_prefix("+Subproject commit ")?;
    Some((old.trim().into(), new.trim().into()))
}

/// `owner/repo` of a submodule URL on GitHub; a relative URL resolves against `repo`'s owner.
pub fn github_slug(url: &str, repo: &str) -> Option<String> {
    let path = if let Some(rel) = url.strip_prefix("../") {
        format!("{}/{rel}", repo.split('/').next()?)
    } else {
        [
            "https://github.com/",
            "ssh://git@github.com/",
            "git@github.com:",
        ]
        .iter()
        .find_map(|p| url.strip_prefix(p))?
        .to_string()
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty() => Some(format!("{o}/{r}")),
        _ => None,
    }
}

/// What `gh pr checks --required --json name,bucket` says: Ok(None) all passed, Ok(Some)
/// still running (their names), Err the failed ones.
pub fn checks(v: &serde_json::Value) -> Result<Option<Vec<String>>, Vec<String>> {
    let all = v.as_array().map(Vec::as_slice).unwrap_or_default();
    let named = |want: &[&str]| -> Vec<String> {
        all.iter()
            .filter(|c| want.contains(&c["bucket"].as_str().unwrap_or("fail")))
            .map(|c| c["name"].as_str().unwrap_or("?").to_string())
            .collect()
    };
    let failed = named(&["fail", "cancel"]);
    if all.is_empty() || !failed.is_empty() {
        return Err(failed);
    }
    let running = named(&["pending"]);
    Ok((!running.is_empty()).then_some(running))
}

/// The approved `sha` (7 to 40 hex digits) against the PR's head.
pub fn head_matches(head: &str, sha: &str) -> Result<(), String> {
    if !(7..=40).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{sha:?} is not a sha (7 to 40 hex digits)"));
    }
    if !head
        .to_ascii_lowercase()
        .starts_with(&sha.to_ascii_lowercase())
    {
        return Err(format!("head moved: {head} (approved {sha})"));
    }
    Ok(())
}

/// One merge to run on this host.
pub struct Ask<'a> {
    /// The `gh` binary (`AGENT_FLEET_GH`, else `gh`).
    pub gh: &'a str,
    pub repo: &'a str,
    pub pr: u64,
    pub sha: &'a str,
    pub allow_pin_rewind: Option<&'a str>,
    /// This machine's checkout of `repo`, asked for only when a gate is configured.
    pub checkout: &'a (dyn Fn() -> Result<PathBuf> + Sync),
    /// The unit of every wait (1 s; the tests use 0): `mergeable` 5 × 6, checks 27 × 20, the
    /// merge's landing 5 × 3.
    pub tick: Duration,
}

/// A merged PR.
#[derive(Debug, PartialEq)]
pub struct Merged {
    pub head: String,
    pub commit: Option<String>,
}

/// `gh args`' stdout; Err carries its stderr. `ok_codes` are exit codes that still answer
/// (`gh pr checks` exits 8 while checks are pending).
async fn gh(bin: &str, args: &[&str], ok_codes: &[i32]) -> Result<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("run gh {}", args.join(" ")))?;
    let code = out.status.code().unwrap_or(-1);
    // An allowed failure still has to answer: empty stdout is the plain failure it looks like.
    if !out.status.success() && (!ok_codes.contains(&code) || out.stdout.is_empty()) {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

struct Pr {
    state: String,
    head: String,
    base: String,
    mergeable: String,
}

async fn pr_view(a: &Ask<'_>) -> Result<Pr> {
    let n = a.pr.to_string();
    let out = gh(
        a.gh,
        &[
            "pr",
            "view",
            &n,
            "-R",
            a.repo,
            "--json",
            "state,headRefOid,baseRefName,mergeable",
        ],
        &[],
    )
    .await?;
    let v: Value = serde_json::from_str(&out).context("gh pr view: bad JSON")?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Ok(Pr {
        state: s("state"),
        head: s("headRefOid"),
        base: s("baseRefName"),
        mergeable: s("mergeable"),
    })
}

/// fleet.toml's `[merge]` on the PR's base branch; none when the file is not there.
pub async fn config(gh_bin: &str, repo: &str, base: &str) -> Result<Config> {
    let path = format!("repos/{repo}/contents/.agent-fleet/fleet.toml?ref={base}");
    match gh(
        gh_bin,
        &[
            "api",
            "-H",
            "Accept: application/vnd.github.raw+json",
            &path,
        ],
        &[],
    )
    .await
    {
        Ok(src) => {
            config_of(&src).with_context(|| format!("{repo}'s .agent-fleet/fleet.toml on {base}"))
        }
        Err(e) if format!("{e:#}").contains("404") => Ok(Config::default()),
        Err(e) => Err(e.context(format!("read {repo}'s fleet.toml on {base}"))),
    }
}

/// The PR's base branch and its `[merge]`: where `agent-fleet merge` decides which host runs.
pub async fn plan(gh_bin: &str, repo: &str, pr: u64) -> Result<Config> {
    let n = pr.to_string();
    let out = gh(
        gh_bin,
        &["pr", "view", &n, "-R", repo, "--json", "baseRefName"],
        &[],
    )
    .await?;
    let v: Value = serde_json::from_str(&out).context("gh pr view: bad JSON")?;
    config(gh_bin, repo, v["baseRefName"].as_str().unwrap_or("main")).await
}

/// The PR's last comment. Unreadable refuses: failing open would merge an unreviewed head.
async fn last_comment(a: &Ask<'_>) -> Result<Option<String>> {
    let path = format!("repos/{}/issues/{}/comments", a.repo, a.pr);
    let out = gh(
        a.gh,
        &["api", "--paginate", &path, "--jq", ".[-1].body | @json"],
        &[],
    )
    .await
    .context("the verdict is unreadable")?;
    // One line per page; the last page's last comment is the PR's.
    match out.lines().rfind(|l| !l.trim().is_empty()) {
        None => Ok(None),
        Some(l) => serde_json::from_str::<Option<String>>(l).context("the verdict is unreadable"),
    }
}

async fn approved(a: &Ask<'_>, head: &str) -> Result<()> {
    verdict(last_comment(a).await?.as_deref(), head).map_err(|e| anyhow!("#{}: {e}", a.pr))
}

/// Every submodule pin the PR moves must move forward from the base branch's: the base's pin
/// is an ancestor of the PR's. A pin that cannot be proved forward is refused like a rewind.
async fn pins(a: &Ask<'_>, base: &str, say: &(dyn Fn(String) + Sync)) -> Result<()> {
    let files = format!("repos/{}/pulls/{}/files", a.repo, a.pr);
    let out = gh(
        a.gh,
        &[
            "api",
            "--paginate",
            &files,
            "--jq",
            ".[] | select((.patch // \"\") | startswith(\"@@ -1 +1 @@\\n-Subproject commit \")) | [.filename, .patch] | @json",
        ],
        &[],
    )
    .await
    .context("the PR's files are unreadable, so its pins are")?;
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let (path, patch): (String, String) =
            serde_json::from_str(line).context("gh api pulls/files: bad JSON")?;
        let Some((_, new)) = pin_change(&patch) else {
            continue;
        };
        let at_base = format!("repos/{}/contents/{path}?ref={base}", a.repo);
        let main = match gh(
            a.gh,
            &[
                "api",
                &at_base,
                "--jq",
                "[.type, .sha, (.submodule_git_url // \"\")] | @json",
            ],
            &[],
        )
        .await
        {
            Ok(o) => o,
            // Not on the base branch: a new submodule has nothing to rewind.
            Err(e) if format!("{e:#}").contains("404") => continue,
            Err(e) => return Err(e.context(format!("{path}'s pin on {base} is unreadable"))),
        };
        let (kind, old, url): (String, String, String) =
            serde_json::from_str(main.trim()).context("gh api contents: bad JSON")?;
        if kind != "submodule" || old == new {
            continue;
        }
        let forward = match github_slug(&url, a.repo) {
            None => Err(format!("{url} is not on GitHub")),
            Some(sub) => gh(
                a.gh,
                &[
                    "api",
                    &format!("repos/{sub}/compare/{old}...{new}"),
                    "--jq",
                    ".status",
                ],
                &[],
            )
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|st| match st.trim() {
                "ahead" | "identical" => Ok(()),
                st => Err(format!("{sub} says the PR's pin is {st} of {base}'s")),
            }),
        };
        if let Err(why) = forward {
            let what = format!("{path} moves from {base}'s {old} to {new}, not forward ({why})");
            match a.allow_pin_rewind {
                Some(reason) => say(format!("#{}: ALLOWED pin rewind: {what} — {reason}", a.pr)),
                None => bail!(
                    "#{}: refused: {what}. Rebase onto {base} or restore the pin; a deliberate rewind: --allow-pin-rewind=<reason>",
                    a.pr
                ),
            }
        }
    }
    Ok(())
}

/// The repository's gate, `<checkout>/<gate> <head>`, in the checkout as it is now.
async fn run_gate(
    a: &Ask<'_>,
    gate: &str,
    head: &str,
    say: &(dyn Fn(String) + Sync),
) -> Result<()> {
    let checkout = (a.checkout)()?;
    let script = checkout.join(gate);
    if !script.is_file() {
        bail!(
            "#{}: the merge gate {gate} is not in {}",
            a.pr,
            checkout.display()
        );
    }
    let at = std::process::Command::new("git")
        .args([
            "-C",
            &checkout.to_string_lossy(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    say(format!(
        "#{}: merge gate {gate} at {head} (in {}, at {at})",
        a.pr,
        checkout.display()
    ));
    let mut cmd = tokio::process::Command::new(&script);
    cmd.arg(head)
        .current_dir(&checkout)
        .env("AGENT_FLEET_MERGE_PR", format!("{}#{}", a.repo, a.pr))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(r) = a.allow_pin_rewind {
        cmd.env("AGENT_FLEET_ALLOW_PIN_REWIND", r);
    }
    let out = tokio::time::timeout(Duration::from_secs(60 * 60), cmd.output())
        .await
        .context("the merge gate timed out (60 min) and was killed")?
        .with_context(|| format!("the merge gate {gate} did not start"))?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let said = crate::spawn::tail(said.trim());
    if !said.is_empty() {
        say(said);
    }
    if !out.status.success() {
        bail!("#{}: MERGE GATE FAILED ({})", a.pr, out.status);
    }
    Ok(())
}

/// The required checks, waited on while they run.
async fn required_checks(a: &Ask<'_>) -> Result<()> {
    let n = a.pr.to_string();
    let args = [
        "pr",
        "checks",
        &n,
        "-R",
        a.repo,
        "--required",
        "--json",
        "name,bucket",
    ];
    for _ in 0..27 {
        let out = gh(a.gh, &args, &[1, 8]).await.map_err(|e| {
            anyhow!(
                "#{}: no [merge] gate and its required checks are unreadable ({e:#}); nothing proves the head",
                a.pr
            )
        })?;
        let v: Value = serde_json::from_str(&out).context("gh pr checks: bad JSON")?;
        match checks(&v) {
            Ok(None) => return Ok(()),
            Ok(Some(_)) => tokio::time::sleep(a.tick * 20).await,
            Err(f) if f.is_empty() => bail!(
                "#{}: no [merge] gate and no required checks: nothing proves the head",
                a.pr
            ),
            Err(f) => bail!("#{}: REQUIRED CHECK FAILED: {}", a.pr, f.join(", ")),
        }
    }
    bail!("#{}: required checks still running after 9 min", a.pr)
}

/// Every check, then `gh pr merge`. Err is the refusal, as `agent-fleet merge` prints it.
pub async fn run(a: &Ask<'_>, say: &(dyn Fn(String) + Sync)) -> Result<Merged> {
    let n = a.pr;
    let pr = pr_view(a).await?;
    if pr.state != "OPEN" {
        bail!("#{n} is {}, not open", pr.state);
    }
    head_matches(&pr.head, a.sha).map_err(|e| anyhow!("#{n}: {e}"))?;
    let head = pr.head;
    let cfg = config(a.gh, a.repo, &pr.base).await?;
    approved(a, &head).await?;
    pins(a, &pr.base, say).await?;
    let mut mergeable = pr.mergeable;
    for _ in 0..5 {
        if mergeable != "UNKNOWN" {
            break;
        }
        tokio::time::sleep(a.tick * 6).await;
        mergeable = pr_view(a).await?.mergeable;
    }
    if mergeable == "CONFLICTING" {
        bail!("#{n} CONFLICTS with {}", pr.base);
    }
    match &cfg.gate {
        Some(g) => run_gate(a, g, &head, say).await?,
        None => required_checks(a).await?,
    }
    // Again right before the merge: the gate takes minutes, and a push or a BLOCK posted
    // meanwhile must still stop it.
    let now = pr_view(a).await?;
    if now.head != head {
        bail!(
            "#{n}: head moved during the gate: {} (approved {head})",
            now.head
        );
    }
    approved(a, &head).await?;
    let nn = n.to_string();
    gh(
        a.gh,
        &[
            "pr",
            "merge",
            &nn,
            "-R",
            a.repo,
            "--squash",
            "--match-head-commit",
            &head,
        ],
        &[],
    )
    .await?;
    for _ in 0..5 {
        let out = gh(
            a.gh,
            &[
                "pr",
                "view",
                &nn,
                "-R",
                a.repo,
                "--json",
                "state,mergeCommit",
            ],
            &[],
        )
        .await?;
        let v: Value = serde_json::from_str(&out).context("gh pr view: bad JSON")?;
        if v["state"] == "MERGED" {
            return Ok(Merged {
                head,
                commit: v["mergeCommit"]["oid"].as_str().map(str::to_string),
            });
        }
        tokio::time::sleep(a.tick * 3).await;
    }
    bail!("#{n}: gh pr merge answered, but the PR is not MERGED yet; check it")
}

/// What `agent-fleet merge` prints for a merged PR.
pub fn merged_line(repo: &str, pr: u64, m: &Merged) -> String {
    format!(
        "{repo}#{pr} MERGED at {} (head {})",
        m.commit.as_deref().unwrap_or("an unreported commit"),
        m.head
    )
}

/// The `merge.done` of a merged PR.
pub fn done(repo: &str, pr: u64, m: Merged) -> crate::event::MergeDone {
    crate::event::MergeDone {
        repo: repo.into(),
        pr,
        sha: m.head,
        commit: m.commit,
    }
}

/// How long a `merge.requested` waits for its host: a gate takes minutes.
const WAIT: Duration = Duration::from_secs(75 * 60);

/// Follows the hub until the request's `merge.result` arrives.
pub async fn wait_result(
    client: &crate::client::Client,
    id: &str,
) -> Result<crate::event::MergeResult> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match client.state().await {
            Ok(s) => {
                if let Some(r) = s.merges.get(id).and_then(|m| m.result.clone()) {
                    return Ok(r);
                }
            }
            Err(e) if e.downcast_ref::<crate::client::Refused>().is_some() => {
                bail!("waiting for the merge result: {e:#}");
            }
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no merge.result for {id} within 75 min: the host may be down (or too old an agent-fleet to answer merge requests)"
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The `merge.requested` ids this host should run: its own, unanswered, not seen before, and
/// younger than a spawn request may be.
pub fn due(
    state: &crate::state::State,
    host: &str,
    seen: &mut std::collections::HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    seen.retain(|id| state.merges.get(id).is_some_and(|m| m.result.is_none()));
    state
        .merges
        .iter()
        .filter(|(id, m)| {
            m.request.host == host
                && m.result.is_none()
                && now - m.since <= crate::spawn::REQUEST_MAX_AGE
                && seen.insert((*id).clone())
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// The host's side: each merge asked of this host runs in its own task (a gate takes minutes,
/// the spawn loop must not wait on it) and is answered with `merge.result`, plus `merge.done`.
// ponytail: two host agents on one host (two herdr sessions) both run it; the second finds the
// PR merged and refuses. A per-host claim if a doubled gate run ever costs too much.
pub fn serve(
    host: &std::sync::Arc<crate::host::Host>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Ok(view) = host.view() else { return };
    for id in due(&view.state, &host.cfg.host, seen, chrono::Utc::now()) {
        let Some(m) = view.state.merges.get(&id).cloned() else {
            continue;
        };
        let host = host.clone();
        tokio::spawn(async move {
            let r = &m.request;
            // A self-update drain waits for this: a restart mid-gate would kill it half done.
            let Some(_busy) = host.busy_guard(format!("merge of {}#{}", r.repo, r.pr)) else {
                return;
            };
            let gh = std::env::var("AGENT_FLEET_GH").unwrap_or_else(|_| "gh".into());
            let machine = host.cfg.config_dir.clone();
            let here = host
                .cfg
                .checkouts_now()
                .ok()
                .and_then(|mut c| c.remove(&r.repo));
            let co = || checkout(&r.repo, machine.as_deref(), here.clone());
            let said = std::sync::Mutex::new(Vec::<String>::new());
            let ask = Ask {
                gh: &gh,
                repo: &r.repo,
                pr: r.pr,
                sha: &r.sha,
                allow_pin_rewind: r.allow_pin_rewind.as_deref(),
                checkout: &co,
                tick: Duration::from_secs(1),
            };
            let out = run(&ask, &|l| said.lock().unwrap().push(l)).await;
            let mut text = said.into_inner().unwrap();
            let ok = match out {
                Ok(merged) => {
                    text.push(merged_line(&r.repo, r.pr, &merged));
                    host.publish(Envelope::new(
                        host.actor(),
                        &m.project,
                        crate::event::Known::MergeDone(done(&r.repo, r.pr, merged)),
                    ))
                    .await;
                    true
                }
                Err(e) => {
                    text.push(format!("{e:#}"));
                    false
                }
            };
            let text = crate::spawn::tail(&text.join("\n"));
            eprintln!("agent-fleet host: merge of {}#{}: {text}", r.repo, r.pr);
            host.publish(
                host.presence(crate::event::Known::MergeResult(
                    crate::event::MergeResult { ok, text },
                ))
                .re(&id),
            )
            .await;
        });
    }
}

/// This machine's checkout of `repo` for its gate: the connected one (`agent-fleet repo add`,
/// in `machine`), else `here` when its `origin` is `repo`.
pub fn checkout(repo: &str, machine: Option<&Path>, here: Option<PathBuf>) -> Result<PathBuf> {
    let registered = machine
        .map(crate::setup::registered)
        .transpose()?
        .and_then(|mut r| r.remove(repo));
    crate::retire::checkout_for(repo, registered, here)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const H: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A fake `gh` answering from files in its own directory, logging every call to `calls`.
    struct Fake {
        dir: tempfile::TempDir,
    }

    impl Fake {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path().display();
            let script = format!(
                r#"#!/bin/sh
d={d}
echo "$*" >> $d/calls
case "$*" in
  "pr view 7 -R o/r --json state,headRefOid,baseRefName,mergeable")
    h=$(cat $d/head); [ -f $d/head2 ] && [ -f $d/gated ] && h=$(cat $d/head2)
    echo "{{\"state\":\"OPEN\",\"headRefOid\":\"$h\",\"baseRefName\":\"main\",\"mergeable\":\"MERGEABLE\"}}" ;;
  "pr view 7 -R o/r --json state,mergeCommit") echo '{{"state":"MERGED","mergeCommit":{{"oid":"m1"}}}}' ;;
  "api -H Accept: application/vnd.github.raw+json repos/o/r/contents/.agent-fleet/fleet.toml?ref=main")
    [ -f $d/fleet.toml ] && cat $d/fleet.toml || {{ echo "gh: Not Found (HTTP 404)" >&2; exit 1; }} ;;
  "api --paginate repos/o/r/issues/7/comments"*) cat $d/comment ;;
  "api --paginate repos/o/r/pulls/7/files"*) cat $d/files 2>/dev/null; true ;;
  "api repos/o/r/contents/sub?ref=main"*) cat $d/sub ;;
  "api repos/o/sub/compare/"*) cat $d/compare ;;
  "pr checks 7 -R o/r --required --json name,bucket") cat $d/checks; exit $(cat $d/checks_rc 2>/dev/null || echo 0) ;;
  "pr merge 7"*) echo merged ;;
  *) echo "fake gh: unexpected $*" >&2; exit 3 ;;
esac
"#
            );
            let gh = dir.path().join("gh");
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let f = Self { dir };
            f.put("head", H);
            f.put(
                "comment",
                &serde_json::to_string(&format!("ok\nVERDICT: READY {H}")).unwrap(),
            );
            f.put("checks", r#"[{"name":"build","bucket":"pass"}]"#);
            f
        }
        fn put(&self, name: &str, body: &str) {
            std::fs::write(self.dir.path().join(name), body).unwrap();
        }
        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
        }
        async fn run(&self, sha: &str, allow: Option<&str>) -> (Result<Merged>, Vec<String>) {
            let gh = self.dir.path().join("gh").display().to_string();
            let checkout = self.dir.path().join("checkout");
            let said = std::sync::Mutex::new(vec![]);
            let co = move || Ok(checkout.clone());
            let a = Ask {
                gh: &gh,
                repo: "o/r",
                pr: 7,
                sha,
                allow_pin_rewind: allow,
                checkout: &co,
                tick: Duration::ZERO,
            };
            let r = run(&a, &|l| said.lock().unwrap().push(l)).await;
            (r, said.into_inner().unwrap())
        }
    }

    #[test]
    fn a_host_runs_only_its_own_fresh_unanswered_merges_once() {
        use crate::event::{MergeRequested, MergeResult};
        use crate::state::{Merge, State};
        let now = chrono::Utc::now();
        let mut state = State::default();
        for (id, host, age_min, answered) in [
            ("mine", "box", 1, false),
            ("old", "box", 20, false),
            ("answered", "box", 1, true),
            ("theirs", "mac", 1, false),
        ] {
            state.merges.insert(
                id.into(),
                Merge {
                    request: MergeRequested {
                        repo: "o/r".into(),
                        pr: 7,
                        sha: H.into(),
                        host: host.into(),
                        allow_pin_rewind: None,
                    },
                    by: "agent:mac/s/me".into(),
                    project: "p".into(),
                    result: answered.then(|| MergeResult {
                        ok: true,
                        text: String::new(),
                    }),
                    since: now - chrono::Duration::minutes(age_min),
                },
            );
        }
        let mut seen = Default::default();
        assert_eq!(due(&state, "box", &mut seen, now), vec!["mine".to_string()]);
        assert!(due(&state, "box", &mut seen, now).is_empty(), "once");
        assert_eq!(
            head_matches(H, "xyz"),
            Err("\"xyz\" is not a sha (7 to 40 hex digits)".into())
        );
    }

    #[tokio::test]
    async fn an_approved_head_with_green_required_checks_squash_merges_without_admin() {
        let f = Fake::new();
        let (r, _) = f.run(&H[..9], None).await;
        assert_eq!(
            r.unwrap(),
            Merged {
                head: H.into(),
                commit: Some("m1".into())
            }
        );
        let calls = f.calls();
        assert!(
            calls.contains(&format!(
                "pr merge 7 -R o/r --squash --match-head-commit {H}"
            )),
            "{calls}"
        );
        assert!(!calls.contains("--admin"), "{calls}");
    }

    #[tokio::test]
    async fn a_moved_head_a_missing_verdict_or_red_checks_refuse_and_merge_nothing() {
        async fn refused(f: Fake, sha: &str, says: &str) {
            let e = format!("{:#}", f.run(sha, None).await.0.unwrap_err());
            assert!(e.contains(says), "{e}");
            assert!(!f.calls().contains("pr merge"), "{}", f.calls());
        }
        refused(Fake::new(), "abcdef12", "head moved").await;
        refused(Fake::new(), "xyz", "not a sha").await;

        let f = Fake::new();
        let late = format!("VERDICT: READY {H}\nnit: later");
        f.put("comment", &serde_json::to_string(&late).unwrap());
        refused(f, H, "last comment").await;

        let f = Fake::new();
        f.put("comment", "null");
        refused(f, H, "no comments").await;

        let f = Fake::new();
        f.put("checks", r#"[{"name":"build","bucket":"fail"}]"#);
        f.put("checks_rc", "1");
        refused(f, H, "REQUIRED CHECK FAILED: build").await;

        let f = Fake::new();
        f.put("checks", r#"[{"name":"build","bucket":"pending"}]"#);
        f.put("checks_rc", "8");
        refused(f, H, "still running").await;

        let f = Fake::new();
        f.put("checks", "[]");
        refused(f, H, "no required checks").await;

        let f = Fake::new();
        f.put("checks", "");
        f.put("checks_rc", "1");
        refused(f, H, "unreadable").await;
    }

    #[tokio::test]
    async fn a_pin_rewind_refuses_unless_allowed_with_a_reason() {
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let rewind = || {
            let f = Fake::new();
            let patch = format!("@@ -1 +1 @@\n-Subproject commit {a}\n+Subproject commit {b}");
            f.put("files", &serde_json::to_string(&("sub", patch)).unwrap());
            f.put(
                "sub",
                &serde_json::to_string(&("submodule", "c".repeat(40), "../sub.git")).unwrap(),
            );
            f.put("compare", "behind\n");
            f
        };
        let f = rewind();
        let (r, _) = f.run(H, None).await;
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("sub moves from main's") && e.contains("--allow-pin-rewind"),
            "{e}"
        );
        assert!(!f.calls().contains("pr merge"));

        let f = rewind();
        let (r, said) = f.run(H, Some("revert of a bad bump")).await;
        assert!(r.is_ok(), "{r:?}");
        assert!(
            said.iter()
                .any(|l| l.contains("ALLOWED pin rewind") && l.contains("revert of a bad bump"))
        );

        let f = rewind();
        f.put("compare", "ahead\n");
        assert!(f.run(H, None).await.0.is_ok(), "a forward pin merges");
    }

    #[tokio::test]
    async fn the_base_branchs_gate_runs_in_the_checkout_and_a_red_gate_or_a_push_during_it_refuses()
    {
        use std::os::unix::fs::PermissionsExt;
        let gated = |exit: u8| {
            let f = Fake::new();
            f.put("fleet.toml", "[merge]\ngate = \"gate.sh\"\n");
            let co = f.dir.path().join("checkout");
            std::fs::create_dir(&co).unwrap();
            let g = co.join("gate.sh");
            std::fs::write(
                &g,
                format!(
                    "#!/bin/sh\necho \"gate $1 $AGENT_FLEET_MERGE_PR\"\ntouch {}/gated\nexit {exit}\n",
                    f.dir.path().display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&g, std::fs::Permissions::from_mode(0o755)).unwrap();
            f
        };
        let f = gated(0);
        let (r, said) = f.run(H, None).await;
        assert!(r.is_ok(), "{r:?}");
        assert!(
            said.iter().any(|l| l == &format!("gate {H} o/r#7")),
            "{said:?}"
        );
        assert!(
            !f.calls().contains("pr checks"),
            "a gate replaces the checks"
        );

        let f = gated(1);
        let (r, _) = f.run(H, None).await;
        assert!(format!("{:#}", r.unwrap_err()).contains("MERGE GATE FAILED"));
        assert!(!f.calls().contains("pr merge"));

        let f = gated(0);
        f.put("head2", &"9".repeat(40));
        let (r, _) = f.run(H, None).await;
        assert!(format!("{:#}", r.unwrap_err()).contains("head moved during the gate"));
        assert!(!f.calls().contains("pr merge"));
    }

    #[test]
    fn the_merge_table_is_read_and_the_rest_ignored() {
        let c = config_of("brief = \"b.md\"\n[merge]\ngate = \"scripts/merge-gate.sh\"\nhost = \"box\"\n[profiles.x]\nkind = \"claude\"\n").unwrap();
        assert_eq!(c.gate.as_deref(), Some("scripts/merge-gate.sh"));
        assert_eq!(c.host.as_deref(), Some("box"));
        assert_eq!(config_of("brief = \"b.md\"\n").unwrap(), Config::default());
        assert!(
            config_of("[merge]\ngates = \"x\"\n").is_err(),
            "unknown key"
        );
        assert!(
            config_of("[merge]\ngate = \"../x.sh\"\n").is_err(),
            "outside the repo"
        );
        assert!(
            config_of("[merge]\ngate = \"/bin/true\"\n").is_err(),
            "absolute"
        );
        let full = "brief = \"b.md\"\n[merge]\ngate = \"g.sh\"\n[profiles.x]\nkind = \"claude\"\n[[rules]]\nprofile = \"x\"\n";
        crate::spawn::FleetConfig::parse(full).expect("spawn reads a fleet.toml with [merge]");
    }

    #[test]
    fn only_a_full_sha_ready_as_the_last_comment_passes() {
        let ok = format!("Looks good.\n\n**`VERDICT: READY {H}`**\n\n");
        assert_eq!(verdict(Some(&ok), H), Ok(()));
        assert_eq!(
            verdict(Some(&format!("VERDICT: READY {}", H.to_uppercase())), H),
            Ok(())
        );
        for bad in [
            format!("VERDICT: READY {}", &H[..9]),
            format!("VERDICT: BLOCK {H} tests fail"),
            format!("VERDICT: READY {H}\nthanks"),
            format!("VERDICT: READY {H} and more"),
            "lgtm".into(),
        ] {
            assert!(verdict(Some(&bad), H).is_err(), "{bad}");
        }
        let other = "1111111111111111111111111111111111111111";
        assert!(verdict(Some(&format!("VERDICT: READY {other}")), H).is_err());
        assert!(verdict(None, H).is_err(), "no comments");
    }

    #[test]
    fn a_submodule_patch_gives_both_pins_and_a_file_patch_none() {
        let a = "5762a54520a35acae97c676c68c7306f9d27d330";
        let b = "b5500707cad7da0f8fb8736bcfa09a0206872d2c";
        let p = format!("@@ -1 +1 @@\n-Subproject commit {a}\n+Subproject commit {b}");
        assert_eq!(pin_change(&p), Some((a.into(), b.into())));
        assert_eq!(
            pin_change(&format!("@@ -0,0 +1 @@\n+Subproject commit {b}")),
            None
        );
        assert_eq!(pin_change("@@ -1 +1 @@\n-a\n+b"), None);
    }

    #[test]
    fn submodule_urls_on_github_name_their_repo() {
        for u in [
            "https://github.com/o/sub.git",
            "https://github.com/o/sub",
            "git@github.com:o/sub.git",
            "ssh://git@github.com/o/sub.git",
            "../sub.git",
        ] {
            assert_eq!(github_slug(u, "o/r").as_deref(), Some("o/sub"), "{u}");
        }
        assert_eq!(github_slug("https://gitlab.com/o/sub.git", "o/r"), None);
        assert_eq!(github_slug("https://github.com/o", "o/r"), None);
    }

    #[test]
    fn required_checks_pass_wait_or_fail() {
        let v = |b: &[(&str, &str)]| {
            json!(
                b.iter()
                    .map(|(n, b)| json!({"name": n, "bucket": b}))
                    .collect::<Vec<_>>()
            )
        };
        assert_eq!(
            checks(&v(&[("build", "pass"), ("lint", "skipping")])),
            Ok(None)
        );
        assert_eq!(
            checks(&v(&[("build", "pass"), ("mac", "pending")])),
            Ok(Some(vec!["mac".into()]))
        );
        assert_eq!(
            checks(&v(&[
                ("build", "fail"),
                ("mac", "pending"),
                ("x", "cancel")
            ])),
            Err(vec!["build".into(), "x".into()])
        );
        assert_eq!(
            checks(&v(&[])),
            Err(vec![]),
            "no required checks proves nothing"
        );
    }
}
