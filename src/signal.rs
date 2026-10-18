//! `agent-fleet hook signal`: a harness's own hook events become `agent.signal` events —
//! the structured channel the fleet may act on. It must never fail or slow the program it
//! hooks: any error exits 0, the write goes through the normal client (it spools when the
//! hub is down), and the whole pass is capped at a second.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client::Paths;
use crate::event::*;
use crate::host::herdr_call;
use crate::state::State;

/// The cap on the whole hook, stdin read aside: a hook never slows the program.
const HOOK_BUDGET: Duration = Duration::from_secs(1);

/// The Claude Code statusline's context fullness that names `context_full`.
pub const CONTEXT_FULL_PCT: f64 = 90.0;

/// Map a harness hook's stdin JSON to `(signal, detail)`; `None` reports nothing.
/// Claude Code names the event `hook_event_name`; Devin CLI and Codex match where their
/// payloads share the shape.
pub fn map(v: &Value) -> Option<(String, Option<String>)> {
    let (sig, detail) = match v["hook_event_name"].as_str()? {
        "StopFailure" => {
            let sig = match v["error"].as_str().unwrap_or_default() {
                "rate_limit" => "rate_limit",
                "billing_error"
                | "account_on_hold"
                | "authentication_failed"
                | "oauth_org_not_allowed" => "account",
                _ => "error",
            };
            let detail = [
                v["error_details"].as_str().unwrap_or_default(),
                v["last_assistant_message"]
                    .as_str()
                    .and_then(|m| m.lines().next())
                    .unwrap_or_default(),
            ]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join(": ");
            (sig, detail)
        }
        "Notification" => (
            match v["notification_type"].as_str()? {
                "permission_prompt" => "permission",
                "idle_prompt" => "idle",
                "agent_needs_input" | "elicitation_dialog" | "elicitation_url_dialog" => "question",
                "quota_auto_resume_stale" | "quota_auto_resume_disabled" => "quota",
                "quota_auto_resume_fired" => "resumed",
                _ => return None,
            },
            v["message"].as_str().unwrap_or_default().to_string(),
        ),
        "PreCompact" if v["trigger"].as_str() == Some("auto") => ("compacting", String::new()),
        "Stop" => ("turn_end", String::new()),
        "UserPromptSubmit" => ("prompt", String::new()),
        _ => return None,
    };
    Some((
        sig.to_string(),
        (!detail.trim().is_empty()).then(|| detail.trim().to_string()),
    ))
}

/// Who the hooked process is: the herdr pane's agent id and program, exactly as `me()`
/// derives them. Outside herdr (`HERDR_ENV` unset) there is nobody to report — `None`.
async fn who(host: &str) -> Option<(String, String)> {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return None;
    }
    let (Ok(pane), Ok(session), Ok(sock)) = (
        std::env::var("HERDR_PANE_ID"),
        std::env::var("HERDR_SESSION"),
        std::env::var("HERDR_SOCKET_PATH"),
    ) else {
        return None;
    };
    let got = herdr_call(Path::new(&sock), "agent.get", json!({ "target": pane }))
        .await
        .ok()?;
    let a = &got["agent"];
    let name = a["name"].as_str().unwrap_or(&pane).to_string();
    let program = a["agent"].as_str().unwrap_or("?").to_string();
    Some((format!("agent:{host}/{session}/{name}"), program))
}

/// Write one `agent.signal` for the hooked agent. Best effort: every failure is swallowed.
async fn emit(
    paths: &Paths,
    hub: Option<&str>,
    token_file: Option<&Path>,
    project: &str,
    host: &str,
    signal: &str,
    detail: Option<String>,
) {
    let Some((agent, program)) = who(host).await else {
        return;
    };
    let token = cli_token(paths, token_file);
    let Some(token) = token else { return };
    let client = crate::client::Client::new(
        hub.map(str::to_string)
            .unwrap_or_else(|| paths.hub_url())
            .as_str(),
        token,
        paths.spool(),
    );
    let e = Envelope::new(
        Actor {
            kind: ActorKind::Agent,
            id: agent.clone(),
        },
        project,
        Known::AgentSignal(AgentSignal {
            agent,
            program,
            signal: signal.to_string(),
            detail,
        }),
    );
    let _ = client.write(&e).await;
}

fn cli_token(paths: &Paths, token_file: Option<&Path>) -> Option<String> {
    crate::client::read_token(&token_file.map_or_else(|| paths.token(), |p| p.to_path_buf())).ok()
}

/// The hook entry: stdin's JSON mapped and written as `agent.signal`. Never fails.
pub async fn hook(
    paths: &Paths,
    hub: Option<&str>,
    token_file: Option<&Path>,
    project: &str,
    host: &str,
    input: &str,
) {
    let Some((sig, detail)) = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| map(&v))
    else {
        return;
    };
    let _ = tokio::time::timeout(
        HOOK_BUDGET,
        emit(paths, hub, token_file, project, host, &sig, detail),
    )
    .await;
}

/// The statusline's own signal: `context_window.used_percentage` at CONTEXT_FULL_PCT or more,
/// once per Claude session (deduped in `file` — a session-id list next to
/// `claude-rate-limits.json`). Never fails.
pub async fn context_full(
    paths: &Paths,
    hub: Option<&str>,
    token_file: Option<&Path>,
    project: &str,
    host: &str,
    input: &Value,
) {
    let Some(pct) = input["context_window"]["used_percentage"].as_f64() else {
        return;
    };
    if pct < CONTEXT_FULL_PCT {
        return;
    }
    let Some(session) = input["session_id"].as_str() else {
        return;
    };
    let Some(file) = Paths::base()
        .ok()
        .map(|d| d.join("claude-context-full.json"))
    else {
        return;
    };
    if reported(&file, session) {
        return;
    }
    let _ = tokio::time::timeout(
        HOOK_BUDGET,
        emit(
            paths,
            hub,
            token_file,
            project,
            host,
            "context_full",
            Some(format!("context {:.0}% used", pct)),
        ),
    )
    .await;
    remember(&file, session);
}

/// `session` was already reported in `file` (a `{"sessions": […]}` list, capped at 1000).
fn reported(file: &Path, session: &str) -> bool {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["sessions"].as_array().cloned())
        .is_some_and(|s| s.iter().any(|x| x.as_str() == Some(session)))
}

fn remember(file: &Path, session: &str) {
    let mut sessions: Vec<String> = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v["sessions"].as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default();
    sessions.push(session.to_string());
    let sessions: Vec<String> = sessions.into_iter().rev().take(1000).collect();
    let body = json!({ "sessions": sessions }).to_string();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }
}

/// The stored signals for `waiting`-style views: `question` signals are questions for the
/// principal — (agent, since, text).
pub fn questions(s: &State) -> Vec<(&str, chrono::DateTime<chrono::Utc>, &str)> {
    s.signals
        .iter()
        .filter(|(id, _)| s.agents.contains_key(*id))
        .filter(|(_, sig)| sig.signal == "question")
        .map(|(id, sig)| (id.as_str(), sig.since, sig.detail.as_deref().unwrap_or("")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(input: Value) -> Option<String> {
        map(&input).map(|(s, _)| s)
    }

    #[test]
    fn the_hook_map_covers_every_documented_event() {
        let m = |name: &str, extra: Value| {
            let mut v = json!({ "hook_event_name": name });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            sig(v)
        };
        // StopFailure: the error kind picks the signal
        let sf = |error: &str| {
            m(
                "StopFailure",
                json!({ "error": error, "error_details": "d", "last_assistant_message": "l1\nl2" }),
            )
        };
        assert_eq!(sf("rate_limit").as_deref(), Some("rate_limit"));
        for e in [
            "billing_error",
            "account_on_hold",
            "authentication_failed",
            "oauth_org_not_allowed",
        ] {
            assert_eq!(sf(e).as_deref(), Some("account"), "{e}");
        }
        for e in ["overloaded", "server_error", "whatever_new"] {
            assert_eq!(sf(e).as_deref(), Some("error"), "{e}");
        }
        // detail = error_details + first line of last_assistant_message
        let v = json!({ "hook_event_name": "StopFailure", "error": "overloaded",
            "error_details": "HTTP 529", "last_assistant_message": "was writing\nrest" });
        let (_, d) = map(&v).unwrap();
        assert_eq!(d.as_deref(), Some("HTTP 529: was writing"));
        // Notification types
        let n = |t: &str| {
            m(
                "Notification",
                json!({ "notification_type": t, "message": "m" }),
            )
        };
        assert_eq!(n("permission_prompt").as_deref(), Some("permission"));
        assert_eq!(n("idle_prompt").as_deref(), Some("idle"));
        for t in [
            "agent_needs_input",
            "elicitation_dialog",
            "elicitation_url_dialog",
        ] {
            assert_eq!(n(t).as_deref(), Some("question"), "{t}");
        }
        assert_eq!(n("quota_auto_resume_stale").as_deref(), Some("quota"));
        assert_eq!(n("quota_auto_resume_disabled").as_deref(), Some("quota"));
        assert_eq!(n("quota_auto_resume_fired").as_deref(), Some("resumed"));
        assert_eq!(n("unrelated"), None);
        // question carries the message as detail
        let v = json!({ "hook_event_name": "Notification",
            "notification_type": "elicitation_dialog", "message": "which one?" });
        assert_eq!(map(&v).unwrap().1.as_deref(), Some("which one?"));
        // PreCompact only when automatic; the episode boundaries
        assert_eq!(
            m("PreCompact", json!({ "trigger": "auto" })).as_deref(),
            Some("compacting")
        );
        assert_eq!(m("PreCompact", json!({ "trigger": "manual" })), None);
        assert_eq!(m("Stop", json!({})).as_deref(), Some("turn_end"));
        assert_eq!(
            m("UserPromptSubmit", json!({ "prompt": "hi" })).as_deref(),
            Some("prompt")
        );
        // unknown events report nothing; garbage input is no event
        assert_eq!(m("SessionStart", json!({})), None);
        assert!(
            serde_json::from_str::<Value>("not json")
                .ok()
                .and_then(|v| map(&v))
                .is_none()
        );
    }

    #[tokio::test]
    async fn outside_herdr_the_hook_is_silent() {
        // No HERDR_* env (a plain shell, a CI run): no agent to name, no write — exit 0.
        unsafe { std::env::remove_var("HERDR_ENV") };
        assert!(who("h").await.is_none());
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: dir.path().to_path_buf(),
        };
        hook(
            &paths,
            None,
            None,
            "p",
            "h",
            r#"{"hook_event_name":"Stop"}"#,
        )
        .await;
    }

    #[test]
    fn reported_sessions_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("cf.json");
        assert!(!reported(&f, "s1"));
        remember(&f, "s1");
        assert!(reported(&f, "s1"));
        assert!(!reported(&f, "s2"));
    }
}
