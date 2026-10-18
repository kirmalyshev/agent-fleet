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

/// An out-of-quota agent whose reset is at most this far out is just reported; further out
/// its work moves to the profile's `on_quota`.
pub const QUOTA_WAIT: Duration = Duration::minutes(30);
/// A rate-limit line without a number waits this long before the `continue`.
const RL_DEFAULT: Duration = Duration::minutes(5);

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

/// The program's own alert icons — `✱`/`▲`/`✗`/`✓`, or `[i]`/`[!]`/`[X]`/`[~]` on a
/// legacy terminal. A notice opens with one of them and then its title.
const ALERT_ICONS: [&str; 10] = ["[i]", "[!]", "[x]", "[~]", "✱", "▲", "✗", "✓", "⚠", "ℹ"];

/// `line` opens with an alert icon.
fn alert_icon(line: &str) -> bool {
    let t = line.trim_start();
    ALERT_ICONS
        .iter()
        .any(|i| t.get(..i.len()).is_some_and(|h| h.eq_ignore_ascii_case(i)))
}

/// The text past a leading alert icon — `▲ Quota exhausted` yields `Quota exhausted`;
/// a line with no icon is itself, trimmed.
fn notice_text(line: &str) -> &str {
    let t = line.trim_start();
    for icon in ALERT_ICONS {
        if t.get(..icon.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(icon))
        {
            return t[icon.len()..].trim_start_matches('\u{fe0f}').trim_start();
        }
    }
    t
}

/// Which lines of `screen` are the program's own UI, by position — not by first
/// character. From the input box's top rule down (the box, the `Context:` footer, the
/// status lines under it) everything is the program's. Above it, a `⏺`/`●`/`•`/`⎿`/`│`/
/// `└`/`○`/`❯`/`❭`/`›`/`>` line opens an assistant or tool block that runs to the next
/// blank line or the box; every line inside it is the agent's prose or its tool's
/// output — a hard-wrapped line resumes at column 0 — and so is every indented line
/// (a reply paragraph, printed file contents). The exception is a line opening with an
/// alert icon: the program's notice keeps its shape wherever it sits.
fn chrome_lines(program: Option<&str>, screen: &str) -> Vec<bool> {
    let lines: Vec<&str> = screen.lines().collect();
    let rule = |l: &str| l.starts_with(&"─".repeat(10));
    let prompt = |l: &str| {
        let l = l.trim_start();
        l.starts_with('❭') || l.starts_with('❯') || l.starts_with('›')
    };
    // the input box: the last rule directly above a ❭/❯/› prompt line; with its top
    // scrolled off, the last rule at all still fences the footers below it
    let top = lines
        .iter()
        .enumerate()
        .rev()
        .find(|&(i, l)| rule(l) && lines.get(i + 1).is_some_and(|l| prompt(l)))
        .map(|(i, _)| i)
        .or_else(|| lines.iter().rposition(|l| rule(l)));
    let mut out = vec![true; lines.len()];
    let mut block = false;
    for (i, l) in lines.iter().enumerate() {
        if top.is_some_and(|b| i >= b) {
            break;
        }
        let t = l.trim_start();
        if t.is_empty() {
            block = false;
            continue;
        }
        if t.starts_with(['⏺', '●', '•', '⎿', '│', '└', '○', '❯', '❭', '›', '>'])
        {
            block = true;
            out[i] = false;
        } else if program.is_none_or(|p| p == "devin") && alert_icon(t) {
            // a notice keeps its shape even inside a block
        } else if block || l.starts_with(char::is_whitespace) {
            out[i] = false;
        }
    }
    out
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

/// The line is the program's own rate-limit line; `Some(wait)` is the parsed retry
/// window. Devin's notices are Alert titles — a title leads its line, so a reply that
/// mentions one mid-sentence is not a notice.
fn rate_limit_on_screen(program: Option<&str>, line: &str) -> Option<Duration> {
    let wait = retry_in(line);
    if program == Some("devin") {
        let t = notice_text(line).to_lowercase();
        return (t.starts_with("rate limit") || t.starts_with("retry after") || wait.is_some())
            .then(|| wait.unwrap_or(RL_DEFAULT));
    }
    let low = line.to_lowercase();
    (low.contains("rate limit") || low.contains("retry after") || wait.is_some())
        .then(|| wait.unwrap_or(RL_DEFAULT))
}

/// The line is the program's own out-of-quota banner. For Devin — the one program
/// whose replies render as bare indented prose — the pattern is an Alert title and
/// leads its line past the icon.
fn quota_on_screen(program: Option<&str>, line: &str) -> bool {
    let low = line.to_lowercase();
    let title = notice_text(line).to_lowercase();
    let devin = ["quota exhausted", "usage limit", "usage paused"]
        .iter()
        .any(|p| title.starts_with(p));
    let other = low.contains("usage limit") || low.contains("limit reached");
    match program {
        Some("devin") => devin,
        Some("codex") => low.contains("usage limit"),
        Some(_) => other,
        None => devin || low.contains("quota exhausted") || other,
    }
}

/// The line is the program's own error banner — for Devin an Alert title leading the
/// line; its replies can quote one mid-sentence.
fn error_on_screen(program: Option<&str>, line: &str) -> bool {
    let low = line.to_lowercase();
    let generic = low.contains("something went wrong")
        || low.contains("send a message to continue")
        || low.contains("api error");
    let devin = [
        "something went wrong",
        "send a message to continue",
        "api error",
        "an internal error",
        "an unexpected error",
        "connection error",
        "response truncated",
    ]
    .iter()
    .any(|p| notice_text(line).to_lowercase().starts_with(p));
    match program {
        Some("devin") => devin,
        Some(_) => generic,
        None => devin || generic,
    }
}

/// ONE cause for a stopped agent, read off `screen_tail` — only the program's own UI
/// lines carry a signal: everything inside an assistant or tool block, a reply
/// paragraph or a printed file is the agent's prose and never does. `None` means the
/// screen names nothing the host recognizes. The result is a HINT: it may be shown to
/// the lead in a message, but it never drives an action — only the program's own
/// `agent.signal` does that.
pub fn stuck_cause(
    program: Option<&str>,
    screen_tail: &str,
    quota: Option<&Quota>,
    now: DateTime<Utc>,
) -> Option<Cause> {
    let lines: Vec<&str> = screen_tail.lines().collect();
    let chrome = chrome_lines(program, screen_tail);
    let ui = |i: usize| chrome.get(i).copied().unwrap_or(false);
    let t6 = lines.len().saturating_sub(6);
    let t14 = lines.len().saturating_sub(14);
    if lines.iter().enumerate().skip(t6).any(|(i, l)| {
        ui(i) && {
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
    let rl = lines
        .iter()
        .enumerate()
        .skip(t14)
        .rev()
        .find_map(|(i, l)| ui(i).then(|| rate_limit_on_screen(program, l)).flatten());
    if let Some(wait) = rl {
        return Some(Cause {
            kind: StuckCause::RateLimit,
            detail: "a rate limit is on screen".into(),
            until: Some(now + wait),
        });
    }
    // `quota` is the program saying it on screen — `agent.quota` only lends the spent
    // window's `resets_at`. An idle agent on an out-of-quota account is just idle.
    if lines
        .iter()
        .enumerate()
        .skip(t14)
        .any(|(i, l)| ui(i) && quota_on_screen(program, l))
    {
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
    if let Some(line) = lines
        .iter()
        .enumerate()
        .skip(t14)
        .rev()
        .find(|(i, l)| ui(*i) && context_full(program, l))
        .map(|(_, l)| *l)
    {
        return Some(Cause {
            kind: StuckCause::ContextFull,
            detail: format!("context nearly full: {}", line.trim()),
            until: None,
        });
    }
    if let Some(line) = lines
        .iter()
        .enumerate()
        .skip(t14)
        .rev()
        .find(|(i, l)| ui(*i) && error_on_screen(program, l))
        .map(|(_, l)| *l)
    {
        return Some(Cause {
            kind: StuckCause::Error,
            detail: format!("error on screen: {}", line.trim()),
            until: None,
        });
    }
    None
}

/// The cause the program's own signal names — the latest `agent.signal` stored for `id` —
/// and for Devin, which has no hooks, the account-level quota read (`agent.quota` exhausted)
/// while it sits idle. `prompt`/`turn_end`/`working` clear the stored signal in the fold, so
/// whatever remains belongs to this episode. `resumed` and unmapped signals name no cause.
fn signal_cause(s: &State, id: &str, a: &Agent, now: DateTime<Utc>) -> Option<Cause> {
    let sig = match s.signals.get(id) {
        Some(sig) => sig,
        None => {
            // Devin reports no hooks; its quota is account-level — `agent.quota` exhausted
            // plus a stopped agent IS the structured signal.
            if a.kind.as_deref() == Some("devin")
                && s.quotas
                    .get(&format!("{}:devin", a.host))
                    .is_some_and(|q| q.exhausted_at(now))
            {
                return Some(Cause {
                    kind: StuckCause::Quota,
                    detail: "the program's account is out of quota".into(),
                    until: None,
                });
            }
            return None;
        }
    };
    let with = |what: &str| match &sig.detail {
        Some(d) => format!("{what}: {d}"),
        None => what.to_string(),
    };
    Some(match sig.signal.as_str() {
        "quota" => Cause {
            kind: StuckCause::Quota,
            detail: "the program reported its quota exhausted".into(),
            until: None,
        },
        "rate_limit" => Cause {
            kind: StuckCause::RateLimit,
            detail: with("rate limited"),
            until: Some(
                sig.since
                    + sig
                        .detail
                        .as_deref()
                        .and_then(retry_in)
                        .unwrap_or(RL_DEFAULT),
            ),
        },
        "permission" => Cause::simple(StuckCause::Permission, "a permission prompt waits"),
        "question" => Cause {
            kind: StuckCause::Question,
            detail: with("it asks the principal"),
            until: None,
        },
        "context_full" => Cause {
            kind: StuckCause::ContextFull,
            detail: with("context nearly full"),
            until: None,
        },
        "compacting" => Cause::simple(
            StuckCause::ContextFull,
            "the program is compacting its context",
        ),
        "error" => Cause {
            kind: StuckCause::Error,
            detail: with("error"),
            until: None,
        },
        "account" => Cause {
            kind: StuckCause::Account,
            detail: with("account problem"),
            until: None,
        },
        "idle" => Cause::simple(StuckCause::Waiting, "the program reports it is idle"),
        _ => return None,
    })
}

/// The cause for a stopped agent: an intentional hold first, then herdr's `blocked` (a
/// permission or trust prompt) and a legitimate resource wait, then the program's own
/// signal, and `waiting` once the agent has worked, `unknown` before that.
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
/// the state or the cause changes. Each flag makes one action once-per-episode.
#[derive(Debug, Default)]
pub struct Episode {
    key: i64,
    cause: Option<StuckCause>,
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
/// Only a structured signal ever earns one: `continue` for a rate limit, a handoff for
/// quota far out. Everything else is a tell to the lead.
pub enum Act {
    /// `continue` once, as a doorbell to the agent itself.
    Continue,
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

/// The facts `step` reads for one pass: the agent, the screen hint text for the message, and
/// the caller's lookups — `handoff` is the vetted `on_quota` profile a `quota` cause may be
/// handed to.
pub struct Ctx<'a> {
    pub s: &'a State,
    pub id: &'a str,
    pub a: &'a Agent,
    pub hint: &'a str,
    pub now: DateTime<Utc>,
    pub handoff: Option<&'a str>,
}

/// The one action a cause earns this pass, or `None`.
pub fn step(c: &Ctx, cause: &Cause, ep: &mut Episode) -> Option<Act> {
    let (s, id, a, now) = (c.s, c.id, c.a, c.now);
    match cause.kind {
        StuckCause::Quota => {
            if ep.fired {
                return None;
            }
            ep.fired = true;
            let far = cause.until.is_none_or(|u| u - now > QUOTA_WAIT);
            let until = cause
                .until
                .map(|u| crate::ui::at(now, u))
                .unwrap_or_else(|| "an unknown time".into());
            if far && c.handoff.is_some() {
                return Some(Act::Handoff {
                    to: c.handoff?.to_string(),
                    extra: format!(
                        "out of {} quota until {until}",
                        a.kind.as_deref().unwrap_or("its program's")
                    ),
                });
            }
            tell(
                s,
                id,
                cause.kind,
                format!("{} is out of quota until {until}", crate::host::short(id)),
            )
        }
        StuckCause::RateLimit => {
            if !ep.continued && cause.until.is_none_or(|u| now >= u) {
                ep.continued = true;
                return Some(Act::Continue);
            }
            None
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
        // A held task and a resource wait are silent; `permission`, `waiting` and `unknown`
        // are covered by the HUNG/STALLED push, which already names the cause.
        StuckCause::Held | StuckCause::Permission | StuckCause::Waiting | StuckCause::Unknown => {
            None
        }
        // question, context_full, error, account, unsent: one tell to the lead per episode.
        _ => {
            if ep.fired {
                return None;
            }
            ep.fired = true;
            tell(
                s,
                id,
                cause.kind,
                format!("{}: {}{}", crate::host::short(id), cause.detail, c.hint),
            )
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

/// The `on_quota` profile `id` may be handed to, when this host can use it: never a profile
/// whose kind is the agent's own — a haiku shares the claude account, so a claude agent is
/// never handed to it here — or one out on this host. `None` while a replacement is open.
pub fn on_quota(host: &Host, s: &State, id: &str, now: DateTime<Utc>) -> Option<String> {
    if crate::spawn::replacement_open(s, id) {
        return None;
    }
    let sp = s.spawn_of(id)?;
    let checkout = host.cfg.checkouts.get(&sp.request.repo)?;
    let (cfg, _) = crate::spawn::load(checkout).ok()?;
    let prof = cfg.profiles.get(&sp.request.profile)?;
    let to = prof.on_quota.as_deref()?;
    let target = cfg.profiles.get(to)?;
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
    Some(to.to_string())
}

/// Read the agent's screen, record `agent.stuck` when the cause appears or changes (and
/// once with none when it works again), and run the cause's once-per-episode action.
/// Returns the cause for the caller's report — `None` when the agent is not stuck.
#[allow(clippy::too_many_arguments)]
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
    // The screen read is a hint only: it may ride along in the message to the lead, never
    // decide the cause or drive an action.
    let hint = stuck_cause(a.kind.as_deref(), &screen, quota, now);
    let mut cause = cause_of(s, id, a, signal_cause(s, id, a, now));
    // A quota cause takes `until` from the spent window's `resets_at`.
    if cause.kind == StuckCause::Quota && cause.until.is_none() {
        cause.until = quota.and_then(|q| {
            q.windows
                .iter()
                .filter(|w| w.used_pct >= 100.0)
                .filter_map(|w| w.resets_at)
                .filter(|r| *r > now)
                .min()
        });
    }
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
        || (cause.kind == StuckCause::Quota && crate::spawn::replacement_open(s, id));
    // A grace stretch: the cause is classified and recorded from the first pass, but the
    // host acts or tells only once the agent has sat in this state for --stall-min.
    if closed || busy || now - a.since < Duration::minutes(host.cfg.stall_min as i64) {
        return Some((cause, tail));
    }
    let handoff = (cause.kind == StuckCause::Quota)
        .then(|| on_quota(host, s, id, now))
        .flatten();
    let ep = eps
        .entry(id.to_string())
        .or_default()
        .for_cause(a.since, cause.kind);
    let screen_hint = match &hint {
        Some(h) if h.kind != cause.kind => format!(" · screen hints {}", h.detail),
        _ => String::new(),
    };
    if let Some(act) = step(
        &Ctx {
            s,
            id,
            a,
            hint: &screen_hint,
            now,
            handoff: handoff.as_deref(),
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
    fn prose_blocks_and_replies_carry_no_signal() {
        let now = Utc::now();
        // A signal quoted in a wrapped reply, a continuation line, a printed file or
        // a Devin reply is the agent talking — never the program's state.
        for (prog, screen) in [
            ("claude", "⏺ This change enforces a\n  usage limit per user"),
            ("claude", "⏺ Summary:\n  - Added rate limit middleware"),
            (
                "claude",
                "⏺ The client now retries on\n  API error responses",
            ),
            (
                "codex",
                "• The endpoint now returns\n  \"rate limit exceeded, retry after 30s\"",
            ),
            (
                "claude",
                "⏺ Read(src/rl.ts)\n  ⎿  Read 3 lines\n     export const RL = \"Rate limit reached\"",
            ),
            (
                "devin",
                " I added a Quota exhausted banner to the settings page.",
            ),
            (
                "devin",
                " The form now shows Something went wrong when the request fails.",
            ),
        ] {
            assert!(
                stuck_cause(Some(prog), screen, None, now).is_none(),
                "{prog}: {screen}"
            );
        }
        // a hard-wrapped continuation resumes at column 0 — still inside its block
        for (prog, screen) in [
            ("claude", "⏺ this change adds a\nusage limit per user"),
            ("codex", "• the endpoint returns\nrate limit exceeded"),
            (
                "devin",
                "I added a Quota exhausted banner to the settings page.",
            ),
            (
                "devin",
                "The form now shows Something went wrong when the request fails.",
            ),
        ] {
            assert!(
                stuck_cause(Some(prog), screen, None, now).is_none(),
                "{prog}: {screen}"
            );
        }
        // and a real notice — Devin's `▲ Quota exhausted` alert, icon and title —
        // still lands, indented or not, inside a block or after a blank line
        for (screen, want) in [
            (" ▲ Quota exhausted", StuckCause::Quota),
            ("▲ Quota exhausted", StuckCause::Quota),
            (" [X] Quota exhausted", StuckCause::Quota),
            ("⏺ done\n ▲ Usage limit reached", StuckCause::Quota),
            (" ⏺ done\n\n ✗ Something went wrong", StuckCause::Error),
        ] {
            assert_eq!(
                kind(stuck_cause(Some("devin"), screen, None, now)),
                Some(want),
                "{screen}"
            );
        }
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

    fn near_q(now: DateTime<Utc>) -> Cause {
        cause(StuckCause::Quota, Some(now + Duration::minutes(10)))
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
        let ctx = |hint: &'static str, now, handoff: Option<&'static str>| Ctx {
            s: &s,
            id,
            a: &a,
            hint,
            now,
            handoff,
        };
        let mut ep = Episode::default();
        let ep = ep.for_cause(a.since, StuckCause::Quota);
        // far-out quota with an on_quota target that has quota → handoff
        let far = cause(StuckCause::Quota, Some(now + Duration::hours(2)));
        assert!(matches!(
            step(&ctx("", now, Some("haiku")), &far, ep),
            Some(Act::Handoff { .. })
        ));
        // a second pass in the same episode does nothing
        assert!(step(&ctx("", now, Some("haiku")), &far, ep).is_none());
        // no vetted target → tell the lead, never hand off
        let mut ep2 = Episode::default();
        let ep2 = ep2.for_cause(a.since, StuckCause::Quota);
        match step(&ctx("", now, None), &far, ep2) {
            Some(Act::Tell { to, kind, text }) => {
                assert_eq!((to.as_str(), kind.as_str()), ("agent:h/s/lead", "QUOTA"));
                assert!(text.contains("out of quota until"), "{text}");
            }
            x => panic!("{x:?} is no tell"),
        }
        // a near reset → tell, even with a vetted target
        let mut ep2b = Episode::default();
        let ep2b = ep2b.for_cause(a.since, StuckCause::Quota);
        assert!(matches!(
            step(&ctx("", now, Some("haiku")), &near_q(now), ep2b),
            Some(Act::Tell { .. })
        ));
        // quota near: one tell, no continue ever
        let mut ep3 = Episode::default();
        let ep3 = ep3.for_cause(a.since, StuckCause::Quota);
        let near = cause(StuckCause::Quota, Some(now + Duration::minutes(10)));
        assert!(matches!(
            step(&ctx("", now, None), &near, ep3),
            Some(Act::Tell { .. })
        ));
        let past = now + Duration::minutes(11);
        assert!(step(&ctx("", past, None), &near, ep3).is_none());
        // rate limit: nothing before until, continue once after
        let mut ep4 = Episode::default();
        let ep4 = ep4.for_cause(a.since, StuckCause::RateLimit);
        let rl = cause(StuckCause::RateLimit, Some(now + Duration::minutes(5)));
        assert!(step(&ctx("", now, None), &rl, ep4).is_none());
        let later = now + Duration::minutes(6);
        assert!(matches!(
            step(&ctx("", later, None), &rl, ep4),
            Some(Act::Continue)
        ));
        assert!(step(&ctx("", later, None), &rl, ep4).is_none());
        // an unknown rate-limit wait defaults to 5 min — the caller sets `until`
        // error, question, context_full, account, unsent: one tell each, never a keystroke,
        // never a handoff — even with a target on the context
        for k in [
            StuckCause::Error,
            StuckCause::Question,
            StuckCause::ContextFull,
            StuckCause::Account,
            StuckCause::Unsent,
        ] {
            let mut e = Episode::default();
            let e = e.for_cause(a.since, k);
            match step(&ctx("", now, Some("haiku")), &cause(k, None), e) {
                Some(Act::Tell { kind, .. }) => assert_eq!(kind, k.name().to_uppercase()),
                x => panic!("{k:?}: {x:?} is no tell"),
            }
            assert!(step(&ctx("", now, Some("haiku")), &cause(k, None), e).is_none());
        }
        // the tell carries the screen hint when it differs
        let mut e = Episode::default();
        let e = e.for_cause(a.since, StuckCause::Question);
        match step(
            &ctx(" · screen hints a quota line", now, None),
            &cause(StuckCause::Question, None),
            e,
        ) {
            Some(Act::Tell { text, .. }) => assert!(text.contains("screen hints"), "{text}"),
            x => panic!("{x:?} is no tell"),
        }
        // permission/waiting/unknown never act — the stalled/hung push covers them
        for k in [
            StuckCause::Permission,
            StuckCause::Waiting,
            StuckCause::Unknown,
        ] {
            let mut e = Episode::default();
            let e = e.for_cause(a.since, k);
            assert!(step(&ctx("", now, None), &cause(k, None), e).is_none());
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
                    hint: "",
                    now,
                    handoff: None
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
                    hint: "",
                    now,
                    handoff: None
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
                hint: "",
                now: later,
                handoff: None,
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
                    hint: "",
                    now: later,
                    handoff: None
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
                Self::Tell { kind, .. } => write!(f, "Tell({kind})"),
                Self::Handoff { to, .. } => write!(f, "Handoff({to})"),
            }
        }
    }
}
