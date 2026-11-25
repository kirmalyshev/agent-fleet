//! `agent-fleet hook signal`: a harness's own hook events become `agent.signal` events —
//! the structured channel the fleet may act on. It must never fail or slow the program it
//! hooks: any error exits 0, the write goes through the normal client (it spools when the
//! hub is down), and the whole pass is capped at a second. `log` writes any other event as the
//! hooked agent the same way (`hook bash` logs `pane.refused` with it).

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client::Paths;
use crate::event::*;
use crate::host::herdr_call;
use crate::state::State;

/// The cap on one hook write, stdin read aside: a hook never slows the program.
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
    let Some(token) = cli_token(paths, token_file) else {
        return;
    };
    send(paths, hub, token, project, agent, program, signal, detail).await;
}

/// The `agent.signal` write itself: spooled first, so a hub that never answers inside the
/// budget costs the flush, never the signal — the next write replays it.
#[allow(clippy::too_many_arguments)]
async fn send(
    paths: &Paths,
    hub: Option<&str>,
    token: String,
    project: &str,
    agent: String,
    program: String,
    signal: &str,
    detail: Option<String>,
) {
    let body = Known::AgentSignal(AgentSignal {
        agent: agent.clone(),
        program,
        signal: signal.to_string(),
        detail,
    });
    write(paths, hub, token, project, agent, body).await;
}

/// Spool `body` as `agent`, then flush.
async fn write(
    paths: &Paths,
    hub: Option<&str>,
    token: String,
    project: &str,
    agent: String,
    body: Known,
) {
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
            id: agent,
        },
        project,
        body,
    );
    if client.queue(&e).is_ok() {
        let _ = client.flush_spool().await;
    }
}

/// Write `body` as the hooked agent, capped at `HOOK_BUDGET`. Never fails; outside a named
/// herdr session (`HERDR_SESSION` unset) it writes nothing, as `who` finds nobody.
pub async fn log(
    paths: &Paths,
    hub: Option<&str>,
    token_file: Option<&Path>,
    project: &str,
    host: &str,
    body: Known,
) {
    let _ = tokio::time::timeout(HOOK_BUDGET, async {
        let (Some((agent, _)), Some(token)) = (who(host).await, cli_token(paths, token_file))
        else {
            return;
        };
        write(paths, hub, token, project, agent, body).await;
    })
    .await;
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

/// Window in which a hook that found no unread tell is not asked again: tool calls come in
/// bursts, and a tell is minutes late at worst without it being seconds late with it.
const UNREAD_CACHE: Duration = Duration::from_secs(2);

/// `hook unread`'s JSON line for the harness: `me`'s unread tells as `additionalContext`, and the
/// `msg.read` for each (what `agent-fleet read` writes), so none is handed over twice. `None` with
/// none.
pub fn unread_output(
    s: &State,
    me: &Actor,
    project: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(String, Vec<Envelope<Known>>)> {
    let lines: Vec<String> = s
        .unread_for(&me.id, now)
        .iter()
        .map(|(id, m)| crate::ui::mail_line(id, m))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let text = format!(
        "agent-fleet: tells sent to you while you worked (now marked read; act on them):\n{}",
        lines.join("\n")
    );
    let out = json!({ "hookSpecificOutput": {
        "hookEventName": "PostToolUse", "additionalContext": text } });
    Some((out.to_string(), crate::read_acks(s, me, project, now)))
}

/// `hook unread`: one `/state` read from the hub, then the acks, then the tells. A tell is never
/// lost to a failed ack (it may show twice, never not at all). Silent (`None`) with no tells, a
/// hub that does not answer inside `HOOK_BUDGET`, or any error.
pub async fn unread_with(
    client: &crate::client::Client,
    me: &Actor,
    project: &str,
) -> Option<String> {
    let state = tokio::time::timeout(HOOK_BUDGET, client.state())
        .await
        .ok()?
        .ok()?;
    let (out, acks) = unread_output(&state, me, project, chrono::Utc::now())?;
    // Spooled first: an ack the hub does not take in time replays on the next write.
    if acks.iter().all(|a| client.queue(a).is_ok()) {
        let _ = tokio::time::timeout(HOOK_BUDGET, client.flush_spool()).await;
    }
    Some(out)
}

/// The `hook unread` entry. Only in a herdr pane whose agent herdr names; a nested `claude -p`
/// (`CLAUDECODE`) inside another program's pane takes nothing, as with `hook mail`.
pub async fn unread_hook(
    paths: &Paths,
    hub: Option<&str>,
    token_file: Option<&Path>,
    project: &str,
    host: &str,
) -> Option<String> {
    let stamp = paths.dir.join("unread.checked");
    if std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < UNREAD_CACHE)
    {
        return None;
    }
    let (agent, program) = tokio::time::timeout(HOOK_BUDGET, who(host)).await.ok()??;
    if std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty()) && program != "claude" {
        return None;
    }
    let token = cli_token(paths, token_file)?;
    let client = crate::client::Client::new(
        hub.map(str::to_string)
            .unwrap_or_else(|| paths.hub_url())
            .as_str(),
        token,
        paths.spool(),
    );
    let me = Actor {
        kind: ActorKind::Agent,
        id: agent,
    };
    let out = unread_with(&client, &me, project).await;
    if out.is_none() {
        let _ = std::fs::write(&stamp, "");
    }
    out
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
    // The list is oldest first; re-reporting a session refreshes its place, and the cap
    // always evicts the oldest, not whichever side the last write left at the front.
    sessions.retain(|s| s != session);
    sessions.push(session.to_string());
    let over = sessions.len().saturating_sub(1000);
    sessions.drain(..over);
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

    #[test]
    fn the_session_cap_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("cf.json");
        for i in 0..1005 {
            remember(&f, &format!("s{i}"));
        }
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let sessions: Vec<&str> = v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert_eq!(sessions.len(), 1000);
        assert_eq!(sessions[0], "s5", "the five oldest are evicted");
        assert_eq!(sessions[999], "s1004");
        // A re-reported session refreshes its place instead of duplicating.
        remember(&f, "s5");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let sessions: Vec<&str> = v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert_eq!(sessions.len(), 1000);
        assert_eq!(sessions[999], "s5");
    }

    #[tokio::test]
    async fn a_hung_hub_still_keeps_the_signal() {
        // A hub that accepts and never answers: the write budget ends, but the event was
        // spooled first — a timeout never loses a signal.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: dir.path().to_path_buf(),
        };
        let hang = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = format!("http://{}", hang.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((s, _)) = hang.accept().await {
                std::mem::forget(s);
            }
        });
        let timed_out = tokio::time::timeout(
            Duration::from_millis(300),
            send(
                &paths,
                Some(hub.as_str()),
                "tok".into(),
                "p",
                "agent:h/s/a".into(),
                "claude".into(),
                "turn_end",
                None,
            ),
        )
        .await;
        assert!(timed_out.is_err(), "the flush hangs past the budget");
        let spooled = std::fs::read_to_string(paths.spool()).unwrap();
        assert!(
            spooled.contains("agent.signal") && spooled.contains("agent:h/s/a"),
            "the signal is spooled, not lost: {spooled}"
        );
    }

    // ---- hook unread ----

    fn tell_to_b(text: &str) -> (Actor, Actor, Vec<Envelope<Known>>) {
        let a = Actor {
            kind: ActorKind::Agent,
            id: "agent:h/s/a".into(),
        };
        let b = Actor {
            kind: ActorKind::Agent,
            id: "agent:h/s/b".into(),
        };
        let e = Envelope::new(
            a.clone(),
            "p",
            Known::MsgSent(MsgSent {
                to: b.id.clone(),
                kind: "DECIDE".into(),
                reference: "r#1".into(),
                text: text.into(),
                closes: Vec::new(),
            }),
        );
        (a, b, vec![e])
    }

    fn fold(es: &[Envelope<Known>]) -> State {
        let mut s = State::default();
        for e in es {
            s.apply(&e.clone().into_event());
        }
        s
    }

    #[test]
    fn unread_output_is_post_tool_use_additional_context_with_the_acks() {
        let (_, b, es) = tell_to_b("push gated: wait for review");
        let (out, acks) = unread_output(&fold(&es), &b, "p", chrono::Utc::now()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let text = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("DECIDE") && text.contains("push gated: wait for review"),
            "{text}"
        );
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].re.as_deref(), Some(es[0].id.as_str()));
        assert_eq!(acks[0].actor.id, b.id);
    }

    #[test]
    fn unread_output_shows_a_tell_once() {
        let (_, b, es) = tell_to_b("x");
        let mut s = fold(&es);
        let (_, acks) = unread_output(&s, &b, "p", chrono::Utc::now()).unwrap();
        for a in acks {
            s.apply(&a.into_event());
        }
        assert!(unread_output(&s, &b, "p", chrono::Utc::now()).is_none());
    }

    #[test]
    fn unread_output_is_none_without_tells_for_that_agent() {
        let (a, _, es) = tell_to_b("x");
        assert!(unread_output(&State::default(), &a, "p", chrono::Utc::now()).is_none());
        // the sender has nothing unread either
        assert!(unread_output(&fold(&es), &a, "p", chrono::Utc::now()).is_none());
    }

    fn client(base: &str, dir: &Path) -> crate::client::Client {
        crate::client::Client::new(base, "tok".into(), dir.join("spool"))
    }

    #[tokio::test]
    async fn unread_with_a_dead_hub_is_silent_and_fast() {
        let dir = tempfile::tempdir().unwrap();
        let (_, b, _) = tell_to_b("x");
        let t = std::time::Instant::now();
        let out = unread_with(&client("http://127.0.0.1:1", dir.path()), &b, "p").await;
        assert!(out.is_none());
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    async fn unread_with_a_live_hub_hands_over_once_then_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let hub = crate::hub::Hub::new(crate::store::Store::memory().unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::hub::router(hub, "tok".into());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let c = client(&base, dir.path());
        let (_, b, es) = tell_to_b("gate: DECIDE");
        let none = unread_with(&c, &b, "p").await;
        assert!(none.is_none(), "no tells yet");
        c.write(&es[0]).await.unwrap();
        let t = std::time::Instant::now();
        let out = unread_with(&c, &b, "p").await.expect("the tell");
        assert!(out.contains("gate: DECIDE"));
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "{:?}",
            t.elapsed()
        );
        assert!(
            unread_with(&c, &b, "p").await.is_none(),
            "acked: shown once"
        );
    }
}
