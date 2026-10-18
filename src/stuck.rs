//! Why a stopped agent is stopped. The host reads the tail of its screen through herdr
//! (`agent.read`), `stuck_cause` names ONE cause from it, `check` records it as `agent.stuck`,
//! and `step` picks the single action a cause gets per episode — a `continue`, a key, a
//! handoff or a word to the lead — logged as events.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use crate::event::*;
use crate::host::{Host, herdr_call};
use crate::state::{Agent, Quota, State};

/// A stuck agent's cause: `detail` as the screen showed it, `until` when it lifts.
pub struct Cause {
    pub kind: StuckCause,
    pub detail: String,
    pub until: Option<DateTime<Utc>>,
}

impl Cause {
    fn simple(kind: StuckCause, detail: &str) -> Self {
        Self {
            kind,
            detail: detail.into(),
            until: None,
        }
    }
}

/// The wait between the Enter and the Escape + re-send for an unsent prompt.
pub const UNSENT_RETRY: Duration = Duration::seconds(60);
/// An out-of-quota agent whose reset is at most this far out waits it; further out its work
/// moves to the profile's `on_quota`.
pub const QUOTA_WAIT: Duration = Duration::minutes(30);
/// A rate-limit line without a number waits this long before the `continue`.
const RL_DEFAULT: Duration = Duration::minutes(5);

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The last `n` non-empty screen lines, joined for a one-line message.
fn last_n(s: &str, n: usize) -> String {
    s.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The "Press Enter to send queued" hint, on a UI chrome line — never in the agent's prose.
fn send_hint(screen: &str) -> bool {
    screen
        .lines()
        .any(|l| chrome(l) && l.to_lowercase().contains("press enter to send queued"))
}

/// The message waiting in a Devin input box, read off the screen: the box is the
/// rule-bounded block directly above the model footer (`… Context: …`), the message the
/// lines from `❭` on. `None` when no box can be located; `Some("")` for an empty box.
pub fn queued_text(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let rule = |l: &str| l.starts_with(&"─".repeat(10));
    let hold = ["Guide Devin while it works", "Ask Devin to build features"];
    let f = lines
        .iter()
        .rposition(|l| l.contains("Context: ") && !l.starts_with(char::is_whitespace))?;
    if f == 0 || !rule(lines[f - 1]) {
        return None;
    }
    let t = (0..f - 1).rev().find(|&i| rule(lines[i]))?;
    let body = &lines[t + 1..f - 1];
    let s = body.iter().position(|l| l.trim_start().starts_with('❭'))?;
    let first = body[s]
        .trim_start()
        .trim_start_matches('❭')
        .trim()
        .to_string();
    if first.starts_with("Press Enter to send queued") {
        // A busy turn holds its queued messages above the box as `○ ` lines; exactly one
        // is the text, several are ambiguous and are not re-sent.
        let mut ring = Vec::new();
        for l in lines[..t].iter().rev() {
            if let Some(m) = l.trim_start().strip_prefix("○ ") {
                ring.push(m.trim().to_string());
            } else if !l.trim().is_empty() {
                break;
            }
        }
        return if ring.len() == 1 {
            Some(ring.swap_remove(0))
        } else {
            Some(String::new())
        };
    }
    if !hold.iter().any(|h| first.starts_with(h)) {
        let mut parts = vec![first];
        for l in &body[s + 1..] {
            let l = l.trim();
            if !l.is_empty() {
                parts.push(l.to_string());
            }
        }
        return Some(parts.join(" "));
    }
    Some(String::new())
}

/// A UI chrome line — the program's own text. `⏺`, `⎿` and `│` lines are Claude's/Devin's
/// prose and tool output, `•` Codex's: a signal quoted there is the agent talking, not the
/// program's state, and never matches.
fn chrome(line: &str) -> bool {
    let l = line.trim_start();
    !(l.starts_with('⏺') || l.starts_with('⎿') || l.starts_with('│') || l.starts_with('•'))
}

/// "reset(s) in ~?N", "retry after N" and friends: the short wait a rate-limit line names.
/// Only a line that is itself about a limit — "git reset in 3 steps" is not one.
fn retry_in(line: &str) -> Option<Duration> {
    let low = line.to_lowercase();
    if !(low.contains("limit") || low.contains("retry") || low.contains("quota")) {
        return None;
    }
    for needle in ["reset", "retry after"] {
        let Some(pos) = low.find(needle) else {
            continue;
        };
        let rest = low[pos + needle.len()..]
            .trim_start_matches(['s', ' '])
            .trim_start();
        let rest = if needle == "reset" {
            let Some(r) = rest.strip_prefix("in") else {
                continue;
            };
            r.trim_start().trim_start_matches('~').trim_start()
        } else {
            rest
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(n) = digits.parse() else { continue };
        let unit = rest[digits.len()..].trim_start();
        return Some(match unit.chars().next() {
            Some('s') => Duration::seconds(n),
            Some('h') => Duration::hours(n),
            _ => Duration::minutes(n),
        });
    }
    None
}

/// Devin's footer `Context: N / M tokens (P%)`: the percent inside the parens.
fn devin_context_pct(line: &str) -> Option<f64> {
    let low = line.to_lowercase();
    let rest = &low[low.find("context:")?..];
    let t = rest.find("tokens")?;
    if !rest[..t].contains('/') {
        return None;
    }
    let p = rest[t..].find('(')? + t + 1;
    rest[p..].split('%').next()?.trim().parse().ok()
}

/// Claude's `Context left until auto-compact: P%`.
fn claude_context_pct(line: &str) -> Option<f64> {
    let low = line.to_lowercase();
    let rest = low.split("context left until auto-compact:").nth(1)?;
    let rest = rest.trim_start();
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    rest[digits.len()..]
        .trim_start()
        .starts_with('%')
        .then(|| digits.parse().ok())?
}

/// Codex's `P% context left` — a count DOWN, so small means full.
fn codex_context_pct(line: &str) -> Option<f64> {
    let low = line.to_lowercase();
    let (before, _) = low.split_once("% context left")?;
    let digits: Vec<char> = before
        .trim_end()
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.into_iter().rev().collect::<String>().parse().ok()
}

/// The line's context fullness, in the program's own anchored shape only: Devin's
/// `Context: N / M tokens (P%)` is full at ≥90%, Claude's `Context left until
/// auto-compact: P%` and Codex's `P% context left` at ≤10%.
fn context_full(program: Option<&str>, line: &str) -> bool {
    if !chrome(line) {
        return false;
    }
    let devin = devin_context_pct(line).is_some_and(|p| p >= 90.0);
    let claude = claude_context_pct(line).is_some_and(|p| p <= 10.0);
    let codex = codex_context_pct(line).is_some_and(|p| p <= 10.0);
    match program {
        Some("devin") => devin,
        Some("claude") => claude,
        Some("codex") => codex,
        _ => devin || claude || codex,
    }
}

/// The line is the program's own out-of-quota banner, as it appears in its UI chrome.
fn quota_on_screen(program: Option<&str>, line: &str) -> bool {
    if !chrome(line) {
        return false;
    }
    let pats: &[&str] = match program {
        Some("devin") => &["quota exhausted"],
        Some("claude") => &["usage limit", "limit reached"],
        Some("codex") => &["usage limit"],
        _ => &["quota exhausted", "usage limit", "limit reached"],
    };
    let low = line.to_lowercase();
    pats.iter().any(|p| low.contains(p))
}

/// ONE cause for a stopped agent, read off `screen_tail` — only the program's own UI
/// lines carry a signal; its prose (`⏺`/`⎿`/`│`/`•` lines) never does. `None` means the
/// screen names nothing the host recognizes; `cause_of` maps that (and herdr's `blocked`)
/// to the rest.
pub fn stuck_cause(
    program: Option<&str>,
    screen_tail: &str,
    quota: Option<&Quota>,
    now: DateTime<Utc>,
) -> Option<Cause> {
    let t6 = tail_lines(screen_tail, 6);
    let t14 = tail_lines(screen_tail, 14);
    if t6.lines().any(|l| {
        chrome(l) && {
            let low = l.to_lowercase();
            low.contains("press enter to send queued")
                || (low.contains("never put") && low.contains("secret") && low.contains("anywhere"))
        }
    }) || queued_text(screen_tail).is_some_and(|t| !t.is_empty())
    {
        return Some(Cause::simple(
            StuckCause::Unsent,
            "a prompt waits in the input box",
        ));
    }
    let rl = t14.lines().rev().filter(|l| chrome(l)).find_map(|l| {
        let low = l.to_lowercase();
        let wait = retry_in(l);
        (low.contains("rate limit") || low.contains("retry after") || wait.is_some())
            .then(|| wait.unwrap_or(RL_DEFAULT))
    });
    if let Some(wait) = rl {
        return Some(Cause {
            kind: StuckCause::RateLimit,
            detail: "a rate limit is on screen".into(),
            until: Some(now + wait),
        });
    }
    // `quota` is the program saying it on screen — `agent.quota` only lends the spent
    // window's `resets_at`. An idle agent on an out-of-quota account is just idle.
    if t14.lines().any(|l| quota_on_screen(program, l)) {
        let until = quota.and_then(|q| {
            q.windows
                .iter()
                .filter(|w| w.used_pct >= 100.0)
                .filter_map(|w| w.resets_at)
                .filter(|r| *r > now)
                .min()
        });
        return Some(Cause {
            kind: StuckCause::Quota,
            detail: "the program says its account is out of quota".into(),
            until,
        });
    }
    if let Some(line) = t14.lines().rev().find(|l| context_full(program, l)) {
        return Some(Cause {
            kind: StuckCause::ContextFull,
            detail: format!("context nearly full: {}", line.trim()),
            until: None,
        });
    }
    if let Some(line) = t14.lines().rev().find(|l| {
        chrome(l) && {
            let low = l.to_lowercase();
            low.contains("something went wrong")
                || low.contains("send a message to continue")
                || low.contains("api error")
        }
    }) {
        return Some(Cause {
            kind: StuckCause::Error,
            detail: format!("error on screen: {}", line.trim()),
            until: None,
        });
    }
    None
}

/// The cause for a stopped agent: an intentional hold first, then herdr's `blocked` (a
/// permission or trust prompt) and a legitimate resource wait, then a recognized screen,
/// and `waiting` once the agent has worked, `unknown` before that.
pub fn cause_of(s: &State, id: &str, a: &Agent, detected: Option<Cause>) -> Cause {
    if let Some(detail) = crate::doctor::held(s, id) {
        return Cause {
            kind: StuckCause::Held,
            detail,
            until: None,
        };
    }
    if a.state == AgentState::Blocked {
        return Cause::simple(StuckCause::Permission, "a permission or trust prompt waits");
    }
    if let Some((detail, _, _)) = crate::doctor::resource_wait(s, a) {
        return Cause {
            kind: StuckCause::Resource,
            detail,
            until: None,
        };
    }
    detected.unwrap_or_else(|| {
        if matches!(a.state, AgentState::Idle | AgentState::Done) && a.worked.is_some() {
            Cause::simple(StuckCause::Waiting, "idle, nothing blocking on screen")
        } else {
            Cause::simple(StuckCause::Unknown, "no known cause on screen")
        }
    })
}

// ---------- the action, once per episode ----------

/// Per-agent episode state: the stretch since the agent's current state began, reset when
/// the state or the cause changes. Each step field makes one action once-per-episode.
#[derive(Debug, Default)]
pub struct Episode {
    key: i64,
    cause: Option<StuckCause>,
    step: u8,
    at: Option<DateTime<Utc>>,
    fired: bool,
    continued: bool,
}

impl Episode {
    fn for_cause(&mut self, since: DateTime<Utc>, kind: StuckCause) -> &mut Self {
        if self.key != since.timestamp() || self.cause != Some(kind) {
            *self = Self {
                key: since.timestamp(),
                cause: Some(kind),
                ..Self::default()
            };
        }
        self
    }
}

/// What the host does for a cause; every variant lands in the log as an event or a key.
pub enum Act {
    /// `continue` once, as a doorbell to the agent itself.
    Continue,
    /// One Enter, to ship the prompt sitting in the box.
    Enter,
    /// One Escape and the queued text re-sent.
    Resend(String),
    /// A `msg.sent` to the lead (`to`), `kind` the cause's name.
    Tell {
        to: String,
        kind: String,
        text: String,
    },
    /// Hand the agent's work to profile `to` (a `spawn.requested` with `replaces`).
    Handoff { to: String, extra: String },
}

fn tell(s: &State, id: &str, kind: StuckCause, text: String) -> Option<Act> {
    let to = crate::doctor::escalate(s, id)?;
    Some(Act::Tell {
        to: to.to_string(),
        kind: kind.name().to_uppercase(),
        text,
    })
}

/// The facts `step` reads for one pass: the agent, its screen and the caller's lookups —
/// `handoff` is the profile the agent may be handed to (`on_quota` for `quota`, `fallback`
/// for `context_full`), `continued_before` a `CONTINUE` doorbell for `error` in the log.
pub struct Ctx<'a> {
    pub s: &'a State,
    pub id: &'a str,
    pub a: &'a Agent,
    pub screen: &'a str,
    pub now: DateTime<Utc>,
    pub handoff: Option<&'a str>,
    pub continued_before: bool,
}

/// The one action a cause earns this pass, or `None`.
pub fn step(c: &Ctx, cause: &Cause, ep: &mut Episode) -> Option<Act> {
    let (s, id, a, screen, now) = (c.s, c.id, c.a, c.screen, c.now);
    match cause.kind {
        StuckCause::Quota => {
            let far = cause.until.is_none_or(|u| u - now > QUOTA_WAIT);
            if far {
                if ep.fired {
                    return None;
                }
                ep.fired = true;
                let until = cause
                    .until
                    .map(|u| crate::ui::at(now, u))
                    .unwrap_or_else(|| "an unknown time".into());
                return match c.handoff {
                    Some(to) => Some(Act::Handoff {
                        to: to.to_string(),
                        extra: format!(
                            "out of {} quota until {until}",
                            a.kind.as_deref().unwrap_or("its program's")
                        ),
                    }),
                    None => tell(
                        s,
                        id,
                        cause.kind,
                        format!(
                            "{} is out of quota until {until}; no fallback has quota",
                            crate::host::short(id),
                        ),
                    ),
                };
            }
            if !ep.fired {
                ep.fired = true;
                return tell(
                    s,
                    id,
                    cause.kind,
                    format!(
                        "{} is out of quota; it waits until {}",
                        crate::host::short(id),
                        crate::ui::at(now, cause.until?)
                    ),
                );
            }
            if !ep.continued && cause.until.is_some_and(|u| now >= u) {
                ep.continued = true;
                return Some(Act::Continue);
            }
            None
        }
        StuckCause::RateLimit => {
            if !ep.continued && cause.until.is_none_or(|u| now >= u) {
                ep.continued = true;
                return Some(Act::Continue);
            }
            None
        }
        StuckCause::Unsent => {
            // Like eait-watch: Enter only while the "Press Enter to send queued" hint is
            // on screen; Escape + re-send only once it is gone, for a queued text we can
            // name — never behind a running turn, and never while a doorbell to this
            // agent is still undelivered.
            let resend = || -> Option<Act> {
                if a.state == AgentState::Working || s.msgs.values().any(|m| m.to == id) {
                    return None;
                }
                let t = queued_text(screen).unwrap_or_default();
                (!t.is_empty()).then(|| Act::Resend(t))
            };
            let held = |s: &State, why: &str| {
                tell(
                    s,
                    id,
                    cause.kind,
                    format!(
                        "{} holds a prompt in its input box; nothing re-sent{why}",
                        crate::host::short(id)
                    ),
                )
            };
            let why = if a.state == AgentState::Working {
                " (it is behind a running turn)"
            } else if s.msgs.values().any(|m| m.to == id) {
                " (a doorbell to it is still pending)"
            } else {
                ""
            };
            match ep.step {
                0 => {
                    ep.at = Some(now);
                    if send_hint(screen) {
                        ep.step = 1;
                        Some(Act::Enter)
                    } else {
                        ep.step = 2;
                        resend().or_else(|| held(s, why))
                    }
                }
                1 if ep.at.is_some_and(|t| now - t >= UNSENT_RETRY) => {
                    ep.step = 2;
                    if send_hint(screen) {
                        tell(
                            s,
                            id,
                            cause.kind,
                            format!(
                                "{} still shows the send-queued hint after Enter",
                                crate::host::short(id)
                            ),
                        )
                    } else {
                        resend().or_else(|| held(s, why))
                    }
                }
                2 => {
                    ep.step = 3;
                    tell(
                        s,
                        id,
                        cause.kind,
                        format!(
                            "{} still holds a prompt in its input box after Enter and a re-send",
                            crate::host::short(id)
                        ),
                    )
                }
                _ => None,
            }
        }
        StuckCause::ContextFull => {
            if ep.fired {
                return None;
            }
            ep.fired = true;
            match c.handoff {
                Some(to) => Some(Act::Handoff {
                    to: to.to_string(),
                    extra: "its context is nearly full — read git status, git log and any STATE.md first".into(),
                }),
                None => tell(
                    s,
                    id,
                    cause.kind,
                    format!(
                        "{} is out of context and has no fallback: {}",
                        crate::host::short(id),
                        cause.detail
                    ),
                ),
            }
        }
        StuckCause::Error => {
            if ep.fired {
                return None;
            }
            ep.fired = true;
            if c.continued_before {
                return tell(
                    s,
                    id,
                    cause.kind,
                    format!(
                        "{} errored again: {}",
                        crate::host::short(id),
                        last_n(screen, 5)
                    ),
                );
            }
            Some(Act::Continue)
        }
        StuckCause::Resource => {
            if ep.fired {
                return None;
            }
            let (_, since, eta) = crate::doctor::resource_wait(s, a)?;
            // the wait is overdue once it beats 2× the holder's stated ETA or 60 min
            let waited = now - since;
            if !(waited > Duration::minutes(60) || eta.is_some_and(|e| waited > e * 2)) {
                return None;
            }
            ep.fired = true;
            tell(
                s,
                id,
                cause.kind,
                format!("{} {}", crate::host::short(id), cause.detail),
            )
        }
        StuckCause::Permission | StuckCause::Waiting | StuckCause::Unknown | StuckCause::Held => {
            None
        }
    }
}

// ---------- the host side ----------

/// The pushes (`HUNG`/`STALLED`) already reported and each agent's episode state.
#[derive(Default)]
pub struct Seen {
    pub reported: HashSet<String>,
    pub episodes: HashMap<String, Episode>,
}

/// The agent's visible screen, or `None` when herdr cannot read it.
async fn screen(host: &Host, id: &str) -> Option<String> {
    let name = crate::host::short(id);
    herdr_call(
        &host.cfg.herdr_socket,
        "agent.read",
        json!({ "target": name, "source": "visible", "strip_ansi": true }),
    )
    .await
    .ok()?["text"]
        .as_str()
        .map(str::to_string)
}

/// `minute` precision is enough for `until`: a countdown re-parses to the same instant.
fn minute(t: Option<DateTime<Utc>>) -> Option<i64> {
    t.map(|t| t.timestamp() / 60)
}

/// The profile `id` may be handed to for `kind`, when this host can use it. `quota` reads
/// `on_quota` and refuses a profile whose kind is the agent's own — a haiku shares the
/// claude account, so a claude agent is never handed to it here — or out on this host;
/// `context_full` reads `fallback`. `None` while a replacement is already open.
fn handoff_target(
    host: &Host,
    s: &State,
    id: &str,
    kind: StuckCause,
    now: DateTime<Utc>,
) -> Option<String> {
    let sp = s.spawn_of(id)?;
    let checkout = host.cfg.checkouts.get(&sp.request.repo)?;
    let (cfg, _) = crate::spawn::load(checkout).ok()?;
    let prof = cfg.profiles.get(&sp.request.profile)?;
    let to = match kind {
        StuckCause::Quota => prof.on_quota.as_deref()?,
        _ => prof.fallback.as_deref()?,
    };
    let target = cfg.profiles.get(to)?;
    if kind == StuckCause::Quota {
        let mine = s
            .agents
            .get(id)
            .and_then(|a| a.kind.as_deref())
            .unwrap_or(&prof.kind);
        if target.kind == mine
            || s.quotas
                .get(&format!("{}:{}", host.cfg.host, target.kind))
                .is_some_and(|q| q.exhausted_at(now))
        {
            return None;
        }
    }
    Some(to.to_string())
}

/// Read the agent's screen, record `agent.stuck` when the cause appears or changes (and
/// once with none when it works again), and run the cause's once-per-episode action.
/// Returns the cause for the caller's report — `None` when the agent is not stuck.
pub async fn check(
    host: &Arc<Host>,
    s: &State,
    events: &[Event],
    eps: &mut HashMap<String, Episode>,
    handed: &HashSet<String>,
    id: &str,
    a: &Agent,
    now: DateTime<Utc>,
) -> Option<(Cause, String)> {
    let eligible = matches!(a.state, AgentState::Idle | AgentState::Blocked)
        || (a.state == AgentState::Working && crate::doctor::hung(s, a, now).is_some());
    if !eligible {
        if s.stuck.contains_key(id) {
            host.publish(host.presence(Known::AgentStuck(AgentStuck {
                agent: id.to_string(),
                cause: None,
                detail: "back at work".into(),
                until: None,
            })))
            .await;
        }
        eps.remove(id);
        return None;
    }
    let screen = screen(host, id).await.unwrap_or_default();
    let quota = a
        .kind
        .as_deref()
        .and_then(|k| s.quotas.get(&format!("{}:{k}", host.cfg.host)));
    let mut cause = cause_of(
        s,
        id,
        a,
        stuck_cause(a.kind.as_deref(), &screen, quota, now),
    );
    // The deadline is first-seen: while the cause stands, `s.stuck`'s `until` wins — a
    // recomputed countdown would republish `agent.stuck` every pass and push the wait out
    // forever (or lose it on a restart and turn a near quota wait into a handoff).
    if let Some(prev) = s.stuck.get(id)
        && prev.cause == cause.kind
    {
        cause.until = prev.until.or(cause.until);
    }
    if s.stuck
        .get(id)
        .is_none_or(|w| w.cause != cause.kind || minute(w.until) != minute(cause.until))
    {
        host.publish(host.presence(Known::AgentStuck(AgentStuck {
            agent: id.to_string(),
            cause: Some(cause.kind),
            detail: cause.detail.clone(),
            until: cause.until,
        })))
        .await;
    }
    let tail = last_n(&screen, 5);
    // Never act on an agent whose work is finished.
    let closed = s
        .tasks
        .values()
        .any(|t| t.owner.as_deref() == Some(id) && t.state.is_terminal())
        && !s
            .tasks
            .values()
            .any(|t| t.owner.as_deref() == Some(id) && !t.state.is_terminal());
    // An agent `handoffs` just handed this pass is busy: its `spawn.requested` has not
    // folded back yet, so `replacement_open` alone would let a cause hand it off twice.
    let busy = handed.contains(id)
        || (matches!(cause.kind, StuckCause::Quota | StuckCause::ContextFull)
            && crate::spawn::replacement_open(s, id));
    // A grace stretch: the cause is classified and recorded from the first pass, but the
    // host acts or tells only once the agent has sat in this state for --stall-min — a
    // human half-typing a prompt is not "unsent" before that.
    if closed || busy || now - a.since < Duration::minutes(host.cfg.stall_min as i64) {
        return Some((cause, tail));
    }
    let handoff = match cause.kind {
        StuckCause::Quota | StuckCause::ContextFull => handoff_target(host, s, id, cause.kind, now),
        _ => None,
    };
    let continued_before = events.iter().any(|e| {
        matches!(&e.body,
            Body::Known(Known::MsgSent(m))
                if m.to == id && m.kind == "CONTINUE" && m.reference == "error")
    });
    let ep = eps
        .entry(id.to_string())
        .or_default()
        .for_cause(a.since, cause.kind);
    if let Some(act) = step(
        &Ctx {
            s,
            id,
            a,
            screen: &screen,
            now,
            handoff: handoff.as_deref(),
            continued_before,
        },
        &cause,
        ep,
    ) && let Some(act) = downgrade(host.cfg.stuck_act, s, id, &cause, act)
    {
        run(host, s, events, id, a, &cause, act).await;
    }
    Some((cause, tail))
}

/// With `--stuck-act` off the host never types into an agent and never hands its work off:
/// each would-be action becomes one tell to the lead of what it would have done. `None`
/// when there is nobody to tell.
fn downgrade(act_ok: bool, s: &State, id: &str, cause: &Cause, act: Act) -> Option<Act> {
    if act_ok {
        return Some(act);
    }
    let name = crate::host::short(id);
    let text = match act {
        Act::Tell { .. } => return Some(act),
        Act::Handoff { to, extra } => format!("{name} would be handed to {to} ({extra})"),
        Act::Continue => format!("{name} would get a `continue` ({})", cause.detail),
        Act::Enter | Act::Resend(_) => format!("{name} holds a prompt in its input box"),
    };
    tell(
        s,
        id,
        cause.kind,
        format!("{text} — stuck actions are off (--stuck-act)"),
    )
}

async fn run(
    host: &Arc<Host>,
    s: &State,
    events: &[Event],
    id: &str,
    a: &Agent,
    cause: &Cause,
    act: Act,
) {
    let name = crate::host::short(id);
    let sock = &host.cfg.herdr_socket;
    match act {
        Act::Continue => {
            host.publish(host.presence(Known::MsgSent(MsgSent {
                to: id.to_string(),
                kind: "CONTINUE".into(),
                reference: cause.kind.name().into(),
                text: "continue where you stopped".into(),
            })))
            .await;
        }
        Act::Enter => {
            let _ = herdr_call(
                sock,
                "agent.send_keys",
                json!({ "target": name, "keys": ["enter"] }),
            )
            .await;
        }
        Act::Resend(text) => {
            let _ = herdr_call(
                sock,
                "agent.send_keys",
                json!({ "target": name, "keys": ["escape"] }),
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let _ = herdr_call(
                sock,
                "agent.prompt",
                json!({ "target": name, "text": text }),
            )
            .await;
        }
        Act::Tell { to, kind, text } => {
            // The log is the dedupe across restarts: one tell of this kind per episode.
            if events.iter().any(|e| {
                e.ts > a.since
                    && matches!(&e.body,
                        Body::Known(Known::MsgSent(m))
                            if m.kind == kind && m.reference == name && m.to == to)
            }) {
                return;
            }
            host.publish(host.presence(Known::MsgSent(MsgSent {
                to,
                kind,
                reference: name.to_string(),
                text,
            })))
            .await;
        }
        Act::Handoff { to, extra } => {
            if let Some(sp) = s.spawn_of(id)
                && let Err(e) = crate::spawn::hand_off(host, s, sp, &to, &extra).await
            {
                eprintln!(
                    "agent-fleet host: handoff for {id} on {}: {e:#}",
                    cause.kind.name()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Quota;

    fn agent(state: AgentState, worked: bool) -> Agent {
        let now = Utc::now();
        Agent {
            host: "h".into(),
            kind: Some("devin".into()),
            pane: None,
            state,
            since: now,
            worked: worked.then_some(now),
            usage: None,
        }
    }

    fn quota_out(until: Option<DateTime<Utc>>) -> Quota {
        Quota {
            windows: vec![crate::event::QuotaWindow {
                name: "5h".into(),
                used_pct: 100.0,
                resets_at: until,
            }],
            plan: None,
            exhausted: true,
            read_at: Utc::now(),
            ts: Utc::now(),
        }
    }

    fn kind(c: Option<Cause>) -> Option<StuckCause> {
        c.map(|c| c.kind)
    }

    #[test]
    fn every_cause_is_read_off_a_fixture_screen() {
        let now = Utc::now();
        let later = now + Duration::hours(2);
        // quota: the program says so on screen; `agent.quota` only lends `until` —
        // the account being out alone is no cause
        let c = stuck_cause(
            Some("devin"),
            "Quota exhausted — come back later",
            Some(&quota_out(Some(later))),
            now,
        )
        .unwrap();
        assert_eq!((c.kind, c.until), (StuckCause::Quota, Some(later)));
        assert!(stuck_cause(Some("devin"), "", Some(&quota_out(Some(later))), now).is_none());
        // … and its own screen text alone names it too
        let c = stuck_cause(
            Some("devin"),
            "Quota exhausted — come back later",
            None,
            now,
        )
        .unwrap();
        assert_eq!(c.kind, StuckCause::Quota);
        for (prog, screen) in [
            ("devin", "Quota exhausted — come back later"),
            (
                "claude",
                "You've hit your usage limit. Your limit reached resets at 3pm",
            ),
            ("codex", "You have hit your usage limit"),
        ] {
            let c = stuck_cause(Some(prog), screen, None, now).unwrap();
            assert_eq!(c.kind, StuckCause::Quota, "{prog}: {screen}");
        }
        // rate_limit: a short retry window on screen
        let c = stuck_cause(
            Some("devin"),
            "Rate limit reached, resets in ~12 minutes",
            None,
            now,
        )
        .unwrap();
        assert_eq!(c.kind, StuckCause::RateLimit);
        assert_eq!(c.until, Some(now + Duration::minutes(12)));
        let c = stuck_cause(
            Some("claude"),
            "429: rate limit, retry after 30s",
            None,
            now,
        )
        .unwrap();
        assert_eq!(c.kind, StuckCause::RateLimit);
        // unsent: the hint, or a pasted brief in the box
        let c = stuck_cause(Some("devin"), "Press Enter to send queued", None, now).unwrap();
        assert_eq!(c.kind, StuckCause::Unsent);
        let devin_box = "output\n──────────────\n❭ fix the login button\n──────────────\nSWE-2 High · Context: 10,000 / 200,000 tokens (5%)\n";
        let c = stuck_cause(Some("devin"), devin_box, None, now).unwrap();
        assert_eq!(c.kind, StuckCause::Unsent);
        assert_eq!(
            queued_text(devin_box).as_deref(),
            Some("fix the login button")
        );
        // context_full
        let full = "SWE-2 High · Context: 190,000 / 200,000 tokens (95%)\n";
        let c = stuck_cause(Some("devin"), full, None, now).unwrap();
        assert_eq!(c.kind, StuckCause::ContextFull);
        let c = stuck_cause(
            Some("claude"),
            "Context left until auto-compact: 8%",
            None,
            now,
        )
        .unwrap();
        assert_eq!(c.kind, StuckCause::ContextFull);
        let c = stuck_cause(Some("codex"), "8% context left", None, now).unwrap();
        assert_eq!(c.kind, StuckCause::ContextFull);
        // error
        for screen in [
            "Something went wrong. Try again.",
            "Send a message to continue",
            "API Error: 500 Internal Server Error",
        ] {
            let c = stuck_cause(Some("devin"), screen, None, now).unwrap();
            assert_eq!(c.kind, StuckCause::Error, "{screen}");
        }
        // nothing recognized → None; cause_of maps the rest
        assert!(stuck_cause(Some("devin"), "all quiet", None, now).is_none());
        assert_eq!(
            cause_of(
                &State::default(),
                "agent:h/s/a",
                &agent(AgentState::Blocked, false),
                None
            )
            .kind,
            StuckCause::Permission
        );
        assert_eq!(
            cause_of(
                &State::default(),
                "agent:h/s/a",
                &agent(AgentState::Idle, true),
                None
            )
            .kind,
            StuckCause::Waiting
        );
        assert_eq!(
            cause_of(
                &State::default(),
                "agent:h/s/a",
                &agent(AgentState::Idle, false),
                None
            )
            .kind,
            StuckCause::Unknown
        );
        assert_eq!(
            cause_of(
                &State::default(),
                "agent:h/s/a",
                &agent(AgentState::Working, true),
                None
            )
            .kind,
            StuckCause::Unknown
        );
    }

    #[test]
    fn signals_come_from_chrome_lines_never_the_agents_prose() {
        let now = Utc::now();
        // The agent's own prose and tool output (`⏺`, `│`, `⎿`, `•` lines) can quote a
        // banner without the program meaning it — none of these is a cause.
        for (prog, screen) in [
            (
                "claude",
                "⏺ I updated the React context so it follows the auth flow.",
            ),
            ("claude", "⏺ The context now allows a null user"),
            ("claude", "⏺ See the context handling at line 120"),
            ("codex", "• 100% context left · ? for shortcuts"),
            ("codex", "• 96% context left"),
            ("claude", "⏺ Added a usage limit check"),
            ("devin", "⏺ the \"Quota exhausted\" banner"),
            ("claude", "⏺ Added rate limit middleware"),
            ("devin", "⏺ Fixed the API error toast"),
            ("devin", "⏺ shows \"Something went wrong\""),
            ("claude", "  ⎿  Something went wrong"),
            ("devin", "│ usage limit"),
            ("devin", "git reset in 3 steps"),
        ] {
            assert!(
                stuck_cause(Some(prog), screen, None, now).is_none(),
                "{prog}: {screen}"
            );
        }
        // And the real chrome lines still classify.
        assert_eq!(
            kind(stuck_cause(Some("codex"), "8% context left", None, now)),
            Some(StuckCause::ContextFull)
        );
        assert_eq!(
            kind(stuck_cause(
                Some("claude"),
                "Context left until auto-compact: 8%",
                None,
                now
            )),
            Some(StuckCause::ContextFull)
        );
        // A codex count-down footer is not full at 96 or 100.
        assert!(stuck_cause(Some("codex"), "96% context left", None, now).is_none());
        assert!(
            stuck_cause(
                Some("codex"),
                "100% context left · ? for shortcuts",
                None,
                now
            )
            .is_none()
        );
    }

    #[test]
    fn the_screen_decides_and_unsent_trumps_noise() {
        let now = Utc::now();
        let c = stuck_cause(
            Some("devin"),
            "Press Enter to send queued\nresets in 3 min",
            Some(&quota_out(None)),
            now,
        )
        .unwrap();
        assert_eq!(c.kind, StuckCause::Unsent);
        let c = stuck_cause(
            Some("devin"),
            "resets in 3 min\nPress Enter to send queued",
            None,
            now,
        )
        .unwrap();
        assert_eq!(c.kind, StuckCause::Unsent);
        assert_eq!(
            kind(stuck_cause(Some("claude"), "usage limit", None, now)),
            Some(StuckCause::Quota)
        );
    }

    #[test]
    fn the_input_box_holds_the_queued_and_the_pasted() {
        let screen = "○ do the thing\n──────────────\n❭ Press Enter to send queued\n──────────────\nSWE-2 High · Context: 1,000 / 200,000 tokens (1%)\n";
        assert_eq!(queued_text(screen).as_deref(), Some("do the thing"));
        let two = "○ one\n○ two\n──────────────\n❭ Press Enter to send queued\n──────────────\nSWE-2 High · Context: 1,000 / 200,000 tokens (1%)\n";
        assert_eq!(queued_text(two).as_deref(), Some(""));
        let empty = "──────────────\n❭ Guide Devin while it works\n──────────────\nSWE-2 High · Context: 1,000 / 200,000 tokens (1%)\n";
        assert_eq!(queued_text(empty).as_deref(), Some(""));
        assert!(queued_text("no box here").is_none());
    }

    fn cause(k: StuckCause, until: Option<DateTime<Utc>>) -> Cause {
        Cause {
            kind: k,
            detail: "d".into(),
            until,
        }
    }

    fn state_with_lead() -> State {
        let mut s = State::default();
        s.roles.insert(
            "agent:h/s/a".into(),
            crate::state::Role {
                level: Level::Implementer,
                lead: Some("agent:h/s/lead".into()),
                domain: None,
                by: "agent:h/s/x".into(),
                since: Utc::now(),
            },
        );
        s
    }

    #[test]
    fn the_policy_acts_once_per_episode_per_cause() {
        let now = Utc::now();
        let s = state_with_lead();
        let a = agent(AgentState::Idle, true);
        let id = "agent:h/s/a";
        let ctx = |screen, now, handoff, cont| Ctx {
            s: &s,
            id,
            a: &a,
            screen,
            now,
            handoff,
            continued_before: cont,
        };
        let mut ep = Episode::default();
        let ep = ep.for_cause(a.since, StuckCause::Quota);
        // far-out quota with a fallback that has quota → handoff
        let far = cause(StuckCause::Quota, Some(now + Duration::hours(2)));
        assert!(matches!(
            step(&ctx("", now, Some("haiku"), false), &far, ep),
            Some(Act::Handoff { .. })
        ));
        // a second pass in the same episode does nothing
        assert!(step(&ctx("", now, Some("haiku"), false), &far, ep).is_none());
        // the fallback's kind is out → tell the lead
        let mut ep2 = Episode::default();
        let ep2 = ep2.for_cause(a.since, StuckCause::Quota);
        match step(&ctx("", now, None, false), &far, ep2) {
            Some(Act::Tell { to, kind, text }) => {
                assert_eq!((to.as_str(), kind.as_str()), ("agent:h/s/lead", "QUOTA"));
                assert!(text.contains("no fallback has quota"), "{text}");
            }
            x => panic!("{x:?} is no tell"),
        }
        // quota near: tell once, then continue once after until
        let mut ep3 = Episode::default();
        let ep3 = ep3.for_cause(a.since, StuckCause::Quota);
        let near = cause(StuckCause::Quota, Some(now + Duration::minutes(10)));
        assert!(matches!(
            step(&ctx("", now, None, false), &near, ep3),
            Some(Act::Tell { .. })
        ));
        let past = now + Duration::minutes(11);
        assert!(matches!(
            step(&ctx("", past, None, false), &near, ep3),
            Some(Act::Continue)
        ));
        assert!(step(&ctx("", past, None, false), &near, ep3).is_none());
        // rate limit: nothing before until, continue once after
        let mut ep4 = Episode::default();
        let ep4 = ep4.for_cause(a.since, StuckCause::RateLimit);
        let rl = cause(StuckCause::RateLimit, Some(now + Duration::minutes(5)));
        assert!(step(&ctx("", now, None, false), &rl, ep4).is_none());
        let later = now + Duration::minutes(6);
        assert!(matches!(
            step(&ctx("", later, None, false), &rl, ep4),
            Some(Act::Continue)
        ));
        assert!(step(&ctx("", later, None, false), &rl, ep4).is_none());
        // error: continue the first time, tell the lead when it recurs
        let mut ep5 = Episode::default();
        let ep5 = ep5.for_cause(a.since, StuckCause::Error);
        let err = cause(StuckCause::Error, None);
        assert!(matches!(
            step(&ctx("", now, None, false), &err, ep5),
            Some(Act::Continue)
        ));
        let mut ep6 = Episode::default();
        let ep6 = ep6.for_cause(a.since, StuckCause::Error);
        match step(&ctx("l1\nl2\nl3\nl4\nl5\nl6", now, None, true), &err, ep6) {
            Some(Act::Tell { kind, text, .. }) => {
                assert_eq!(kind, "ERROR");
                assert!(text.contains("l6") && text.contains("l2"), "{text}");
            }
            x => panic!("{x:?} is no tell"),
        }
        // context_full → handoff to fallback; unsent → Enter then Escape+resend
        let mut ep7 = Episode::default();
        let ep7 = ep7.for_cause(a.since, StuckCause::ContextFull);
        assert!(matches!(
            step(
                &ctx("", now, Some("sonnet"), false),
                &cause(StuckCause::ContextFull, None),
                ep7
            ),
            Some(Act::Handoff { .. })
        ));
        let mut ep8 = Episode::default();
        let ep8 = ep8.for_cause(a.since, StuckCause::Unsent);
        let un = cause(StuckCause::Unsent, None);
        let box_ = "○ do it\n──────────────\n❭ Press Enter to send queued\n──────────────\nSWE-2 High · Context: 1 / 2,000 tokens (1%)\n";
        assert!(matches!(
            step(&ctx(box_, now, None, false), &un, ep8),
            Some(Act::Enter)
        ));
        assert!(step(&ctx(box_, now, None, false), &un, ep8).is_none());
        // the hint is still up 60 s on: no re-send while "press enter to send queued"
        // shows — the lead is told instead
        assert!(matches!(
            step(
                &ctx(box_, now + Duration::seconds(61), None, false),
                &un,
                ep8
            ),
            Some(Act::Tell { .. })
        ));
        // a pasted brief with no hint is Escape+re-sent right away …
        let mut ep9 = Episode::default();
        let ep9 = ep9.for_cause(a.since, StuckCause::Unsent);
        let pasted = "──────────────\n❭ do it\n──────────────\nSWE-2 High · Context: 1 / 2,000 tokens (1%)\n";
        match step(&ctx(pasted, now, None, false), &un, ep9) {
            Some(Act::Resend(t)) => assert_eq!(t, "do it"),
            x => panic!("{x:?} is no resend"),
        }
        // … but never while a doorbell to the agent is still undelivered
        let mut s2 = state_with_lead();
        s2.msgs.insert(
            "m1".into(),
            crate::state::Msg {
                from: "agent:h/s/lead".into(),
                to: "agent:h/s/a".into(),
                kind: "FYI".into(),
                reference: "x".into(),
                text: "hi".into(),
                since: now,
            },
        );
        let mut ep10 = Episode::default();
        let ep10 = ep10.for_cause(a.since, StuckCause::Unsent);
        let ctx2 = Ctx {
            s: &s2,
            id,
            a: &a,
            screen: pasted,
            now,
            handoff: None,
            continued_before: false,
        };
        assert!(matches!(step(&ctx2, &un, ep10), Some(Act::Tell { .. })));
        // permission/waiting/unknown never act
        for k in [
            StuckCause::Permission,
            StuckCause::Waiting,
            StuckCause::Unknown,
        ] {
            let mut e = Episode::default();
            let e = e.for_cause(a.since, k);
            assert!(step(&ctx("", now, None, false), &cause(k, None), e).is_none());
        }
    }

    #[test]
    fn held_and_resource_beat_the_screen_and_the_resource_wait_can_be_overdue() {
        let now = Utc::now();
        let mut s = state_with_lead();
        let id = "agent:h/s/a";
        let a = agent(AgentState::Idle, true);
        // a held task: the cause is held whatever the screen says
        s.tasks.insert(
            "task:p/1".into(),
            crate::state::Task {
                project: "p".into(),
                title: "t".into(),
                url: None,
                state: TaskState::Working,
                since: now,
                owner: Some(id.into()),
                lead: None,
                blocked_by: Default::default(),
                handoffs: vec![],
            },
        );
        s.blockers.insert(
            "b1".into(),
            crate::state::Blocker {
                task: Some("task:p/1".into()),
                what: "ships with the next release".into(),
                owner: None,
                unblocks_when: None,
                by: "agent:h/s/x".into(),
                since: now,
            },
        );
        let detected = Some(Cause::simple(StuckCause::Error, "e"));
        assert_eq!(cause_of(&s, id, &a, detected).kind, StuckCause::Held);
        let mut ep = Episode::default();
        let ep = ep.for_cause(a.since, StuckCause::Held);
        let hc = cause(StuckCause::Held, None);
        assert!(
            step(
                &Ctx {
                    s: &s,
                    id,
                    a: &a,
                    screen: "",
                    now,
                    handoff: None,
                    continued_before: false
                },
                &hc,
                ep
            )
            .is_none()
        );
        // a resource wait: cause resource; the tell fires only when overdue
        let mut s2 = state_with_lead();
        s2.worktrees.insert(
            "h:/w/a".into(),
            crate::state::Worktree {
                state: crate::event::WorktreeState {
                    host: "h".into(),
                    path: "/w/a".into(),
                    repo: "/r".into(),
                    branch: None,
                    head: "x".into(),
                    unpushed: 0,
                    dirty: 0,
                    files: vec![],
                    last_commit: now,
                    in_main: false,
                },
                since: now,
            },
        );
        s2.resources.insert(
            "h:sim".into(),
            crate::state::Resource {
                host: "h".into(),
                resource: "sim".into(),
                holder: Some(crate::state::Hold {
                    pid: 1,
                    cwd: Some("/w/other".into()),
                    scope: Some("e2e, ~21m".into()),
                    since: now - Duration::minutes(30),
                }),
                waiting: vec![crate::state::Hold {
                    pid: 2,
                    cwd: Some("/w/a".into()),
                    scope: None,
                    since: now - Duration::minutes(10),
                }],
            },
        );
        let mut aq = agent(AgentState::Working, true);
        aq.usage = Some(crate::state::Usage {
            rss_kb: None,
            cpu_pct: Some(0),
            procs: None,
            cwd: Some("/w/a".into()),
            ts: now,
        });
        let c = cause_of(&s2, id, &aq, None);
        assert_eq!(c.kind, StuckCause::Resource);
        assert!(c.detail.contains("waiting for sim"), "{}", c.detail);
        let mut ep2 = Episode::default();
        let ep2 = ep2.for_cause(aq.since, StuckCause::Resource);
        let rc = cause(StuckCause::Resource, None);
        // 10 min in behind a ~21m ETA: not overdue
        assert!(
            step(
                &Ctx {
                    s: &s2,
                    id,
                    a: &aq,
                    screen: "",
                    now,
                    handoff: None,
                    continued_before: false
                },
                &rc,
                ep2
            )
            .is_none()
        );
        // past 2× the ETA: one tell to the lead
        let later = now + Duration::minutes(40);
        let x = step(
            &Ctx {
                s: &s2,
                id,
                a: &aq,
                screen: "",
                now: later,
                handoff: None,
                continued_before: false,
            },
            &rc,
            ep2,
        );
        assert!(matches!(x, Some(Act::Tell { .. })), "{x:?}");
        assert!(
            step(
                &Ctx {
                    s: &s2,
                    id,
                    a: &aq,
                    screen: "",
                    now: later,
                    handoff: None,
                    continued_before: false
                },
                &rc,
                ep2
            )
            .is_none()
        );
    }

    impl std::fmt::Debug for Act {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Continue => write!(f, "Continue"),
                Self::Enter => write!(f, "Enter"),
                Self::Resend(t) => write!(f, "Resend({t})"),
                Self::Tell { kind, .. } => write!(f, "Tell({kind})"),
                Self::Handoff { to, .. } => write!(f, "Handoff({to})"),
            }
        }
    }
}
