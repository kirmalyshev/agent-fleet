//! The host's housekeeping, configured per project in fleet.toml `[chores]`: nudges for a
//! stuck agent, a spawn queue that drains while a profile has a free seat, and a periodic
//! sweep command. The decisions are pure (`classify`, `Nudger::step`, `parse_queue`,
//! `pick`); `serve` runs them against herdr and the log.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as Wait;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::event::{AgentState, Known, MsgSent};
use crate::host::{Host, herdr_call, short};
use crate::state::State;

/// `[chores]` in fleet.toml. Every part is off unless its table is present.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub nudge: Option<NudgeCfg>,
    pub queue: Option<QueueCfg>,
    pub sweep: Option<SweepCfg>,
}

/// `[chores.nudge]`: which agents get the nudges, and what else counts as an unsent prompt.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NudgeCfg {
    /// The programs nudged (`agent.kind`).
    #[serde(default = "default_kinds")]
    pub kinds: Vec<String>,
    /// A line near the bottom of the screen holding one of these means a message sits queued
    /// behind a busy turn, so Enter sends it. The program's own hint is always one.
    #[serde(default)]
    pub unsent_hints: Vec<String>,
}

fn default_kinds() -> Vec<String> {
    vec!["devin".into()]
}

/// `[chores.queue]`: issues waiting for a seat.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueCfg {
    /// One entry per line: `owner/repo#N [scope words]`. `#` starts a comment. A line that is
    /// not an issue reference is left alone. Absolute, or `~/…`.
    pub file: String,
    /// Short names for a repository: with `"dv-" = "owner/repo"`, the line `dv-12` is
    /// `owner/repo#12`. The longest matching prefix wins; the rest of the word must be digits.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// The profile every entry spawns with; its `cap` is the number of seats.
    pub profile: String,
    /// Only the host with this name drains the queue; absent, every host that has the file.
    #[serde(default)]
    pub on: Option<String>,
    /// Where a queued agent is spawned (`spawn --host`); absent, the routing rules decide only
    /// when there is no profile, so name it here.
    #[serde(default)]
    pub spawn_host: Option<String>,
}

/// `[chores.sweep]`: a command run in the project checkout.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepCfg {
    /// The program and its arguments, run without a shell, in the checkout.
    pub run: Vec<String>,
    #[serde(default = "default_every")]
    pub every_min: u64,
    /// Only the host with this name runs it; absent, every host that serves the checkout.
    #[serde(default)]
    pub on: Option<String>,
}

fn default_every() -> u64 {
    5
}

impl Config {
    /// What fleet.toml may not say: an empty command, a zero interval.
    pub fn check(&self, profiles: &BTreeMap<String, crate::spawn::Profile>) -> Result<(), String> {
        if let Some(s) = &self.sweep
            && (s.run.is_empty() || s.every_min == 0)
        {
            return Err("[chores.sweep] needs a `run` command and every_min >= 1".into());
        }
        if let Some(q) = &self.queue {
            match profiles.get(&q.profile) {
                None => return Err(format!("[chores.queue] profile {:?} is unknown", q.profile)),
                Some(p) if p.cap.is_none() => {
                    return Err(format!(
                        "[chores.queue] profile {:?} has no `cap`: the queue needs a seat count",
                        q.profile
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// ---------- the spawn queue ----------

/// One queue line that names an issue.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The line as written, for removing it.
    pub line: String,
    /// `owner/repo#N`.
    pub target: String,
    pub scope: Option<String>,
    /// The word as written when it was a short name (`dv-eait-12`): the agent and its branch
    /// are named by it, so two repositories' issue 12 never share a name.
    pub name: Option<String>,
}

/// `owner/repo#N` — what `agent-fleet spawn` takes.
fn is_target(w: &str) -> bool {
    let Some((repo, n)) = w.split_once('#') else {
        return false;
    };
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    ok(owner) && ok(name) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

/// `dv-12` as `owner/repo#12` under the longest matching alias.
fn alias_target(word: &str, aliases: &BTreeMap<String, String>) -> Option<String> {
    let (prefix, repo) = aliases
        .iter()
        .filter(|(p, _)| word.starts_with(p.as_str()))
        .max_by_key(|(p, _)| p.len())?;
    let n = &word[prefix.len()..];
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("{repo}#{n}"))
}

/// The entries of a queue file, in order. Comments, blanks and other bookkeeping lines are
/// not entries.
pub fn parse_queue(text: &str, aliases: &BTreeMap<String, String>) -> Vec<Entry> {
    text.lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.is_empty() || t.starts_with('#') {
                return None;
            }
            let (target, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
            let word = target;
            let aliased = alias_target(word, aliases);
            let name = aliased.is_some().then(|| word.to_string());
            let target = aliased.unwrap_or_else(|| word.to_string());
            is_target(&target).then(|| Entry {
                name,
                line: l.to_string(),
                target,
                scope: Some(rest.trim().to_string()).filter(|s| !s.is_empty()),
            })
        })
        .collect()
}

/// `text` without the first line equal to `line`.
pub fn drop_line(text: &str, line: &str) -> String {
    let mut gone = false;
    let mut out = String::with_capacity(text.len());
    for l in text.split_inclusive('\n') {
        if !gone && l.trim_end_matches(['\n', '\r']) == line {
            gone = true;
            continue;
        }
        out.push_str(l);
    }
    out
}

/// What a pass does with the queue: entries whose issue already has a live agent leave the
/// queue, and as many of the rest as there are free seats spawn.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub drop: Vec<Entry>,
    pub spawn: Vec<Entry>,
}

pub fn pick(entries: &[Entry], live: &HashSet<String>, seats: usize) -> Plan {
    let mut plan = Plan::default();
    for e in entries {
        if live.contains(&e.target) {
            plan.drop.push(e.clone());
        } else if plan.spawn.len() < seats && !plan.spawn.iter().any(|s| s.target == e.target) {
            plan.spawn.push(e.clone());
        }
    }
    plan
}

/// The spawns that hold a seat and their issue on this host: those of live agents, and requests
/// for this host and session not yet answered and not past `REQUEST_MAX_AGE` (older ones are
/// expired by `spawn` itself, so they hold nothing).
pub fn holders<'a>(
    s: &'a State,
    prefix: &str,
    host: &str,
    session: &str,
    now: DateTime<Utc>,
) -> Vec<&'a crate::state::Spawn> {
    let mut out: Vec<&crate::state::Spawn> = s
        .agents
        .keys()
        .filter(|id| id.starts_with(prefix))
        .filter_map(|id| s.spawn_of(id))
        .collect();
    for sp in s.spawns.values() {
        if sp.result.is_none()
            && sp.request.host == host
            && sp.request.session == session
            && now - sp.since <= crate::spawn::REQUEST_MAX_AGE
            && !out.iter().any(|o| o.request.agent == sp.request.agent)
        {
            out.push(sp);
        }
    }
    out
}

// ---------- nudges ----------

/// What a screen asks of the host.
#[derive(Debug, Clone, PartialEq)]
pub enum Sight {
    /// A message sits in the input box or queued behind a busy turn: `text` is what it says
    /// (empty when unreadable), `busy` when the program holds it behind a turn.
    Unsent { text: String, busy: bool },
    /// The program says it is rate limited and when it lifts.
    RateLimit(Duration),
    /// The program stopped with an error and waits for a message.
    Stopped,
}

const BUSY_HINT: &str = "Press Enter to send queued";
const STOPPED: [&str; 2] = ["Send a message to continue", "Something went wrong"];

fn tail(screen: &str, n: usize) -> Vec<&str> {
    let mut v: Vec<&str> = screen.lines().rev().take(n).collect();
    v.reverse();
    v
}

/// The minutes in a `resets in ~N` / `reset in N` on a line.
fn reset_minutes(line: &str) -> Option<i64> {
    let low = line.to_lowercase();
    let at = low.find("reset")?;
    let rest = low[at..]
        .split_once(" in")?
        .1
        .trim_start()
        .trim_start_matches('~');
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.parse().ok()?;
    let unit = rest[digits.len()..].trim_start();
    Some(if unit.starts_with('h') {
        n * 60
    } else if unit.starts_with('d') {
        n * 1440
    } else if unit.starts_with('s') {
        1
    } else {
        n
    })
}

pub fn classify(screen: &str, hints: &[String]) -> Option<Sight> {
    let bottom = tail(screen, 6);
    let busy = bottom.iter().any(|l| l.contains(BUSY_HINT));
    let hinted = bottom.iter().any(|l| {
        hints
            .iter()
            .any(|h| !h.is_empty() && l.contains(h.as_str()))
    });
    let text = crate::stuck::queued_text(screen).unwrap_or_default();
    if busy || hinted || !text.is_empty() {
        return Some(Sight::Unsent { text, busy });
    }
    let t = tail(screen, 14);
    if let Some(l) = t
        .iter()
        .rev()
        .find(|l| l.to_lowercase().contains("rate limit"))
    {
        return Some(Sight::RateLimit(Duration::minutes(
            reset_minutes(l)
                .or_else(|| t.iter().rev().find_map(|l| reset_minutes(l)))
                .unwrap_or(5),
        )));
    }
    t.iter()
        .any(|l| STOPPED.iter().any(|s| l.contains(s)))
        .then_some(Sight::Stopped)
}

/// What the host does for one agent this pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    Enter,
    /// One escape, then the same text sent again.
    EscResend(String),
    /// Nothing safe left to type: a human must clear the prompt.
    Blocked(String),
    /// `Continue where you stopped.`
    Continue,
    /// Two escapes, then the continue: the spinner has not moved for `HUNG`.
    Unhang,
}

/// Plain Enters tried against an unsent prompt before its text is re-sent once.
pub const ENTER_TRIES: u32 = 2;
const ENTER_EVERY: Duration = Duration::seconds(55);
const CONTINUE_EVERY: Duration = Duration::minutes(5);
const BLOCKED_EVERY: Duration = Duration::minutes(10);
const HUNG: Duration = Duration::minutes(15);

/// One agent's nudge state across passes.
#[derive(Debug, Default)]
pub struct Nudger {
    enters: u32,
    last_enter: Option<DateTime<Utc>>,
    /// The unsent prompt was seen on an earlier pass: a person may still be typing it.
    waited: bool,
    last_continue: Option<DateTime<Utc>>,
    rate_until: Option<DateTime<Utc>>,
    last_blocked: Option<DateTime<Utc>>,
    spin: Option<(u64, DateTime<Utc>)>,
    /// The agent is mid-turn: what its output says about limits or errors is not a stop.
    pub working: bool,
}

/// The screen with its timers and spinner glyphs taken out, hashed.
fn spinner_hash(screen: &str) -> Option<u64> {
    let t = tail(screen, 25);
    if !t.iter().any(|l| l.contains("(esc")) {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for l in t {
        let kept: String = l
            .chars()
            .filter(|c| !c.is_ascii_digit() && !('\u{2800}'..='\u{28ff}').contains(c))
            .collect();
        kept.hash(&mut h);
    }
    Some(h.finish())
}

impl Nudger {
    pub fn step(&mut self, screen: &str, hints: &[String], now: DateTime<Utc>) -> Option<Act> {
        let mut sight = classify(screen, hints);
        if self.working && matches!(sight, Some(Sight::RateLimit(_) | Sight::Stopped)) {
            sight = None;
        }
        if !matches!(sight, Some(Sight::Unsent { .. })) {
            self.enters = 0;
            self.last_enter = None;
            self.last_blocked = None;
            self.waited = false;
        }
        if sight != Some(Sight::Stopped) {
            self.last_continue = None;
        }
        if !matches!(sight, Some(Sight::RateLimit(_))) {
            self.rate_until = None;
        }
        // A spinner whose screen has not changed for HUNG is a hang, whatever else shows.
        match spinner_hash(screen) {
            None => self.spin = None,
            Some(h) => match self.spin {
                Some((old, since)) if old == h => {
                    if now - since >= HUNG {
                        self.spin = None;
                        return Some(Act::Unhang);
                    }
                }
                _ => self.spin = Some((h, now)),
            },
        }
        match sight? {
            Sight::Unsent { text, busy } => {
                // First sighting is no proof: a person attached to the pane may be mid-sentence.
                if !self.waited {
                    self.waited = true;
                    return None;
                }
                if self.last_enter.is_some_and(|t| now - t < ENTER_EVERY) {
                    return None;
                }
                self.last_enter = Some(now);
                self.enters += 1;
                if self.enters <= ENTER_TRIES {
                    return Some(Act::Enter);
                }
                // An Escape would interrupt the turn a busy queue waits behind.
                if self.enters == ENTER_TRIES + 1 && !busy && !text.is_empty() {
                    return Some(Act::EscResend(text));
                }
                if self.last_blocked.is_some_and(|t| now - t < BLOCKED_EVERY) {
                    return None;
                }
                self.last_blocked = Some(now);
                Some(Act::Blocked(if text.is_empty() {
                    format!(
                        "a prompt is still in the input after {ENTER_TRIES} Enters; its text cannot be read"
                    )
                } else {
                    format!("a prompt is still in the input after {ENTER_TRIES} Enters: {text}")
                }))
            }
            Sight::RateLimit(wait) => {
                // First sighting parks the nudge until the announced reset; then it fires once
                // and parks 5 min, so a still-limited screen cannot defer it forever.
                let due = *self.rate_until.get_or_insert(now + wait);
                if now < due {
                    return None;
                }
                self.rate_until = Some(now + CONTINUE_EVERY);
                Some(Act::Continue)
            }
            Sight::Stopped => {
                if self.last_continue.is_some_and(|t| now - t < CONTINUE_EVERY) {
                    return None;
                }
                self.last_continue = Some(now);
                Some(Act::Continue)
            }
        }
    }
}

// ---------- the host side ----------

/// `Continue where you stopped.`
const CONTINUE: &str = "Continue where you stopped. Keep replies short.";
const UNHUNG: &str =
    "Your last step hung for 15+ minutes and was interrupted. Continue where you stopped.";

/// A pass of each part runs this often at most (the sweep has its own interval).
const EVERY: Duration = Duration::seconds(60);

#[derive(Default)]
pub struct Seen {
    nudgers: HashMap<String, Nudger>,
    last: Option<DateTime<Utc>>,
    swept: HashMap<String, DateTime<Utc>>,
    sweeping: Arc<std::sync::Mutex<HashSet<String>>>,
    failed: HashMap<String, DateTime<Utc>>,
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()
            .map(|d| d.home_dir().join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

async fn keys(host: &Host, name: &str, ks: &[&str]) -> bool {
    herdr_call(
        &host.cfg.herdr_socket,
        "agent.send_keys",
        json!({ "target": name, "keys": ks }),
    )
    .await
    .is_ok()
}

async fn prompt(host: &Host, name: &str, text: &str) -> bool {
    herdr_call(
        &host.cfg.herdr_socket,
        "agent.prompt",
        json!({ "target": name, "text": text }),
    )
    .await
    .is_ok()
}

async fn nudge(
    host: &Arc<Host>,
    s: &State,
    repo: &str,
    cfg: &NudgeCfg,
    seen: &mut Seen,
    now: DateTime<Utc>,
) {
    let prefix = crate::spawn::agent_prefix(&host.cfg.host, &host.cfg.session);
    seen.nudgers.retain(|id, _| s.agents.contains_key(id));
    for (id, a) in &s.agents {
        if !id.starts_with(&prefix)
            || s.spawn_of(id).is_none_or(|sp| sp.request.repo != repo)
            || !a
                .kind
                .as_deref()
                .is_some_and(|k| cfg.kinds.iter().any(|c| c == k))
            || !matches!(
                a.state,
                AgentState::Working | AgentState::Idle | AgentState::Blocked
            )
        {
            continue;
        }
        let Some(screen) = crate::stuck::screen(host, id).await else {
            continue;
        };
        // A selection dialog takes an Enter as an answer: never type into one.
        if crate::stuck::dialog_open(&screen) {
            continue;
        }
        let n = seen.nudgers.entry(id.clone()).or_default();
        n.working = a.state == AgentState::Working;
        let Some(act) = n.step(&screen, &cfg.unsent_hints, now) else {
            continue;
        };
        let name = short(id);
        let ok = match &act {
            Act::Enter => keys(host, name, &["enter"]).await,
            Act::EscResend(t) => {
                keys(host, name, &["escape"]).await && {
                    tokio::time::sleep(Wait::from_secs(1)).await;
                    prompt(host, name, t).await
                }
            }
            Act::Continue => prompt(host, name, CONTINUE).await,
            Act::Unhang => {
                let k = keys(host, name, &["escape"]).await;
                tokio::time::sleep(Wait::from_millis(400)).await;
                let k = keys(host, name, &["escape"]).await && k;
                tokio::time::sleep(Wait::from_secs(3)).await;
                k && prompt(host, name, UNHUNG).await
            }
            Act::Blocked(why) => {
                if let Some(to) = crate::doctor::escalate(s, id) {
                    host.publish(host.presence(Known::MsgSent(MsgSent {
                        to: to.to_string(),
                        kind: "BLOCKED".into(),
                        reference: name.to_string(),
                        text: format!("{name}: {why}; nothing sent. Clear it by hand."),
                        closes: Vec::new(),
                    })))
                    .await;
                }
                true
            }
        };
        eprintln!(
            "agent-fleet host: nudge {name}: {act:?}{}",
            if ok { "" } else { " FAILED" }
        );
    }
}

/// Spawn what the queue's free seats allow, by running this binary's own `spawn` in the
/// project checkout, so the queue goes through the same routing, caps and brief as a person.
async fn queue(
    host: &Arc<Host>,
    s: &State,
    cfg: &QueueCfg,
    seats: usize,
    checkout: &Path,
    seen: &mut Seen,
    now: DateTime<Utc>,
) {
    let file = expand(&cfg.file);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("agent-fleet host: queue: read {}: {e}", file.display());
            return;
        }
    };
    let prefix = crate::spawn::agent_prefix(&host.cfg.host, &host.cfg.session);
    let live: HashSet<String> = holders(s, &prefix, &host.cfg.host, &host.cfg.session, now)
        .iter()
        .map(|sp| {
            format!(
                "{}#{}",
                sp.request.repo,
                sp.request.issue.unwrap_or_default()
            )
        })
        .collect();
    // An entry cooling after a failure gives its seat to the next one.
    let entries: Vec<Entry> = parse_queue(&text, &cfg.aliases)
        .into_iter()
        .filter(|e| {
            seen.failed
                .get(&e.target)
                .is_none_or(|t| now - *t >= Duration::minutes(10))
        })
        .collect();
    let plan = pick(&entries, &live, seats);
    let mut handled: Vec<String> = Vec::new();
    for e in &plan.drop {
        handled.push(e.line.clone());
        eprintln!(
            "agent-fleet host: queue: dropped {} (already live)",
            e.target
        );
    }
    for e in &plan.spawn {
        let exe = match std::env::current_exe() {
            Ok(x) => x,
            Err(e) => {
                eprintln!("agent-fleet host: queue: no own binary to spawn with: {e}");
                break;
            }
        };
        let mut c = tokio::process::Command::new(exe);
        c.arg("spawn").arg(&e.target).arg("--no-wait");
        c.args(["--profile", &cfg.profile]);
        if let Some(sc) = &e.scope {
            c.args(["--scope", sc]);
        }
        if let Some(n) = &e.name {
            c.args(["--name", n, "--branch", n]);
        }
        if let Some(h) = &cfg.spawn_host {
            c.args(["--host", h]);
        }
        c.current_dir(checkout).kill_on_drop(true);
        let done = tokio::time::timeout(Wait::from_secs(60), c.output()).await;
        match done {
            Ok(Ok(o)) if o.status.success() => {
                handled.push(e.line.clone());
                eprintln!("agent-fleet host: queue: spawn requested {}", e.target);
            }
            other => {
                seen.failed.insert(e.target.clone(), now);
                let why = match other {
                    Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "no answer in 60 s".into(),
                };
                eprintln!(
                    "agent-fleet host: queue: spawn {} FAILED, retry in 10m: {why}",
                    e.target
                );
            }
        }
    }
    rewrite(&file, &handled);
}

/// Take `handled` lines out of the file as it is NOW (a person may have appended during a
/// spawn), through a temp file and a rename so a crash never leaves it half written.
fn rewrite(file: &Path, handled: &[String]) {
    if handled.is_empty() {
        return;
    }
    let Ok(mut now) = std::fs::read_to_string(file) else {
        return;
    };
    for l in handled {
        now = drop_line(&now, l);
    }
    let tmp = file.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, now).and_then(|_| std::fs::rename(&tmp, file)) {
        eprintln!("agent-fleet host: queue: write {}: {e}", file.display());
    }
}

async fn sweep(cfg: SweepCfg, checkout: PathBuf) {
    let mut c = tokio::process::Command::new(&cfg.run[0]);
    c.args(&cfg.run[1..])
        .current_dir(&checkout)
        .kill_on_drop(true);
    match tokio::time::timeout(Wait::from_secs(15 * 60), c.output()).await {
        Ok(Ok(o)) => {
            let out = String::from_utf8_lossy(&o.stdout);
            for l in out
                .lines()
                .filter(|l| l.starts_with("removed") || l.starts_with("freed"))
            {
                eprintln!("agent-fleet host: sweep: {l}");
            }
            if !o.status.success() {
                eprintln!(
                    "agent-fleet host: sweep {:?} exited {}: {}",
                    cfg.run,
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
        }
        Ok(Err(e)) => eprintln!("agent-fleet host: sweep {:?}: {e}", cfg.run),
        Err(_) => eprintln!("agent-fleet host: sweep {:?}: no end after 15 min", cfg.run),
    }
}

/// One pass over every project checkout this host knows. Cheap when nothing is configured.
pub async fn serve(host: &Arc<Host>, s: &State, seen: &mut Seen) {
    let now = Utc::now();
    if seen.last.is_some_and(|t| now - t < EVERY) {
        return;
    }
    seen.last = Some(now);
    let checkouts = match host.cfg.checkouts_now() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agent-fleet host: chores: checkouts: {e:#}");
            return;
        }
    };
    for (repo, dir) in checkouts {
        let fc = match crate::spawn::load_in(&dir, host.cfg.config_dir.as_deref()) {
            Ok((fc, _)) => fc,
            Err(e) => {
                eprintln!("agent-fleet host: chores: {repo}: {e:#}");
                continue;
            }
        };
        let c = &fc.chores;
        // Typing into an agent is a stuck action: only under `host --stuck-act`.
        if let Some(n) = c.nudge.as_ref().filter(|_| host.cfg.stuck_act) {
            nudge(host, s, &repo, n, seen, now).await;
        }
        if let Some(q) = c
            .queue
            .as_ref()
            .filter(|q| q.on.as_ref().is_none_or(|h| *h == host.cfg.host))
        {
            let cap = fc.profiles.get(&q.profile).and_then(|p| p.cap).unwrap_or(0) as usize;
            let prefix = crate::spawn::agent_prefix(&host.cfg.host, &host.cfg.session);
            // Seats are the profile's cap for THIS repo, as `spawn` counts them: live agents
            // plus requests not yet answered.
            let used = holders(s, &prefix, &host.cfg.host, &host.cfg.session, now)
                .iter()
                .filter(|sp| sp.request.repo == repo && sp.request.profile == q.profile)
                .count();
            // One spawn per pass: a hung `spawn` must not hold the host's loop for long.
            let seats = cap.saturating_sub(used).min(1);
            queue(host, s, q, seats, &dir, seen, now).await;
        }
        if let Some(sw) = c
            .sweep
            .as_ref()
            .filter(|w| w.on.as_ref().is_none_or(|h| *h == host.cfg.host))
        {
            let due = seen
                .swept
                .get(&repo)
                .is_none_or(|t| now - *t >= Duration::minutes(sw.every_min as i64));
            if due
                && seen
                    .sweeping
                    .lock()
                    .is_ok_and(|mut r| r.insert(repo.clone()))
            {
                seen.swept.insert(repo.clone(), now);
                let (running, key) = (seen.sweeping.clone(), repo.clone());
                let (sw, dir) = (sw.clone(), dir.clone());
                tokio::spawn(async move {
                    sweep(sw, dir).await;
                    if let Ok(mut r) = running.lock() {
                        r.remove(&key);
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(min: i64) -> DateTime<Utc> {
        "2026-11-21T10:00:00Z".parse::<DateTime<Utc>>().unwrap() + Duration::minutes(min)
    }

    const BOX: &str = "────────────────────────────────────────\n ❭ fix the login test\n────────────────────────────────────────\nModel: x  Context: 1 / 2 tokens (3%)";
    const QUEUED: &str = "  ○ fix the login test\n────────────────────────────────────────\n ❭ Press Enter to send queued\n────────────────────────────────────────\nModel: x  Context: 1 / 2 tokens (3%)";

    #[test]
    fn queue_lines_are_issue_refs_only() {
        let q = "# waiting\na/b#12\n\nmac/%4\na/b#13 docs only  \nnot a ref\n";
        let e = parse_queue(q, &BTreeMap::new());
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].target, "a/b#12");
        assert_eq!(e[0].scope, None);
        assert_eq!(e[1].target, "a/b#13");
        assert_eq!(e[1].scope.as_deref(), Some("docs only"));
    }

    #[test]
    fn aliases_turn_short_names_into_issue_refs_longest_prefix_first() {
        let a: BTreeMap<String, String> = [
            ("dv-".to_string(), "o/app".to_string()),
            ("dv-x-".to_string(), "o/other".to_string()),
        ]
        .into();
        let e = parse_queue("dv-12 scope\ndv-x-7\ndv-x\ndv-1a\nmac/%3\n", &a);
        let got: Vec<(&str, Option<&str>)> = e
            .iter()
            .map(|e| (e.target.as_str(), e.scope.as_deref()))
            .collect();
        assert_eq!(got, [("o/app#12", Some("scope")), ("o/other#7", None)]);
        // the line is kept as written, so it can be removed from the file
        assert_eq!(e[0].line, "dv-12 scope");
        // a short name names the agent and branch; a full ref leaves the default
        assert_eq!(e[1].name.as_deref(), Some("dv-x-7"));
        assert_eq!(parse_queue("a/b#1", &a)[0].name, None);
    }

    #[test]
    fn drop_line_removes_one_line_and_keeps_the_rest() {
        let q = "# c\na/b#1\na/b#2 x\na/b#1\n";
        assert_eq!(drop_line(q, "a/b#1"), "# c\na/b#2 x\na/b#1\n");
        assert_eq!(drop_line(q, "zzz"), q);
    }

    #[test]
    fn pick_spawns_only_free_seats_and_drops_live_issues() {
        let e = parse_queue("a/b#1\na/b#2\na/b#3\na/b#2\n", &BTreeMap::new());
        let live: HashSet<String> = ["a/b#1".to_string()].into();
        let p = pick(&e, &live, 1);
        assert_eq!(p.drop.len(), 1);
        assert_eq!(p.spawn.len(), 1);
        assert_eq!(p.spawn[0].target, "a/b#2");
        assert!(pick(&e, &live, 0).spawn.is_empty());
    }

    #[test]
    fn holders_are_live_agents_and_fresh_requests_for_this_host_only() {
        let req = |agent: &str, host: &str| crate::event::SpawnRequested {
            name: agent.into(),
            agent: format!("agent:h/s/{agent}"),
            host: host.into(),
            session: "s".into(),
            repo: "o/r".into(),
            issue: Some(1),
            branch: agent.into(),
            profile: "p".into(),
            brief: String::new(),
            level: None,
            lead: None,
            worktree: None,
            replaces: None,
            why: String::new(),
        };
        let mut st = State::default();
        for (id, host, age) in [("fresh", "h", 1), ("stale", "h", 30), ("other", "x", 1)] {
            st.spawns.insert(
                id.into(),
                crate::state::Spawn {
                    request: req(id, host),
                    by: String::new(),
                    project: String::new(),
                    result: None,
                    since: t(-age),
                    answered: None,
                },
            );
        }
        let h = holders(&st, "agent:h/s/", "h", "s", t(0));
        let names: Vec<&str> = h.iter().map(|s| s.request.name.as_str()).collect();
        assert_eq!(names, ["fresh"]);
    }

    #[test]
    fn config_needs_a_capped_known_profile() {
        let c: Config = toml::from_str("[queue]\nfile = \"q\"\nprofile = \"d\"").unwrap();
        assert!(c.check(&BTreeMap::new()).unwrap_err().contains("unknown"));
        let s: Config = toml::from_str("[sweep]\nrun = []").unwrap();
        assert!(s.check(&BTreeMap::new()).is_err());
        assert!(toml::from_str::<Config>("[sweep]\nrun = [\"x\"]\nbogus = 1").is_err());
        let h: Config = toml::from_str(
            "[queue]\nfile = \"q\"\nprofile = \"d\"\non = \"a\"\nspawn_host = \"b\"\n[sweep]\nrun = [\"x\"]\non = \"a\"",
        )
        .unwrap();
        assert_eq!(h.queue.unwrap().spawn_host.as_deref(), Some("b"));
        assert_eq!(h.sweep.unwrap().on.as_deref(), Some("a"));
    }

    #[test]
    fn classifies_what_a_screen_asks() {
        assert!(matches!(
            classify(BOX, &[]),
            Some(Sight::Unsent { busy: false, .. })
        ));
        match classify(QUEUED, &[]) {
            Some(Sight::Unsent { text, busy }) => {
                assert!(busy);
                assert_eq!(text, "fix the login test");
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(
            classify("x\nRate limit hit, resets in ~7 minutes", &[]),
            Some(Sight::RateLimit(Duration::minutes(7)))
        );
        assert_eq!(classify("Something went wrong", &[]), Some(Sight::Stopped));
        assert_eq!(classify("all fine\n$ ", &[]), None);
        let hint = vec!["Never put secrets".to_string()];
        assert!(classify("Never put secrets here", &hint).is_some());
        assert!(classify("Never put secrets here", &[]).is_none());
    }

    #[test]
    fn enter_twice_then_one_esc_resend_then_blocked() {
        let mut n = Nudger::default();
        assert_eq!(n.step(BOX, &[], t(-1)), None); // first sighting only waits
        assert_eq!(n.step(BOX, &[], t(0)), Some(Act::Enter));
        // too soon for another Enter
        assert_eq!(n.step(BOX, &[], t(0)), None);
        assert_eq!(n.step(BOX, &[], t(1)), Some(Act::Enter));
        assert_eq!(
            n.step(BOX, &[], t(2)),
            Some(Act::EscResend("fix the login test".into()))
        );
        assert!(matches!(n.step(BOX, &[], t(3)), Some(Act::Blocked(_))));
        // one BLOCKED per 10 minutes
        assert_eq!(n.step(BOX, &[], t(4)), None);
        assert!(matches!(n.step(BOX, &[], t(14)), Some(Act::Blocked(_))));
        // a clear screen starts over
        assert_eq!(n.step("$ ", &[], t(15)), None);
        assert_eq!(n.step(BOX, &[], t(16)), None);
        assert_eq!(n.step(BOX, &[], t(17)), Some(Act::Enter));
    }

    #[test]
    fn drop_line_handles_crlf_and_rewrite_keeps_late_appends() {
        assert_eq!(drop_line("a/b#1\r\na/b#2\r\n", "a/b#1"), "a/b#2\r\n");
        let f = std::env::temp_dir().join(format!("chores-q-{}", std::process::id()));
        std::fs::write(&f, "a/b#1\na/b#2\n").unwrap();
        rewrite(&f, &["a/b#1".to_string()]);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a/b#2\n");
        std::fs::remove_file(&f).unwrap();
    }

    #[test]
    fn rate_limit_units_and_a_second_block_after_recovery() {
        assert_eq!(
            classify("Rate limit, resets in 2 hours", &[]),
            Some(Sight::RateLimit(Duration::minutes(120)))
        );
        let mut n = Nudger::default();
        n.step(BOX, &[], t(-1)); // first sighting only waits
        n.step(BOX, &[], t(0));
        n.step(BOX, &[], t(1));
        n.step(BOX, &[], t(2));
        assert!(matches!(n.step(BOX, &[], t(3)), Some(Act::Blocked(_))));
        n.step("$ ", &[], t(4));
        n.step(BOX, &[], t(4)); // first sighting waits
        n.step(BOX, &[], t(5));
        n.step(BOX, &[], t(6));
        n.step(BOX, &[], t(7));
        assert!(matches!(n.step(BOX, &[], t(8)), Some(Act::Blocked(_))));
    }

    #[test]
    fn limit_or_error_text_on_a_working_agent_is_not_a_stop() {
        let mut n = Nudger {
            working: true,
            ..Nudger::default()
        };
        assert_eq!(n.step("Something went wrong", &[], t(0)), None);
        assert_eq!(n.step("Rate limit, resets in 1 minute", &[], t(9)), None);
        n.working = false;
        assert_eq!(
            n.step("Something went wrong", &[], t(10)),
            Some(Act::Continue)
        );
    }

    #[test]
    fn a_busy_queue_never_gets_an_escape() {
        let mut n = Nudger::default();
        n.step(QUEUED, &[], t(-1)); // first sighting only waits
        n.step(QUEUED, &[], t(0));
        n.step(QUEUED, &[], t(1));
        assert!(matches!(n.step(QUEUED, &[], t(2)), Some(Act::Blocked(_))));
    }

    #[test]
    fn an_unreadable_prompt_is_blocked_not_retyped() {
        let mut n = Nudger::default();
        n.step("Never put it anywhere", &["Never put".to_string()], t(-1)); // first sighting only waits
        let hint = vec!["Never put".to_string()];
        let s = "Never put it anywhere";
        n.step(s, &hint, t(0));
        n.step(s, &hint, t(1));
        assert!(matches!(n.step(s, &hint, t(2)), Some(Act::Blocked(_))));
    }

    #[test]
    fn rate_limit_waits_for_the_reset_then_continues_once() {
        let mut n = Nudger::default();
        let s = "Rate limit, resets in 3 minutes";
        assert_eq!(n.step(s, &[], t(0)), None);
        assert_eq!(n.step(s, &[], t(2)), None);
        assert_eq!(n.step(s, &[], t(3)), Some(Act::Continue));
        assert_eq!(n.step(s, &[], t(4)), None);
    }

    #[test]
    fn stopped_gets_a_continue_every_five_minutes() {
        let mut n = Nudger::default();
        assert_eq!(
            n.step("Something went wrong", &[], t(0)),
            Some(Act::Continue)
        );
        assert_eq!(n.step("Something went wrong", &[], t(4)), None);
        assert_eq!(
            n.step("Something went wrong", &[], t(5)),
            Some(Act::Continue)
        );
    }

    #[test]
    fn a_frozen_spinner_is_unhung_after_fifteen_minutes() {
        let mut n = Nudger::default();
        let at =
            |m: i64, s: i64| format!("⠋ thinking · {m}m {s}s (esc to interrupt)\nworking on x");
        assert_eq!(n.step(&at(1, 5), &[], t(0)), None);
        // only the timer moves
        assert_eq!(n.step(&at(10, 5), &[], t(9)), None);
        assert_eq!(n.step(&at(16, 5), &[], t(15)), Some(Act::Unhang));
        // a screen that changes restarts the clock
        let mut n = Nudger::default();
        n.step(&at(1, 5), &[], t(0));
        n.step("⠋ thinking (esc to interrupt)\nnow on y", &[], t(10));
        assert_eq!(
            n.step("⠋ thinking (esc to interrupt)\nnow on y", &[], t(20)),
            None
        );
    }
}
