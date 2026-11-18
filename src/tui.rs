//! `agent-fleet tui`: the fleet on one screen, live, with the actions an operator takes. Every
//! write goes through the same commands as the CLI, and every action asks first. The command
//! line (`:`) runs the CLI itself, this binary with the screen's own flags, in the background.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{self, Event as Key, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::doctor::{Finding, findings, worktree_of};
use crate::event::{AgentState, Known, TaskState, TaskStateChanged, TaskUnassigned};
use crate::state::{State, worktree_key};
use crate::ui::ago;

/// What the screen needs from the CLI: reads with a note on where they came from, and writes.
pub trait Fleet {
    /// The state, and a warning when it is not live (the hub unreachable, a replica instead).
    async fn snapshot(&self) -> Result<(State, Option<String>)>;
    async fn task(&self, task: &str, body: Known) -> Result<String>;
    async fn tell(&self, agent: &str, text: &str) -> Result<String>;
    /// Answers an Inbox ask, as the board does.
    async fn answer(&self, ask: &str, text: &str, approve: bool) -> Result<String>;
    /// The CLI as a command line: this binary and the flags the screen runs under.
    fn cli(&self) -> Vec<String>;
    fn instance(&self) -> &str;
    fn hub(&self) -> String;
    fn host(&self) -> String;
}

const REFRESH: Duration = Duration::from_secs(2);
const ZEBRA: Color = Color::Indexed(236);
const DIM: Color = Color::Indexed(244);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Agents,
    Worktrees,
    Tasks,
    Findings,
    Resources,
    Asks,
}

const TABS: [(Tab, &str); 6] = [
    (Tab::Agents, "1 Agents"),
    (Tab::Worktrees, "2 Worktrees"),
    (Tab::Tasks, "3 Tasks"),
    (Tab::Findings, "4 Findings"),
    (Tab::Resources, "5 Resources"),
    (Tab::Asks, "6 Asks"),
];

/// An action waiting for `y`.
enum Pending {
    Kill(Vec<u32>, String),
    Remove {
        repo: String,
        path: String,
        branch: Option<String>,
    },
    Task(String, Box<Known>, &'static str),
    /// A CLI command, run in the background.
    Run(Vec<String>),
    Answer {
        ask: String,
        text: String,
        approve: bool,
    },
}

/// What a line of text is typed for.
#[derive(Clone)]
enum Input {
    Tell(String),
    Command,
    Answer { ask: String, approve: bool },
}

/// A command's whole output, and for a spawn's dry run the spawn `y` starts.
#[derive(Clone)]
struct Output {
    title: String,
    text: String,
    then: Option<Vec<String>>,
}

enum Mode {
    Normal,
    Confirm(String, Pending),
    Input(Input, String),
    Output(Output, u16),
    Help,
}

/// A finished background command.
struct Done {
    args: Vec<String>,
    ok: bool,
    out: String,
    then: Option<Vec<String>>,
    /// The instance it ran against, under `tui --all`.
    on: Option<String>,
}

/// One instance in the strip `tui --all` draws under the title.
struct Summary {
    name: String,
    text: String,
    color: Color,
}

/// What the strip says of an instance: its counts, read off its hub or its replica, or that it has
/// no state at all.
fn summary(name: String, got: &Result<(State, Option<String>)>, now: DateTime<Utc>) -> Summary {
    let (s, warn) = match got {
        Ok(v) => v,
        Err(_) => {
            return Summary {
                name,
                text: "no state".into(),
                color: Color::Red,
            };
        }
    };
    let working = (s.agents.values())
        .filter(|a| a.state == AgentState::Working)
        .count();
    let found = findings(s, now).len();
    let counts = format!(
        "{} agents, {working} working, {found} findings",
        s.agents.len()
    );
    match warn {
        Some(_) => Summary {
            name,
            text: format!("replica: {counts}"),
            color: Color::Yellow,
        },
        None => Summary {
            name,
            text: counts,
            color: if found == 0 { Color::Green } else { DIM },
        },
    }
}

struct App {
    tab: Tab,
    tables: [TableState; 6],
    state: State,
    found: Vec<Finding>,
    warn: Option<String>,
    note: Option<(String, Instant)>,
    mode: Mode,
    loaded: Option<Instant>,
    /// The commands still running, as typed.
    jobs: Vec<String>,
    last: Option<Output>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    /// The instance on screen: an index into the fleets the screen runs with.
    on: usize,
    /// Every instance, in the order of the fleets; more than one only under `tui --all`.
    instances: Vec<Summary>,
}

/// The home directory as `~`, also inside `<host>:<path>` keys.
fn home(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => p.replace(&format!("{h}/"), "~/"),
        _ => p.to_string(),
    }
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn state_name<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn agent_color(s: AgentState) -> Color {
    match s {
        AgentState::Working => Color::Green,
        AgentState::Blocked => Color::Red,
        AgentState::Unknown => Color::Yellow,
        AgentState::Idle | AgentState::Done => Color::Reset,
    }
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "hung" | "orphan" | "duplicate" | "breach" => Color::Red,
        "heavy" | "unpushed" | "forgotten" | "queued" | "refused" | "quota" => Color::Yellow,
        _ => DIM,
    }
}

fn mb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1}G", kb as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}M", kb / 1024)
    }
}

impl App {
    fn new() -> App {
        let (tx, rx) = mpsc::channel();
        App {
            tab: Tab::Agents,
            tables: Default::default(),
            state: State::default(),
            found: Vec::new(),
            warn: None,
            note: None,
            mode: Mode::Normal,
            loaded: None,
            jobs: Vec::new(),
            last: None,
            tx,
            rx,
            on: 0,
            instances: Vec::new(),
        }
    }

    /// Several instances on one screen (`tui --all`).
    fn multi(&self) -> bool {
        self.instances.len() > 1
    }

    /// `<instance>: ` before a prompt or an outcome under `tui --all`, so it is plain which
    /// instance an action goes to.
    fn label(&self, name: &str) -> String {
        if self.multi() {
            format!("{name}: ")
        } else {
            String::new()
        }
    }

    /// Puts the next (or previous) instance on screen. Its rows replace the last one's only once
    /// its own state is read, so no row of one instance is ever shown or acted on under another.
    fn switch(&mut self, forward: bool) {
        let n = self.instances.len();
        if n < 2 {
            return;
        }
        self.on = (self.on + if forward { 1 } else { n - 1 }) % n;
        self.state = State::default();
        self.found.clear();
        self.warn = None;
        self.tables = Default::default();
        self.loaded = None;
        let name = self.instances[self.on].name.clone();
        self.say(format!("instance {name}"));
    }

    fn at(&self) -> usize {
        self.tables[self.tab as usize].selected().unwrap_or(0)
    }

    fn rows(&self) -> usize {
        match self.tab {
            Tab::Agents => self.state.agents.len(),
            Tab::Worktrees => self.worktree_keys().len(),
            Tab::Tasks => self.state.tasks.len(),
            Tab::Findings => self.found.len(),
            Tab::Resources => {
                self.state.resources.len() + self.state.limits.len() + self.state.quotas.len()
            }
            Tab::Asks => self.state.inbox().len(),
        }
    }

    /// Worktrees, then orphaned directories (gone, but processes still run there).
    fn worktree_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.state.worktrees.keys().cloned().collect();
        keys.extend(
            self.state
                .worktree_procs
                .iter()
                .filter(|(_, p)| p.procs.orphaned)
                .map(|(k, _)| k.clone()),
        );
        keys
    }

    fn findings_for(&self, subject: &str) -> Vec<&Finding> {
        self.found
            .iter()
            .filter(|f| f.subject == subject || f.subject.split(" + ").any(|s| s == subject))
            .collect()
    }

    fn say(&mut self, s: impl Into<String>) {
        self.note = Some((s.into(), Instant::now()));
    }

    fn select(&mut self, delta: isize) {
        let n = self.rows();
        let t = &mut self.tables[self.tab as usize];
        if n == 0 {
            t.select(None);
            return;
        }
        let at = t.selected().unwrap_or(0) as isize;
        t.select(Some((at + delta).rem_euclid(n as isize) as usize));
    }
}

/// The screen over `fleets`, one instance each, starting on `fleets[on]`. With one fleet it is
/// the single-instance screen; with several (`tui --all`), `[` and `]` switch between them.
pub async fn run<F: Fleet>(fleets: &[F], on: usize) -> Result<()> {
    let mut term = ratatui::init();
    let result = main_loop(&mut term, fleets, on).await;
    ratatui::restore();
    result
}

async fn main_loop<F: Fleet>(term: &mut DefaultTerminal, fleets: &[F], on: usize) -> Result<()> {
    let mut app = App {
        on: on.min(fleets.len().saturating_sub(1)),
        ..App::new()
    };
    loop {
        while let Ok(done) = app.rx.try_recv() {
            finished(&mut app, done);
        }
        if app.loaded.is_none_or(|t| t.elapsed() >= REFRESH) {
            refresh(&mut app, fleets).await;
        }
        let now = Utc::now();
        term.draw(|f| draw(f, &app, &fleets[app.on], now))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Key::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return Ok(());
        }
        if !press(&mut app, fleets, k.code).await {
            return Ok(());
        }
    }
}

/// Reads every instance's state at once, each from its own hub or replica: one that is down
/// shows its own warning and leaves the others live.
async fn refresh<F: Fleet>(app: &mut App, fleets: &[F]) {
    let got = futures_util::future::join_all(fleets.iter().map(|f| f.snapshot())).await;
    let now = Utc::now();
    app.instances = (fleets.iter().zip(&got))
        .map(|(f, got)| summary(f.instance().to_string(), got, now))
        .collect();
    match got.into_iter().nth(app.on) {
        Some(Ok((s, warn))) => {
            app.found = findings(&s, now);
            app.state = s;
            app.warn = warn;
        }
        Some(Err(e)) => app.warn = Some(format!("{e:#}")),
        None => {}
    }
    app.loaded = Some(Instant::now());
    for tab in TABS.map(|(t, _)| t) {
        let saved = app.tab;
        app.tab = tab;
        let n = app.rows();
        let t = &mut app.tables[tab as usize];
        t.select(if n == 0 {
            None
        } else {
            Some(t.selected().unwrap_or(0).min(n - 1))
        });
        app.tab = saved;
    }
}

/// Handles a key in any mode; every action goes to the instance on screen. `false` quits.
async fn press<F: Fleet>(app: &mut App, fleets: &[F], code: KeyCode) -> bool {
    let fleet = &fleets[app.on];
    match std::mem::replace(&mut app.mode, Mode::Normal) {
        Mode::Help => {}
        Mode::Confirm(_, Pending::Run(args)) if code == KeyCode::Char('y') => {
            start(app, fleet, args, None)
        }
        Mode::Confirm(_, pending) if code == KeyCode::Char('y') => {
            let msg = act(fleet, pending).await;
            app.say(format!("{}{msg}", app.label(fleet.instance())));
            app.loaded = None;
        }
        Mode::Confirm(..) => app.say("cancelled"),
        Mode::Output(out, scroll) => match code {
            KeyCode::Char('y') if out.then.is_some() => {
                if let Some(l) = &mut app.last {
                    l.then = None;
                }
                start(app, fleet, out.then.unwrap_or_default(), None);
            }
            KeyCode::Down | KeyCode::Char('j') => app.mode = Mode::Output(out, scroll + 1),
            KeyCode::Up | KeyCode::Char('k') => {
                app.mode = Mode::Output(out, scroll.saturating_sub(1))
            }
            KeyCode::PageDown => app.mode = Mode::Output(out, scroll + 10),
            KeyCode::PageUp => app.mode = Mode::Output(out, scroll.saturating_sub(10)),
            _ => {}
        },
        Mode::Input(what, mut buf) => match code {
            KeyCode::Esc => app.say("cancelled"),
            KeyCode::Enter if !buf.trim().is_empty() => entered(app, fleet, what, buf),
            KeyCode::Tab if matches!(what, Input::Command) => {
                complete(app, &mut buf, fleet.instance());
                app.mode = Mode::Input(what, buf);
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::Input(what, buf);
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::Input(what, buf);
            }
            _ => app.mode = Mode::Input(what, buf),
        },
        Mode::Normal if app.multi() && matches!(code, KeyCode::Char('[' | ']')) => {
            app.switch(code == KeyCode::Char(']'))
        }
        Mode::Normal => return key(app, code, fleet).await,
    }
    true
}

/// A typed line: a message to send, a command to run, an answer to give. Each asks first.
fn entered(app: &mut App, fleet: &impl Fleet, what: Input, buf: String) {
    let text = buf.trim().to_string();
    match what {
        Input::Tell(agent) => {
            let args = ["tell", &agent, "NOTE", "tui", &text].map(str::to_string);
            app.mode = Mode::Confirm(
                format!("tell {}: {text}?", short(&agent)),
                Pending::Run(args.to_vec()),
            );
        }
        Input::Answer { ask, approve } => {
            let verb = if approve { "approve" } else { "answer" };
            app.mode = Mode::Confirm(
                format!("{verb} {}: {text}?", short(&ask)),
                Pending::Answer { ask, text, approve },
            );
        }
        Input::Command => match words(&text).and_then(plan) {
            Err(e) => app.say(e),
            Ok(Plan::Read(args)) => start(app, fleet, args, None),
            Ok(Plan::DryRun(dry, real)) => start(app, fleet, dry, Some(real)),
            Ok(Plan::Write(args)) => {
                app.mode = Mode::Confirm(format!("run agent-fleet {text}?"), Pending::Run(args))
            }
        },
    }
}

/// How the command line runs a command.
#[derive(Debug, PartialEq)]
enum Plan {
    /// It only reads: run it now.
    Read(Vec<String>),
    /// It writes: ask first.
    Write(Vec<String>),
    /// A spawn: its dry run first, then the spawn on `y`.
    DryRun(Vec<String>, Vec<String>),
}

fn plan(args: Vec<String>) -> Result<Plan, String> {
    let w = |i: usize| args.get(i).map_or("", String::as_str);
    let has = |f: &str| args.iter().any(|a| a == f);
    Ok(match w(0) {
        "" => return Err("type a command, e.g. task add <task> --title <title>".into()),
        "hub" | "host" | "tui" | "hook" | "mint-token" | "bench" => {
            return Err(format!("{} runs outside the screen", w(0)));
        }
        "spawn" if !has("--dry-run") => {
            let mut dry = args.clone();
            dry.push("--dry-run".into());
            Plan::DryRun(dry, args)
        }
        "ps" | "stats" | "proc" | "instances" | "state" | "ui" | "spawn" | "help" | "api" => {
            Plan::Read(args)
        }
        _ if has("--help") || has("-h") => Plan::Read(args),
        "doctor" if !has("--apply") => Plan::Read(args),
        "task" if matches!(w(1), "ls" | "show") => Plan::Read(args),
        "repo" if w(1) == "ls" => Plan::Read(args),
        "dev" if matches!(w(1), "ls" | "clean") && !has("--execute") => Plan::Read(args),
        "role" if args.len() == 2 => Plan::Read(args),
        _ => Plan::Write(args),
    })
}

/// Shell-like words: whitespace splits, single or double quotes keep spaces.
fn words(s: &str) -> Result<Vec<String>, String> {
    let (mut out, mut cur, mut quote) = (Vec::new(), None::<String>, None);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.get_or_insert_default().push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.get_or_insert_default();
            }
            None if c.is_whitespace() => out.extend(cur.take()),
            None => cur.get_or_insert_default().push(c),
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed".into());
    }
    out.extend(cur);
    Ok(out)
}

/// Runs the CLI in the background; `finished` takes the result.
fn start(app: &mut App, fleet: &impl Fleet, args: Vec<String>, then: Option<Vec<String>>) {
    let argv = fleet.cli();
    let tx = app.tx.clone();
    let on = app.multi().then(|| fleet.instance().to_string());
    let label = format!("{}{}", app.label(fleet.instance()), args.join(" "));
    app.jobs.push(label.clone());
    app.say(format!("running {label}"));
    std::thread::spawn(move || {
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output();
        let (ok, out) = match out {
            Ok(o) => (
                o.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
            ),
            Err(e) => (false, format!("could not run {}: {e}", argv[0])),
        };
        let _ = tx.send(Done {
            args,
            ok,
            out,
            then,
            on,
        });
    });
}

/// A background command ended: its last line is the outcome; a failure, a longer output or a
/// dry run opens the whole output (`o` opens it again).
fn finished(app: &mut App, d: Done) {
    let cmd = d.args.join(" ");
    let (label, flag) = match &d.on {
        Some(on) => (format!("{on}: "), format!("--instance {on} ")),
        None => Default::default(),
    };
    if let Some(i) = app.jobs.iter().position(|j| *j == format!("{label}{cmd}")) {
        app.jobs.remove(i);
    }
    app.loaded = None;
    let last = d.out.lines().rev().find(|l| !l.trim().is_empty());
    let last = last.unwrap_or(if d.ok { "done" } else { "failed" });
    let verb: Vec<&str> = d.args.iter().take(2).map(String::as_str).collect();
    app.say(format!("{label}{}: {last}", verb.join(" ")));
    let out = Output {
        title: format!(
            "agent-fleet {flag}{cmd}{}",
            if d.ok { "" } else { " (failed)" }
        ),
        text: d.out.clone(),
        then: d.then.filter(|_| d.ok),
    };
    let open = !d.ok || d.out.trim().lines().count() > 1 || out.then.is_some();
    app.last = Some(out.clone());
    if open && matches!(app.mode, Mode::Normal) {
        app.mode = Mode::Output(out, 0);
    }
}

/// Completes the last word of the command line from the commands, the agents that are up, tasks,
/// blockers, levels and the profiles of fleet.toml. Several matches: the common part, and the
/// matches in the note line.
fn complete(app: &mut App, buf: &mut String, instance: &str) {
    let word = buf.rsplit(' ').next().unwrap_or("").to_string();
    let mut all: Vec<String> = if buf.contains(' ') {
        let s = &app.state;
        let profiles: Vec<String> = crate::spawn::find(instance)
            .map(|(c, _)| c.profiles.into_keys().collect())
            .unwrap_or_default();
        (s.agents.keys().map(|a| short(a).to_string()))
            .chain([crate::event::COORDINATOR.to_string()])
            .chain(s.tasks.keys().cloned())
            .chain(s.blockers.keys().cloned())
            .chain(s.limits.values().map(|l| l.name.clone()))
            .chain(
                [
                    "root-coordinator",
                    "domain-lead",
                    "implementation-lead",
                    "implementer",
                ]
                .map(str::to_string),
            )
            .chain(profiles)
            .collect()
    } else {
        COMMANDS.map(str::to_string).to_vec()
    };
    all.retain(|c| c.starts_with(&word));
    all.sort();
    all.dedup();
    let Some(first) = all.first() else {
        return app.say("no match");
    };
    let common = all.iter().fold(first.clone(), |acc, c| {
        acc.chars()
            .zip(c.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect()
    });
    buf.truncate(buf.len() - word.len());
    buf.push_str(&common);
    if all.len() == 1 {
        buf.push(' ');
    } else {
        app.say(all.iter().take(12).cloned().collect::<Vec<_>>().join("  "));
    }
}

const COMMANDS: [&str; 20] = [
    "task",
    "dep",
    "block",
    "unblock",
    "ask",
    "role",
    "spawn",
    "handoff",
    "tell",
    "kill",
    "resource",
    "self-update",
    "doctor",
    "stats",
    "ui",
    "ps",
    "proc",
    "repo",
    "init",
    "instances",
];

/// Handles a key in the normal mode. `false` quits.
async fn key(app: &mut App, code: KeyCode, fleet: &impl Fleet) -> bool {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return false,
        KeyCode::Char('?') => app.mode = Mode::Help,
        KeyCode::Down | KeyCode::Char('j') => app.select(1),
        KeyCode::Up | KeyCode::Char('k') => app.select(-1),
        KeyCode::PageDown => app.select(10),
        KeyCode::PageUp => app.select(-10),
        KeyCode::Char('r') => app.loaded = None,
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.tab = TABS[(app.tab as usize + 1) % TABS.len()].0
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.tab = TABS[(app.tab as usize + TABS.len() - 1) % TABS.len()].0
        }
        KeyCode::Char(c @ '1'..='6') => app.tab = TABS[(c as u8 - b'1') as usize].0,
        KeyCode::Enter if app.tab == Tab::Findings => jump(app),
        KeyCode::Char(c) => action(app, c, fleet).await,
        _ => {}
    }
    true
}

/// From a finding to the agent or worktree it is about.
fn jump(app: &mut App) {
    let Some(f) = app.found.get(app.at()) else {
        return;
    };
    let subject = f
        .subject
        .split(" + ")
        .next()
        .unwrap_or_default()
        .to_string();
    if let Some(i) = app.state.agents.keys().position(|a| *a == subject) {
        app.tab = Tab::Agents;
        app.tables[Tab::Agents as usize].select(Some(i));
    } else if let Some(i) = app.worktree_keys().iter().position(|k| *k == subject) {
        app.tab = Tab::Worktrees;
        app.tables[Tab::Worktrees as usize].select(Some(i));
    } else if let Some(i) = (app.state.resources.keys())
        .chain(app.state.limits.keys())
        .chain(app.state.quotas.keys())
        .position(|k| *k == subject)
    {
        app.tab = Tab::Resources;
        app.tables[Tab::Resources as usize].select(Some(i));
    }
}

async fn action(app: &mut App, c: char, fleet: &impl Fleet) {
    let at = app.at();
    match (app.tab, c) {
        (Tab::Agents, 't') => {
            if let Some(agent) = app.state.agents.keys().nth(at) {
                app.mode = Mode::Input(Input::Tell(agent.clone()), String::new());
            }
        }
        (Tab::Agents, 'R' | 'H') => {
            if let Some(agent) = app.state.agents.keys().nth(at) {
                let a = short(agent);
                command(
                    app,
                    &if c == 'R' {
                        format!("role {a} ")
                    } else {
                        format!("handoff {a} --to ")
                    },
                );
            }
        }
        (Tab::Tasks, 'n') => command(app, "task add "),
        (Tab::Tasks, 'a' | 'b' | 'B' | 'p' | 'P') => {
            let Some((task, t)) = app.state.tasks.iter().nth(at) else {
                return;
            };
            let blocker = app
                .state
                .blockers
                .iter()
                .find(|(_, b)| b.task.as_deref() == Some(task))
                .map(|(id, _)| id.as_str());
            let line = match c {
                'a' => format!("task add {task} --to "),
                'b' => format!("block {task} "),
                'p' => format!("dep add {task} "),
                'P' => match t.blocked_by.iter().next() {
                    Some(on) => format!("dep rm {task} {on}"),
                    None => return app.say("it waits for no task"),
                },
                _ => match blocker {
                    Some(b) => format!("unblock {b} --note "),
                    None => return app.say("no blocker on it"),
                },
            };
            command(app, &line);
        }
        (Tab::Findings, 'K') => {
            let Some(f) = app.found.get(at) else {
                return;
            };
            let pids = pids_of(f);
            if pids.is_empty() || !matches!(f.kind, "hung" | "orphan") {
                return app.say("only a hung or orphaned process finding names pids to kill");
            }
            let host = f.subject.split_once(':').map_or("", |(h, _)| h);
            if host != fleet.host() {
                return app.say(format!("those run on {host}; kill them there"));
            }
            let mut args = vec!["kill".to_string()];
            args.extend(pids.iter().map(u32::to_string));
            app.mode = Mode::Confirm(
                format!(
                    "agent-fleet {}? (it checks again and may refuse)",
                    args.join(" ")
                ),
                Pending::Run(args),
            );
        }
        (Tab::Resources, 'L') => command(app, "resource limit "),
        (Tab::Resources, 'U') => {
            let rule = app
                .at()
                .checked_sub(app.state.resources.len())
                .and_then(|i| app.state.limits.values().nth(i));
            let Some(l) = rule else {
                return app.say("pick a rule row");
            };
            let args = ["resource", "unlimit", &l.name, "--host", &l.host].map(str::to_string);
            app.mode = Mode::Confirm(
                format!("remove the rule {} on {}?", l.name, l.host),
                Pending::Run(args.to_vec()),
            );
        }
        (Tab::Asks, 'a' | 'A') => {
            let Some(item) = app.state.inbox().into_iter().nth(at) else {
                return;
            };
            if item.answer.is_some() {
                return app.say("already answered");
            }
            let what = Input::Answer {
                ask: item.id,
                approve: c == 'A',
            };
            app.mode = Mode::Input(what, String::new());
        }
        (Tab::Agents, 'f') => {
            let Some((id, a)) = app.state.agents.iter().nth(at) else {
                return;
            };
            let msg = focus(fleet, id, &a.host, a.pane.as_deref()).await;
            app.say(msg);
        }
        (Tab::Tasks, 'd' | 'x' | 'u') => {
            let Some(task) = app.state.tasks.keys().nth(at).cloned() else {
                return;
            };
            let (body, verb) = match c {
                'd' => (
                    Known::TaskState(TaskStateChanged {
                        state: TaskState::Completed,
                    }),
                    "mark done",
                ),
                'x' => (
                    Known::TaskState(TaskStateChanged {
                        state: TaskState::Canceled,
                    }),
                    "cancel",
                ),
                _ => (
                    Known::TaskUnassigned(TaskUnassigned { reason: None }),
                    "release",
                ),
            };
            app.mode = Mode::Confirm(
                format!("{verb} {task}?"),
                Pending::Task(task, Box::new(body), verb),
            );
        }
        (Tab::Worktrees, 'K') => {
            let Some(key) = app.worktree_keys().get(at).cloned() else {
                return;
            };
            let Some(p) = app.state.worktree_procs.get(&key) else {
                return app.say("nothing runs there");
            };
            if p.procs.host != fleet.host() {
                return app.say(format!("those run on {}; kill them there", p.procs.host));
            }
            let pids = p.procs.pids.clone();
            let list: Vec<String> = pids.iter().map(u32::to_string).collect();
            app.mode = Mode::Confirm(
                format!(
                    "kill {} ({}) in {}?",
                    list.join(" "),
                    p.procs.names.join(", "),
                    home(&p.procs.path)
                ),
                Pending::Kill(pids, p.procs.path.clone()),
            );
        }
        (Tab::Worktrees, 'x') => {
            let Some(key) = app.worktree_keys().get(at).cloned() else {
                return;
            };
            let Some(w) = app.state.worktrees.get(&key) else {
                return;
            };
            let st = &w.state;
            if st.host != fleet.host() {
                return app.say(format!("that worktree is on {}", st.host));
            }
            if st.path == st.repo {
                return app.say("a main checkout is never removed");
            }
            if app
                .state
                .agents
                .values()
                .any(|a| worktree_of(&app.state, a).is_some_and(|(k, _)| *k == key))
            {
                return app.say("an agent works there");
            }
            let how = if hook(&st.repo).is_some() {
                "the repository's own retire hook"
            } else {
                "git worktree remove"
            };
            app.mode = Mode::Confirm(
                format!("remove {} through {how}?", home(&st.path)),
                Pending::Remove {
                    repo: st.repo.clone(),
                    path: st.path.clone(),
                    branch: st.branch.clone(),
                },
            );
        }
        (_, ':') => command(app, ""),
        (_, 'S') => command(app, "spawn "),
        (_, 'c') => command(app, "tell coordinator NOTE tui "),
        (_, 'o') => match app.last.clone() {
            Some(out) => app.mode = Mode::Output(out, 0),
            None => app.say("no command has run yet"),
        },
        _ => {}
    }
}

/// Opens the command line with `line` typed.
fn command(app: &mut App, line: &str) {
    app.note = None;
    app.mode = Mode::Input(Input::Command, line.to_string());
}

/// The pids a hung or orphan finding says to kill (`… kill 12 34`).
fn pids_of(f: &Finding) -> Vec<u32> {
    f.detail.rsplit_once("kill ").map_or(vec![], |(_, t)| {
        t.split_whitespace().map_while(|w| w.parse().ok()).collect()
    })
}

/// `<repo>/.agent-fleet/hook`, when the repository ships one: it knows the stack, databases and
/// caches a worktree holds, which fleet cannot.
pub fn hook(repo: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let p = Path::new(repo).join(".agent-fleet/hook");
    let meta = std::fs::metadata(&p).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(p)
}

async fn act(fleet: &impl Fleet, pending: Pending) -> String {
    match pending {
        Pending::Task(task, body, verb) => match fleet.task(&task, *body).await {
            Ok(id) => format!("{verb}: {id}"),
            Err(e) => format!("{verb} failed: {e:#}"),
        },
        Pending::Run(_) => unreachable!("runs in the background"),
        Pending::Answer { ask, text, approve } => match fleet.answer(&ask, &text, approve).await {
            Ok(id) => format!("answered {}: {id}", short(&ask)),
            Err(e) => format!("answer failed: {e:#}"),
        },
        Pending::Kill(pids, dir) => {
            let mut out = Vec::new();
            for pid in pids {
                // A pid may have been reused since the scan. Kill only a process whose own
                // directory is deleted, inside `dir`, which is itself gone; never herdr or a
                // pane's shell.
                let link = std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let comm =
                    |p: u32| std::fs::read_to_string(format!("/proc/{p}/comm")).unwrap_or_default();
                let ppid = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|s| crate::scan::parse_stat(&s))
                    .map_or(0, |(ppid, ..)| ppid);
                let terminal = comm(pid).trim() == "herdr" || comm(ppid).trim() == "herdr";
                let here = !terminal
                    && !Path::new(&dir).exists()
                    && link
                        .strip_suffix(" (deleted)")
                        .is_some_and(|c| c == dir || c.starts_with(&format!("{dir}/")));
                if !here {
                    out.push(format!("{pid} skipped (no longer there)"));
                    continue;
                }
                // SAFETY: kill only sends a signal.
                let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                out.push(if rc == 0 {
                    format!("{pid} sent TERM")
                } else {
                    format!("{pid} failed")
                });
            }
            out.join(", ")
        }
        Pending::Remove { repo, path, branch } => match retire(&repo, &path).await {
            Ok(()) => {
                let gh = std::env::var("AGENT_FLEET_GH").unwrap_or_else(|_| "gh".into());
                removed_line(fleet, &gh, &repo, &path, branch.as_deref()).await
            }
            Err(e) => e,
        },
    }
}

/// Removes the worktree at `path` of the checkout `repo`: the repo's `.agent-fleet/hook retire`,
/// else `git worktree remove` (which refuses uncommitted or untracked files). `Err` is the line
/// to show.
pub async fn retire(repo: &str, path: &str) -> Result<(), String> {
    let out = match hook(repo) {
        Some(h) => {
            tokio::process::Command::new(h)
                .args(["retire", path])
                .output()
                .await
        }
        None => {
            tokio::process::Command::new("git")
                .args(["-C", repo, "worktree", "remove", path])
                .output()
                .await
        }
    };
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "refused: {}",
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .last()
                .unwrap_or("?")
        )),
        Err(e) => Err(format!("could not run: {e}")),
    }
}

/// The outcome line of a removed worktree: when its branch still has an open PR, the line says
/// so, and the root coordinator is told the PR is a coordinator item now.
pub async fn removed_line(
    fleet: &impl Fleet,
    gh: &str,
    repo: &str,
    path: &str,
    branch: Option<&str>,
) -> String {
    let mut line = format!("removed {}", home(path));
    let Some(b) = branch else {
        return line;
    };
    match crate::orphanpr::retire_note(gh, repo, b).await {
        Ok(None) => {}
        Err(e) => line += &format!("; PR check failed: {e:#}"),
        Ok(Some((_, note))) => {
            line += &format!("; {note}");
            let root = fleet
                .snapshot()
                .await
                .map(|(s, _)| crate::orphanpr::root_of(&s));
            line += &match root {
                Ok(Some(root)) => match fleet.tell(&root, &format!("{b} retired: {note}")).await {
                    Ok(_) => " (coordinator told)".to_string(),
                    Err(e) => format!(" (tell failed: {e:#})"),
                },
                Ok(None) => " (no root coordinator to tell)".into(),
                Err(e) => format!(" (tell failed: {e:#})"),
            };
        }
    }
    line
}

async fn focus(fleet: &impl Fleet, id: &str, host: &str, pane: Option<&str>) -> String {
    if host != fleet.host() {
        return format!("{} runs on {host}", short(id));
    }
    let (Some(pane), Some(sock)) = (pane, std::env::var_os("HERDR_SOCKET_PATH")) else {
        return "focus works from inside the agent's herdr session".into();
    };
    let target = serde_json::json!({ "target": pane });
    match crate::host::herdr_call(Path::new(&sock), "agent.focus", target).await {
        Ok(_) => format!("focused {}", short(id)),
        Err(e) => format!("focus failed: {e:#}"),
    }
}

fn draw(f: &mut Frame, app: &App, fleet: &impl Fleet, now: DateTime<Utc>) {
    let alerts: Vec<&Finding> = app
        .found
        .iter()
        .filter(|f| matches!(f.kind, "orphan" | "hung" | "duplicate" | "breach"))
        .take(3)
        .collect();
    let [head, alert, tabs, table, detail, foot] = Layout::vertical([
        Constraint::Length(1 + u16::from(app.multi())),
        Constraint::Length(alerts.len() as u16 + u16::from(app.warn.is_some())),
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(7),
        Constraint::Length(2),
    ])
    .areas(f.area());

    let s = &app.state;
    let working = s
        .agents
        .values()
        .filter(|a| a.state == AgentState::Working)
        .count();
    let open = s.tasks.values().filter(|t| !t.state.is_terminal()).count();
    let summary = Line::from(vec![
        Span::styled(
            format!("agent-fleet {}", fleet.instance()),
            Style::new().bold(),
        ),
        Span::raw(format!(
            " · {} · {} agents, {working} working · {open} open tasks · {} worktrees · ",
            fleet.hub(),
            s.agents.len(),
            s.worktrees.len()
        )),
        Span::styled(
            format!("{} findings", app.found.len()),
            Style::new().fg(if app.found.is_empty() {
                Color::Green
            } else {
                Color::Yellow
            }),
        ),
        Span::raw(format!(" · {} undelivered", s.msgs.len())),
    ]);
    let mut top = vec![summary];
    if app.multi() {
        // Every instance, the one on screen reversed: its counts, or why it has none.
        let mut strip = vec![Span::styled("[ ] ", Style::new().fg(DIM))];
        for (i, sum) in app.instances.iter().enumerate() {
            let style = if i == app.on {
                Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::new().bold()
            };
            strip.push(Span::styled(format!(" {} ", sum.name), style));
            strip.push(Span::styled(
                format!(" {}   ", sum.text),
                Style::new().fg(sum.color),
            ));
        }
        top.push(Line::from(strip));
    }
    f.render_widget(Paragraph::new(top), head);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(w) = &app.warn {
        lines.push(Line::styled(w.clone(), Style::new().fg(Color::Yellow)));
    }
    for a in &alerts {
        lines.push(Line::styled(
            format!("{}: {} · {}", a.kind, home(&a.subject), a.detail),
            Style::new().fg(Color::Red),
        ));
    }
    f.render_widget(Paragraph::new(lines), alert);

    let tab_line = Line::from(
        TABS.iter()
            .flat_map(|(t, name)| {
                let style = if *t == app.tab {
                    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                } else {
                    Style::new().fg(DIM)
                };
                [Span::styled(format!(" {name} "), style), Span::raw("  ")]
            })
            .collect::<Vec<_>>(),
    );
    f.render_widget(Paragraph::new(vec![Line::raw(""), tab_line]), tabs);

    let (header, widths, rows, about) = match app.tab {
        Tab::Agents => agents(app, now),
        Tab::Worktrees => worktrees(app, now),
        Tab::Tasks => tasks(app, now),
        Tab::Findings => found(app),
        Tab::Resources => resources(app, now),
        Tab::Asks => asks(app, now),
    };
    let rows: Vec<Row> = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| if i % 2 == 1 { r.bg(ZEBRA) } else { r })
        .collect();
    let empty = rows.is_empty();
    let t = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(DIM)))
        .row_highlight_style(Style::new().fg(Color::Black).bg(Color::White))
        .highlight_symbol("› ");
    let mut ts = app.tables[app.tab as usize].clone();
    f.render_stateful_widget(t, table.inner(ratatui::layout::Margin::new(0, 0)), &mut ts);
    if empty {
        let none = Rect {
            y: table.y + 1,
            height: 1,
            ..table
        };
        f.render_widget(Paragraph::new("  nothing here").fg(DIM), none);
    }

    let rule = "─".repeat(detail.width as usize);
    let mut text = vec![Line::styled(rule.clone(), Style::new().fg(DIM))];
    text.extend(about);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), detail);

    let on = app.label(fleet.instance());
    let keys = match (&app.mode, app.tab) {
        (Mode::Confirm(q, _), _) => Line::styled(
            format!("{on}{q}  y yes · any other key no"),
            Style::new().fg(Color::Yellow).bold(),
        ),
        (Mode::Input(what, buf), _) => {
            let (prompt, hint) = match what {
                Input::Tell(agent) => (format!("tell {}: ", short(agent)), "Enter send"),
                Input::Command => (":".to_string(), "Tab complete · Enter run"),
                Input::Answer { ask, approve } => (
                    format!(
                        "{} {}: ",
                        if *approve { "approve" } else { "answer" },
                        short(ask)
                    ),
                    "Enter send",
                ),
            };
            let note = app.note.as_ref().filter(|_| matches!(what, Input::Command));
            Line::from(vec![
                Span::styled(format!("{on}{prompt}"), Style::new().bold()),
                Span::raw(buf.clone()),
                Span::styled("█", Style::new().fg(DIM)),
                Span::styled(format!("   {hint} · Esc cancel   "), Style::new().fg(DIM)),
                Span::styled(
                    note.map(|(n, _)| n.clone()).unwrap_or_default(),
                    Style::new().fg(Color::Cyan),
                ),
            ])
        }
        (_, tab) => {
            let note = app
                .note
                .as_ref()
                .filter(|(_, t)| t.elapsed() < Duration::from_secs(6))
                .map(|(n, _)| format!("{n}   "))
                .unwrap_or_default();
            let jobs = if app.jobs.is_empty() {
                String::new()
            } else {
                format!("running: {}   ", app.jobs.join(" · "))
            };
            let own = match tab {
                Tab::Agents => "t Tell · f Focus · R Role · H Handoff · ",
                Tab::Worktrees => "x Remove · K Kill · ",
                Tab::Tasks => {
                    "n New · a Assign · d Done · u Release · x Cancel · b Block · B Unblock · p/P Dep · "
                }
                Tab::Findings => "Enter Go to · K Kill · ",
                Tab::Resources => "L Limit · U Unlimit · ",
                Tab::Asks => "a Answer · A Approve · ",
            };
            let switch = if app.multi() { "[ ] Instance · " } else { "" };
            Line::from(vec![
                Span::styled(jobs, Style::new().fg(Color::Yellow)),
                Span::styled(note, Style::new().fg(Color::Cyan)),
                Span::raw(format!(
                    "{own}: Command · S Spawn · c Coordinator · o Output · {switch}? Help · q Quit"
                )),
            ])
        }
    };
    f.render_widget(
        Paragraph::new(vec![Line::styled(rule, Style::new().fg(DIM)), keys]),
        foot,
    );

    match &app.mode {
        Mode::Help => help(f),
        Mode::Output(out, scroll) => output(f, out, *scroll),
        _ => {}
    }
}

type View<'a> = (
    Vec<&'static str>,
    Vec<Constraint>,
    Vec<Row<'a>>,
    Vec<Line<'a>>,
);

fn agents(app: &App, now: DateTime<Utc>) -> View<'_> {
    let s = &app.state;
    let owned = |id: &str| {
        s.tasks
            .iter()
            .find(|(_, t)| t.owner.as_deref() == Some(id) && !t.state.is_terminal())
            .map(|(k, _)| short(k).to_string())
            .unwrap_or_default()
    };
    let rows = s
        .agents
        .iter()
        .map(|(id, a)| {
            let wt = worktree_of(s, a).map(|(_, w)| w.state.branch.clone().unwrap_or_default());
            let (cpu, mem) = a
                .usage
                .as_ref()
                .map_or((String::new(), String::new()), |u| {
                    (
                        u.cpu_pct.map(|c| format!("{c}%")).unwrap_or_default(),
                        u.rss_kb.map(mb).unwrap_or_default(),
                    )
                });
            let n = app.findings_for(id).len();
            let role = s.roles.get(id).map(|r| r.level.short()).unwrap_or_default();
            let stuck = s
                .stuck
                .get(id)
                .map(|st| format!("·{}", st.cause.name()))
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(short(id).to_string()),
                Cell::from(a.host.clone()),
                Cell::from(format!("{}{}", state_name(a.state), stuck)).fg(agent_color(a.state)),
                Cell::from(ago(now, a.since)),
                Cell::from(role),
                Cell::from(cpu),
                Cell::from(mem),
                Cell::from(owned(id)),
                Cell::from(wt.unwrap_or_default()),
                Cell::from(if n > 0 { format!("{n}") } else { String::new() }).fg(Color::Red),
            ])
        })
        .collect();
    let about = match s.agents.iter().nth(app.at()) {
        None => vec![],
        Some((id, a)) => {
            let mut l = vec![Line::from(vec![
                Span::styled(id.clone(), Style::new().bold()),
                Span::raw(format!(
                    "  {} for {} · pane {} · {}",
                    state_name(a.state),
                    ago(now, a.since),
                    a.pane.as_deref().unwrap_or("-"),
                    crate::ui::kind_profile(s, id, a.kind.as_deref())
                )),
            ])];
            if let Some(st) = s.stuck.get(id) {
                l.push(Line::raw(format!(
                    "stuck: {} — {}",
                    st.cause.name(),
                    st.detail
                )));
            }
            if let Some(sp) = s.spawn_of(id) {
                l.push(Line::raw(format!("spawn: {}", sp.request.why)));
            }
            if let Some(u) = &a.usage {
                let cost = match (u.procs, u.rss_kb, u.cpu_pct) {
                    (Some(p), Some(r), Some(c)) => format!("{p} processes, {}, {c}% CPU · ", mb(r)),
                    _ => String::new(),
                };
                l.push(Line::raw(format!(
                    "{cost}in {} (sampled {} ago)",
                    u.cwd.as_deref().map(home).unwrap_or_default(),
                    ago(now, u.ts)
                )));
            }
            l.extend(crate::ui::role_lines(s, id, now).into_iter().map(Line::raw));
            l.extend(finding_lines(app, id));
            l
        }
    };
    (
        vec![
            "AGENT", "HOST", "STATE", "FOR", "ROLE", "CPU", "MEM", "TASK", "BRANCH", "!",
        ],
        vec![
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Min(16),
            Constraint::Length(2),
        ],
        rows,
        about,
    )
}

fn worktrees(app: &App, now: DateTime<Utc>) -> View<'_> {
    let s = &app.state;
    let busy = |key: &str| {
        s.agents
            .iter()
            .filter(|(_, a)| worktree_of(s, a).is_some_and(|(k, _)| k == key))
            .map(|(id, _)| short(id).to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let keys = app.worktree_keys();
    let rows = keys
        .iter()
        .map(|key| {
            let procs = s.worktree_procs.get(key).map(|p| p.procs.clone());
            let ran = procs
                .as_ref()
                .map_or(String::new(), |p| format!("{} · {}", p.procs, mb(p.rss_kb)));
            match s.worktrees.get(key) {
                Some(w) => {
                    let st = &w.state;
                    let n =
                        |v: u32, c: Color| {
                            Cell::from(if v > 0 { v.to_string() } else { "·".into() })
                                .fg(if v > 0 { c } else { DIM })
                        };
                    Row::new(vec![
                        Cell::from(st.branch.clone().unwrap_or_else(|| "(detached)".into())),
                        Cell::from(busy(key)),
                        n(st.unpushed, Color::Yellow),
                        n(st.dirty, Color::Yellow),
                        Cell::from(ran),
                        Cell::from(ago(now, st.last_commit)),
                        Cell::from(home(&st.path)),
                    ])
                }
                None => {
                    let p = procs
                        .unwrap_or_else(|| unreachable!("orphan keys come from worktree_procs"));
                    Row::new(vec![
                        Cell::from("worktree gone").fg(Color::Red),
                        Cell::from(""),
                        Cell::from("—"),
                        Cell::from("—"),
                        Cell::from(ran).fg(Color::Red),
                        Cell::from("—"),
                        Cell::from(home(&p.path)),
                    ])
                }
            }
        })
        .collect();
    let about = keys.get(app.at()).map_or(vec![], |key| {
        let mut l = Vec::new();
        if let Some(w) = s.worktrees.get(key) {
            let st = &w.state;
            l.push(Line::from(vec![
                Span::styled(home(&st.path), Style::new().bold()),
                Span::raw(format!(
                    "  {} at {} · repo {} · changed {} ago",
                    st.branch.as_deref().unwrap_or("detached"),
                    &st.head[..st.head.len().min(8)],
                    home(&st.repo),
                    ago(now, w.since)
                )),
            ]));
            if !st.files.is_empty() {
                let shown: Vec<&str> = st.files.iter().take(6).map(String::as_str).collect();
                l.push(Line::raw(format!(
                    "{} files: {}",
                    st.files.len(),
                    shown.join(", ")
                )));
            }
        }
        if let Some(p) = s.worktree_procs.get(key) {
            let pids: Vec<String> = p.procs.pids.iter().map(u32::to_string).collect();
            l.push(Line::raw(format!(
                "runs {} ({}): {}, {}% CPU · pids {}",
                p.procs.procs,
                p.procs.names.join(", "),
                mb(p.procs.rss_kb),
                p.procs.cpu_pct,
                pids.join(" ")
            )));
        }
        l.extend(finding_lines(app, key));
        l
    });
    (
        vec![
            "BRANCH", "AGENT", "UNPUSHED", "DIRTY", "RUNS", "COMMIT", "WORKTREE",
        ],
        vec![
            Constraint::Length(26),
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Min(20),
        ],
        rows,
        about,
    )
}

fn tasks(app: &App, now: DateTime<Utc>) -> View<'_> {
    let s = &app.state;
    let rows = s
        .tasks
        .iter()
        .map(|(id, t)| {
            let color = match t.state {
                TaskState::Working => Color::Green,
                TaskState::InputRequired => Color::Yellow,
                TaskState::Failed => Color::Red,
                _ => DIM,
            };
            let blockers = s
                .blockers
                .values()
                .filter(|b| b.task.as_deref() == Some(id))
                .count();
            Row::new(vec![
                Cell::from(id.trim_start_matches("task:").to_string()),
                Cell::from(state_name(t.state)).fg(color),
                Cell::from(ago(now, t.since)),
                Cell::from(t.owner.as_deref().map(short).unwrap_or("-").to_string()),
                Cell::from(if blockers > 0 {
                    blockers.to_string()
                } else {
                    String::new()
                })
                .fg(Color::Red),
                Cell::from(t.title.clone()),
            ])
        })
        .collect();
    let about = s.tasks.iter().nth(app.at()).map_or(vec![], |(id, t)| {
        let mut l = vec![Line::from(vec![
            Span::styled(id.clone(), Style::new().bold()),
            Span::raw(format!(
                "  {} · {}",
                t.title,
                t.url.as_deref().unwrap_or("")
            )),
        ])];
        for b in s
            .blockers
            .values()
            .filter(|b| b.task.as_deref() == Some(id))
        {
            l.push(Line::styled(
                format!(
                    "blocked: {} (owner {})",
                    b.what,
                    b.owner.as_deref().unwrap_or("-")
                ),
                Style::new().fg(Color::Red),
            ));
        }
        if !t.blocked_by.is_empty() {
            let on: Vec<&str> = t.blocked_by.iter().map(String::as_str).collect();
            l.push(Line::raw(format!("waits for {}", on.join(", "))));
        }
        l
    });
    (
        vec!["TASK", "STATE", "FOR", "OWNER", "B", "TITLE"],
        vec![
            Constraint::Length(28),
            Constraint::Length(15),
            Constraint::Length(7),
            Constraint::Length(16),
            Constraint::Length(2),
            Constraint::Min(20),
        ],
        rows,
        about,
    )
}

fn found(app: &App) -> View<'_> {
    let rows = app
        .found
        .iter()
        .map(|f| {
            Row::new(vec![
                Cell::from(f.kind).fg(kind_color(f.kind)),
                Cell::from(home(&f.subject)),
                Cell::from(f.detail.clone()),
            ])
        })
        .collect();
    let about = app.found.get(app.at()).map_or(vec![], |f| {
        let mut l = vec![
            Line::styled(
                format!("{} · {}", f.kind, home(&f.subject)),
                Style::new().bold(),
            ),
            Line::raw(f.detail.clone()),
        ];
        // What `agent-fleet proc` says of each pid the finding names.
        let host = f.subject.split_once(':').map_or("", |(h, _)| h);
        for pid in pids_of(f) {
            let (verdict, why) = crate::watch::verdict_of(&app.state, host, pid, Utc::now());
            l.push(Line::styled(
                format!(
                    "proc {pid}: {verdict}{}",
                    why.map(|w| format!(": {w}")).unwrap_or_default()
                ),
                Style::new().fg(DIM),
            ));
        }
        l
    });
    (
        vec!["KIND", "SUBJECT", "DETAIL"],
        vec![
            Constraint::Length(10),
            Constraint::Percentage(40),
            Constraint::Min(30),
        ],
        rows,
        about,
    )
}

fn resources(app: &App, now: DateTime<Utc>) -> View<'_> {
    let s = &app.state;
    let mut rows: Vec<Row> = s
        .resources
        .iter()
        .map(|(key, r)| {
            let (holder, age) = r
                .holder
                .as_ref()
                .map_or(("free".into(), String::new()), |h| {
                    let what = h.scope.clone().unwrap_or_else(|| format!("pid {}", h.pid));
                    let wt = h
                        .cwd
                        .as_deref()
                        .map_or("", |c| c.rsplit('/').next().unwrap_or(c));
                    (format!("{what} · {wt}"), ago(now, h.since))
                });
            let queue = match r.waiting.first() {
                None => String::new(),
                Some(w) => format!(
                    "{} waiting, first {} ago",
                    r.waiting.len(),
                    ago(now, w.since)
                ),
            };
            Row::new(vec![
                Cell::from(home(key)),
                Cell::from(holder),
                Cell::from(age),
                Cell::from(queue).fg(if r.waiting.is_empty() {
                    DIM
                } else {
                    Color::Yellow
                }),
            ])
        })
        .collect();
    rows.extend(s.limits.iter().map(|(key, l)| {
        let until = l.until.map_or(String::new(), |u| {
            if now >= u {
                "expired".to_string()
            } else {
                let u = u.with_timezone(&chrono::Local);
                let today = u.date_naive() == now.with_timezone(&chrono::Local).date_naive();
                format!("→{}", u.format(if today { "%H:%M" } else { "%b%d" }))
            }
        });
        Row::new(vec![
            Cell::from(home(key)),
            Cell::from(format!(
                "rule: at most {} of {}",
                l.max,
                l.resources.join(" / ")
            )),
            Cell::from(until).fg(DIM),
            Cell::from(String::new()),
        ])
    }));
    rows.extend(s.quotas.iter().map(|(key, q)| {
        let windows = q
            .windows
            .iter()
            .map(|w| format!("{} {:.0}%", w.name, w.used_pct))
            .collect::<Vec<_>>()
            .join(" · ");
        Row::new(vec![
            Cell::from(key.clone()),
            Cell::from(if q.exhausted_at(now) {
                format!("EXHAUSTED · {windows}")
            } else {
                windows
            })
            .fg(if q.exhausted_at(now) {
                Color::Red
            } else {
                Color::Reset
            }),
            Cell::from(ago(now, q.read_at)),
            Cell::from(q.plan.clone().unwrap_or_default()).fg(DIM),
        ])
    }));
    let at = app.at();
    let quotas_at = at.saturating_sub(s.resources.len() + s.limits.len());
    let about = if let Some((key, q)) = s.quotas.iter().nth(quotas_at)
        && at >= s.resources.len() + s.limits.len()
    {
        let mut lines = vec![Line::styled(key.clone(), Style::new().bold())];
        for w in &q.windows {
            let resets = w.resets_at.map_or(String::new(), |r| {
                format!(", resets {}", crate::ui::at(now, r))
            });
            lines.push(Line::raw(format!(
                "{}: {:.0}% used{resets}",
                w.name, w.used_pct
            )));
        }
        lines.push(Line::raw(format!(
            "plan {} · read {} ago{}",
            q.plan.as_deref().unwrap_or("-"),
            ago(now, q.read_at),
            if q.exhausted_at(now) {
                " · EXHAUSTED"
            } else {
                ""
            }
        )));
        lines.extend(finding_lines(app, key));
        lines
    } else if let Some((key, r)) = s.resources.iter().nth(at) {
        let mut lines = vec![Line::styled(home(key), Style::new().bold())];
        match &r.holder {
            Some(h) => lines.push(Line::raw(format!("held by {}", crate::ui::hold(now, h)))),
            None => lines.push(Line::raw("free")),
        }
        for (i, w) in r.waiting.iter().enumerate() {
            lines.push(Line::raw(format!(
                "  {}. {}",
                i + 1,
                crate::ui::hold(now, w)
            )));
        }
        lines.extend(finding_lines(app, key));
        lines
    } else if let Some((key, l)) = s.limits.iter().nth(at - s.resources.len()) {
        let mut lines = vec![
            Line::styled(home(key), Style::new().bold()),
            Line::raw(format!("at most {} of {}", l.max, l.resources.join(" / "))),
            Line::raw(l.until.map_or("no expiry".to_string(), |u| {
                if now >= u {
                    format!("expired at {}", crate::ui::at(now, u))
                } else {
                    format!("until {}", crate::ui::at(now, u))
                }
            })),
        ];
        for name in &l.resources {
            let Some(r) = s.resources.get(&worktree_key(&l.host, name)) else {
                continue;
            };
            if let Some(h) = &r.holder {
                lines.push(Line::raw(format!(
                    "{name}: held by {} · {} waiting",
                    crate::ui::hold(now, h),
                    r.waiting.len()
                )));
            }
        }
        lines.extend(finding_lines(app, key));
        lines
    } else {
        vec![]
    };
    (
        vec!["RESOURCE", "HOLDER", "FOR", "QUEUE"],
        vec![
            Constraint::Length(24),
            Constraint::Percentage(45),
            Constraint::Length(7),
            Constraint::Min(20),
        ],
        rows,
        about,
    )
}

fn asks(app: &App, now: DateTime<Utc>) -> View<'_> {
    let inbox = app.state.inbox();
    let rows = inbox
        .iter()
        .map(|i| {
            let (state, color) = match &i.answer {
                None => ("open", Color::Yellow),
                Some(a) if a.acked.is_some() => ("seen", DIM),
                Some(_) => ("answered", Color::Green),
            };
            Row::new(vec![
                Cell::from(state).fg(color),
                Cell::from(short(&i.ask.by).to_string()),
                Cell::from(i.level.short()),
                Cell::from(ago(now, i.ask.since)),
                Cell::from(i.ask.question.clone()),
            ])
        })
        .collect();
    let about = inbox.into_iter().nth(app.at()).map_or(vec![], |i| {
        let mut l = vec![Line::from(vec![
            Span::styled(i.ask.question.clone(), Style::new().bold()),
            Span::raw(format!(
                "  {} · {}",
                i.ask.by,
                i.ask.task.as_deref().unwrap_or("no task")
            )),
        ])];
        for (n, o) in i.ask.options.iter().enumerate() {
            l.push(Line::raw(format!("  {}. {o}", n + 1)));
        }
        if let Some(r) = &i.ask.recommendation {
            l.push(Line::raw(format!("recommends: {r}")));
        }
        if let Some(c) = &i.ask.context {
            l.push(Line::raw(c.clone()).fg(DIM));
        }
        if let Some(a) = &i.answer {
            let how = if a.approve { "approved" } else { "answered" };
            l.push(Line::styled(
                format!("{how} {} ago: {}", ago(now, a.at), a.answer),
                Style::new().fg(Color::Green),
            ));
        }
        l
    });
    (
        vec!["STATE", "BY", "LEVEL", "FOR", "QUESTION"],
        vec![
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Min(20),
        ],
        rows,
        about,
    )
}

fn finding_lines<'a>(app: &'a App, subject: &str) -> Vec<Line<'a>> {
    app.findings_for(subject)
        .into_iter()
        .map(|f| {
            Line::styled(
                format!("{}: {}", f.kind, f.detail),
                Style::new().fg(kind_color(f.kind)),
            )
        })
        .collect()
}

fn help(f: &mut Frame) {
    let text = vec![
        Line::styled("agent-fleet tui", Style::new().bold()),
        Line::raw(""),
        Line::raw("↑↓ / j k      move            PgUp PgDn   ten rows"),
        Line::raw("← → / 1-6     tabs            r           refresh now (it refreshes every 2 s)"),
        Line::raw("[ ]           the previous or next instance (tui --all); actions go to it"),
        Line::raw(""),
        Line::raw(
            "Agents        t tell · f focus its herdr pane · R role · H handoff --to <profile>",
        ),
        Line::raw("Worktrees     x remove one no agent works in (.agent-fleet/hook retire, else"),
        Line::raw("                git worktree remove) · K kill the processes left in it"),
        Line::raw("Tasks         n new (task add) · a assign · d done · u release · x cancel"),
        Line::raw("              b block · B unblock · p dep add · P dep rm"),
        Line::raw("Findings      Enter go to its subject · K kill a hung or orphaned process"),
        Line::raw("Resources     L add a capacity rule · U remove the selected rule"),
        Line::raw("Asks          the board's Inbox: a answer · A approve"),
        Line::raw(""),
        Line::raw(":             any CLI command, run as `agent-fleet <command>` (Tab completes):"),
        Line::raw(
            "              task, dep, block, unblock, role, spawn, handoff, tell, kill, resource,",
        ),
        Line::raw("              self-update, doctor --apply, stats, ui, proc <pid>, ps …"),
        Line::raw(
            "S spawn (its dry run first) · c tell the coordinator · o the last command's output",
        ),
        Line::raw(""),
        Line::raw("Every action asks first. Findings are `agent-fleet doctor`'s."),
        Line::raw(""),
        Line::styled("any key closes this", Style::new().fg(DIM)),
    ];
    let area = f.area();
    let w = 92.min(area.width);
    let h = (text.len() as u16 + 2).min(area.height);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, r);
    f.render_widget(
        Paragraph::new(text).block(ratatui::widgets::Block::bordered()),
        r,
    );
}

/// A command's whole output over the screen; for a dry run, `y` starts the spawn.
fn output(f: &mut Frame, out: &Output, scroll: u16) {
    let area = f.area().inner(ratatui::layout::Margin::new(2, 1));
    let keys = if out.then.is_some() {
        " y spawn · ↑↓ scroll · any other key cancels "
    } else {
        " ↑↓ scroll · any other key closes "
    };
    let block = ratatui::widgets::Block::bordered()
        .title(Line::styled(
            format!(" {} ", out.title),
            Style::new().bold(),
        ))
        .title_bottom(Line::styled(keys, Style::new().fg(Color::Yellow)));
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(
        Paragraph::new(out.text.clone())
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Agent;
    use clap::Parser;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A fleet with a root coordinator that records what it is told.
    struct Told(std::sync::Mutex<Vec<(String, String)>>);

    impl Fleet for Told {
        async fn snapshot(&self) -> Result<(State, Option<String>)> {
            let mut s = State::default();
            s.roles.insert(
                "agent:h/s/root".into(),
                crate::state::Role {
                    level: crate::event::Level::RootCoordinator,
                    lead: None,
                    domain: None,
                    by: "x".into(),
                    since: Utc::now(),
                },
            );
            Ok((s, None))
        }
        async fn task(&self, _: &str, _: Known) -> Result<String> {
            unreachable!()
        }
        async fn tell(&self, a: &str, t: &str) -> Result<String> {
            self.0.lock().unwrap().push((a.into(), t.into()));
            Ok(String::new())
        }
        async fn answer(&self, ask: &str, t: &str, approve: bool) -> Result<String> {
            self.0
                .lock()
                .unwrap()
                .push((ask.into(), format!("{t} {approve}")));
            Ok("e1".into())
        }
        fn cli(&self) -> Vec<String> {
            // Prints the command line it was given, as the CLI's outcome line.
            ["sh", "-c", "echo \"$@\"", "agent-fleet"]
                .map(str::to_string)
                .to_vec()
        }
        fn instance(&self) -> &str {
            "i"
        }
        fn hub(&self) -> String {
            String::new()
        }
        fn host(&self) -> String {
            "h".into()
        }
    }

    #[tokio::test]
    async fn a_retired_worktree_with_an_open_pr_says_so_and_tells_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["remote", "add", "origin", "https://host.example/o/r"],
        ] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        let gh = dir.path().join("gh");
        crate::write_script(
            &gh,
            "#!/bin/sh\ncase \"$*\" in *\"--head s-1 \"*) echo '[{\"number\":7}]' ;; *) echo '[]' ;; esac\n",
        );
        let (gh, repo) = (gh.to_str().unwrap(), repo.to_str().unwrap());
        let fleet = Told(Default::default());
        let line = removed_line(&fleet, gh, repo, "/w/s-1", Some("s-1")).await;
        assert!(line.starts_with("removed "), "{line}");
        assert!(line.contains("o/r#7 is still open"), "{line}");
        assert!(line.ends_with("(coordinator told)"), "{line}");
        let told = fleet.0.lock().unwrap().clone();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].0, "agent:h/s/root");
        // No open PR (or no branch): the plain line, nobody told.
        let fleet = Told(Default::default());
        let line = removed_line(&fleet, gh, repo, "/w/s-2", Some("s-2")).await;
        assert!(!line.contains("still open") && fleet.0.lock().unwrap().is_empty());
        assert!(
            !removed_line(&fleet, gh, repo, "/w/x", None)
                .await
                .contains(';')
        );
    }

    struct Stub;

    impl Fleet for Stub {
        async fn snapshot(&self) -> Result<(State, Option<String>)> {
            unreachable!()
        }
        async fn task(&self, _: &str, _: Known) -> Result<String> {
            unreachable!()
        }
        async fn tell(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        async fn answer(&self, _: &str, _: &str, _: bool) -> Result<String> {
            unreachable!()
        }
        fn cli(&self) -> Vec<String> {
            unreachable!()
        }
        fn instance(&self) -> &str {
            "demo"
        }
        fn hub(&self) -> String {
            "http://hub".into()
        }
        fn host(&self) -> String {
            "h".into()
        }
    }

    #[test]
    fn the_board_draws_agents_and_their_findings() {
        let now = Utc::now();
        let mut state = State::default();
        state.agents.insert(
            "agent:h/s/builder".into(),
            Agent {
                host: "h".into(),
                kind: Some("claude".into()),
                pane: Some("w:1".into()),
                state: AgentState::Idle,
                since: now - chrono::Duration::hours(3),
                worked: None,
                finished: None,
                usage: None,
            },
        );
        let app = App {
            found: findings(&state, now),
            state,
            warn: Some("hub unreachable; local replica".into()),
            ..App::new()
        };
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| draw(f, &app, &Stub, now)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        for want in [
            "agent-fleet demo",
            "1 findings",
            "hub unreachable",
            "builder",
            "idle",
            "q Quit",
        ] {
            assert!(screen.contains(want), "missing {want:?}");
        }
    }

    #[test]
    fn a_rule_row_puts_expiry_in_for_and_explains_itself() {
        use crate::event::{Actor, ActorKind, Envelope, Event, ResourceHold, ResourceLimit};
        let now = Utc::now();
        let until = now + chrono::Duration::hours(3);
        let far = now + chrono::Duration::hours(30);
        let old = now - chrono::Duration::hours(2);
        let ev = |body| {
            Envelope::new(
                Actor {
                    kind: ActorKind::Host,
                    id: "host:h".into(),
                },
                "p",
                body,
            )
            .into_event()
        };
        let hold = |pid: u32, cwd: &str, mins: i64, waiting: bool| {
            let h = ResourceHold {
                host: "h".into(),
                resource: "sim".into(),
                pid,
                cwd: Some(cwd.into()),
                scope: None,
                since: now - chrono::Duration::minutes(mins),
            };
            if waiting {
                Known::ResourceWaiting(h)
            } else {
                Known::ResourceTaken(h)
            }
        };
        let rule = |name: &str, until| {
            Known::ResourceLimit(ResourceLimit {
                host: "h".into(),
                name: name.into(),
                resources: vec!["sim".into()],
                max: 1,
                until,
            })
        };
        let events: Vec<Event> = [
            hold(42, "/w/a", 5, false),
            hold(43, "/w/b", 1, true),
            rule("native", Some(until)),
            rule("far", Some(far)),
            rule("old", Some(old)),
        ]
        .into_iter()
        .map(ev)
        .collect();
        let state = State::fold(&events);
        let mut tables: [TableState; 6] = Default::default();
        tables[Tab::Resources as usize].select(Some(1));
        let mut app = App {
            tab: Tab::Resources,
            tables,
            found: findings(&state, now),
            state,
            ..App::new()
        };
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let local = |t: DateTime<Utc>| t.with_timezone(&chrono::Local);
        let today = |t: DateTime<Utc>| local(t).date_naive() == local(now).date_naive();
        let want_for = |u: DateTime<Utc>| {
            format!(
                "→{}",
                local(u).format(if today(u) { "%H:%M" } else { "%b%d" })
            )
        };
        let rows = |term: &Terminal<TestBackend>| -> Vec<String> {
            term.backend()
                .buffer()
                .content()
                .chunks(120)
                .map(|r| r.iter().map(|c| c.symbol()).collect())
                .collect()
        };
        term.draw(|f| draw(f, &app, &Stub, now)).unwrap();
        let screen = rows(&term);
        let find = |name: &str| screen.iter().find(|r| r.contains(name)).unwrap();
        assert!(
            find("h:native").contains(&want_for(until)),
            "FOR has the expiry"
        );
        let old_row = find("h:old");
        assert!(old_row.contains("expired") && !old_row.contains('→'));
        assert!(find("h:far").contains(&want_for(far)), "FOR has the date");
        let detail = screen.join("\n");
        assert!(detail.contains(&format!("until {}", crate::ui::at(now, far))));
        app.tables[Tab::Resources as usize].select(Some(2));
        term.draw(|f| draw(f, &app, &Stub, now)).unwrap();
        let screen = rows(&term).join("\n");
        for want in [
            "at most 1 of sim".to_string(),
            format!("until {}", crate::ui::at(now, until)),
            "sim: held by pid 42 in a".to_string(),
            "1 waiting".to_string(),
        ] {
            assert!(screen.contains(&want), "missing {want:?}");
        }
        app.tables[Tab::Resources as usize].select(Some(3));
        term.draw(|f| draw(f, &app, &Stub, now)).unwrap();
        let screen = rows(&term).join("\n");
        assert!(screen.contains(&format!("expired at {}", crate::ui::at(now, old))));
    }

    /// A fleet: a lead agent up, a task with a blocker and a dependency, a lead's open ask, a
    /// capacity rule and a hung process on this host.
    fn fleet_state() -> State {
        use crate::event::{
            Actor, ActorKind, AgentRole, AskRaised, BlockerRaised, Dep, Envelope, Level,
            ProcessHung, ResourceLimit, TaskCreated,
        };
        let lead = "agent:h/s/lead";
        let by = Actor {
            kind: ActorKind::Agent,
            id: lead.into(),
        };
        let ev = |body, task: Option<&str>| {
            let e = Envelope::new(by.clone(), "p", body);
            match task {
                Some(t) => e.task(t),
                None => e,
            }
            .into_event()
        };
        let t = Some("task:p/1");
        let created = |title: &str| {
            Known::TaskCreated(TaskCreated {
                title: title.into(),
                url: None,
            })
        };
        let mut s = State::fold(&[
            ev(created("first"), Some("task:p/0")),
            ev(created("second"), t),
            ev(
                Known::DepAdded(Dep {
                    on: "task:p/0".into(),
                }),
                t,
            ),
            ev(
                Known::BlockerRaised(BlockerRaised {
                    what: "CI".into(),
                    owner: None,
                    unblocks_when: None,
                    pid: None,
                    started: None,
                }),
                t,
            ),
            ev(
                Known::AgentRole(AgentRole {
                    agent: lead.into(),
                    level: Some(Level::RootCoordinator),
                    lead: None,
                    domain: None,
                }),
                None,
            ),
            ev(
                Known::AskRaised(AskRaised {
                    question: "ship it?".into(),
                    options: vec!["yes".into(), "no".into()],
                    recommendation: Some("yes".into()),
                    context: None,
                }),
                t,
            ),
            ev(
                Known::ResourceLimit(ResourceLimit {
                    host: "h".into(),
                    name: "native".into(),
                    resources: vec!["sim".into()],
                    max: 1,
                    until: None,
                }),
                None,
            ),
        ]);
        s.agents.insert(
            lead.into(),
            Agent {
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Working,
                since: Utc::now(),
                worked: None,
                finished: None,
                usage: None,
            },
        );
        s.hung.insert(
            "h:30".into(),
            ProcessHung {
                host: "h".into(),
                pid: 30,
                pids: vec![30, 31],
                agent: Some(lead.into()),
                command: Some("sleep 999".into()),
                started: None,
                evidence: Some("no output".into()),
                since: Some(Utc::now()),
                checked: Some(Utc::now()),
            },
        );
        s
    }

    /// The command line an action left: typed (plus what the operator would add), or waiting
    /// for `y`.
    fn line(app: &App, rest: &str) -> Vec<String> {
        match &app.mode {
            Mode::Input(Input::Command, buf) => words(&format!("{buf}{rest}")).unwrap(),
            Mode::Confirm(_, Pending::Run(args)) => args.clone(),
            _ => panic!("no command"),
        }
    }

    #[tokio::test]
    async fn every_action_builds_a_command_the_cli_parses() {
        let fleet = Told(Default::default());
        let mut app = App {
            state: fleet_state(),
            ..App::new()
        };
        app.found = findings(&app.state, Utc::now());
        let hung = app.found.iter().position(|f| f.subject == "h:30").unwrap();
        app.tables[Tab::Findings as usize].select(Some(hung));
        let blocker = app.state.blockers.keys().next().unwrap().clone();
        let cases = [
            (
                Tab::Agents,
                'R',
                "implementer --lead coordinator",
                "role lead implementer --lead coordinator".to_string(),
            ),
            (Tab::Agents, 'H', "codex", "handoff lead --to codex".into()),
            (
                Tab::Tasks,
                'n',
                "task:p/2 --title 'two words'",
                "task add task:p/2 --title two words".into(),
            ),
            (
                Tab::Tasks,
                'a',
                "lead",
                "task add task:p/0 --to lead".into(),
            ),
            (
                Tab::Tasks,
                'b',
                "'waits on CI'",
                "block task:p/0 waits on CI".into(),
            ),
            (Tab::Findings, 'K', "", "kill 30 31".into()),
            (
                Tab::Resources,
                'L',
                "sims sim --max 2",
                "resource limit sims sim --max 2".into(),
            ),
            (
                Tab::Resources,
                'U',
                "",
                "resource unlimit native --host h".into(),
            ),
            (Tab::Asks, ':', "self-update", "self-update".into()),
            (Tab::Agents, 'S', "o/r#5", "spawn o/r#5".into()),
            (
                Tab::Agents,
                'c',
                "hello",
                "tell coordinator NOTE tui hello".into(),
            ),
        ];
        for (tab, key, rest, want) in cases {
            app.tab = tab;
            action(&mut app, key, &fleet).await;
            let args = line(&app, rest);
            assert_eq!(args.join(" "), want, "{key}");
            let argv = std::iter::once("agent-fleet".to_string()).chain(args);
            assert!(crate::Cli::try_parse_from(argv).is_ok(), "{want}");
            app.mode = Mode::Normal;
        }
        // The second task waits for the first and has the blocker.
        app.tab = Tab::Tasks;
        app.tables[Tab::Tasks as usize].select(Some(1));
        for (key, rest, want) in [
            ('B', "fixed", format!("unblock {blocker} --note fixed")),
            ('p', "task:p/9", "dep add task:p/1 task:p/9".into()),
            ('P', "", "dep rm task:p/1 task:p/0".into()),
        ] {
            action(&mut app, key, &fleet).await;
            let args = line(&app, rest);
            assert_eq!(args.join(" "), want);
            let argv = std::iter::once("agent-fleet".to_string()).chain(args);
            assert!(crate::Cli::try_parse_from(argv).is_ok(), "{want}");
            app.mode = Mode::Normal;
        }
        // tell: typed, then the CLI's own `tell` waits for `y`.
        app.tab = Tab::Agents;
        action(&mut app, 't', &fleet).await;
        let Mode::Input(what, _) = std::mem::replace(&mut app.mode, Mode::Normal) else {
            panic!("no input")
        };
        entered(&mut app, &fleet, what, "look at #3".into());
        assert_eq!(
            line(&app, "").join(" "),
            "tell agent:h/s/lead NOTE tui look at #3"
        );
        // An ask: approved through the board's endpoint after `y`.
        app.tab = Tab::Asks;
        action(&mut app, 'A', &fleet).await;
        let Mode::Input(what, _) = std::mem::replace(&mut app.mode, Mode::Normal) else {
            panic!("no input")
        };
        entered(&mut app, &fleet, what, "yes".into());
        let Mode::Confirm(_, pending) = std::mem::replace(&mut app.mode, Mode::Normal) else {
            panic!("no confirm")
        };
        assert!(act(&fleet, pending).await.starts_with("answered "));
        let ask = app.state.asks.keys().next().unwrap().clone();
        assert_eq!(fleet.0.lock().unwrap()[0], (ask, "yes true".to_string()));
    }

    #[test]
    fn the_command_line_reads_at_once_asks_before_a_write_and_dry_runs_a_spawn() {
        let w = |s: &str| words(s).unwrap();
        assert_eq!(w(r#"block t "a b" 'c d'"#), ["block", "t", "a b", "c d"]);
        assert!(words("block t \"open").is_err());
        assert_eq!(plan(w("doctor")), Ok(Plan::Read(w("doctor"))));
        assert_eq!(
            plan(w("doctor --apply")),
            Ok(Plan::Write(w("doctor --apply")))
        );
        assert_eq!(plan(w("role lead")), Ok(Plan::Read(w("role lead"))));
        assert_eq!(
            plan(w("role lead --clear")),
            Ok(Plan::Write(w("role lead --clear")))
        );
        assert_eq!(
            plan(w("spawn o/r#5")),
            Ok(Plan::DryRun(w("spawn o/r#5 --dry-run"), w("spawn o/r#5")))
        );
        assert_eq!(
            plan(w("dev clean --execute")),
            Ok(Plan::Write(w("dev clean --execute")))
        );
        for read in [
            "help",
            "api schema",
            "task add --help",
            "block -h",
            "dev clean",
            "dev ls",
        ] {
            assert_eq!(plan(w(read)), Ok(Plan::Read(w(read))));
        }
        assert!(plan(w("hub")).is_err() && plan(vec![]).is_err());
    }

    #[test]
    fn a_command_runs_in_the_background_and_its_last_line_is_the_outcome() {
        let fleet = Told(Default::default());
        let mut app = App::new();
        start(&mut app, &fleet, w2("task ls"), None);
        assert_eq!(app.jobs, ["task ls"]);
        let done = app.rx.recv_timeout(Duration::from_secs(10)).unwrap();
        finished(&mut app, done);
        assert!(app.jobs.is_empty() && matches!(app.mode, Mode::Normal));
        assert_eq!(app.note.as_ref().unwrap().0, "task ls: task ls");
        // A dry run opens its output with the spawn behind `y`; a failure opens its output.
        start(
            &mut app,
            &fleet,
            w2("spawn o/r#5 --dry-run"),
            Some(w2("spawn o/r#5")),
        );
        let done = app.rx.recv_timeout(Duration::from_secs(10)).unwrap();
        finished(&mut app, done);
        assert!(matches!(&app.mode, Mode::Output(o, 0) if o.then == Some(w2("spawn o/r#5"))));
        app.mode = Mode::Normal;
        finished(
            &mut app,
            Done {
                args: w2("kill 30"),
                ok: false,
                out: "agent-fleet kill: REFUSED not yours\nError: killed nothing\n".into(),
                then: None,
                on: None,
            },
        );
        assert!(matches!(&app.mode, Mode::Output(o, _) if o.text.contains("REFUSED")));
    }

    fn w2(s: &str) -> Vec<String> {
        words(s).unwrap()
    }

    /// One instance of `tui --all`: its state (none: its hub down with no replica), a warning
    /// when it is read off a replica, and the task writes it receives.
    struct Inst {
        name: &'static str,
        state: Option<State>,
        warn: Option<&'static str>,
        did: std::sync::Mutex<Vec<String>>,
    }

    impl Inst {
        fn new(name: &'static str, state: Option<State>, warn: Option<&'static str>) -> Inst {
            Inst {
                name,
                state,
                warn,
                did: Default::default(),
            }
        }
    }

    impl Fleet for Inst {
        async fn snapshot(&self) -> Result<(State, Option<String>)> {
            match &self.state {
                Some(s) => Ok((s.clone(), self.warn.map(str::to_string))),
                None => Err(anyhow::anyhow!("{}: connection refused", self.name)),
            }
        }
        async fn task(&self, task: &str, _: Known) -> Result<String> {
            self.did.lock().unwrap().push(task.to_string());
            Ok("e1".into())
        }
        async fn tell(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        async fn answer(&self, _: &str, _: &str, _: bool) -> Result<String> {
            unreachable!()
        }
        fn cli(&self) -> Vec<String> {
            // Prints the command line it was given: the instance flag, then the command.
            [
                "sh",
                "-c",
                "echo \"$@\"",
                "agent-fleet",
                "--instance",
                self.name,
            ]
            .map(str::to_string)
            .to_vec()
        }
        fn instance(&self) -> &str {
            self.name
        }
        fn hub(&self) -> String {
            format!("http://{}", self.name)
        }
        fn host(&self) -> String {
            "h".into()
        }
    }

    fn one_agent() -> State {
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/builder".into(),
            Agent {
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Working,
                since: Utc::now(),
                worked: None,
                finished: None,
                usage: None,
            },
        );
        s
    }

    fn screen(app: &App, fleet: &impl Fleet) -> String {
        let mut term = Terminal::new(TestBackend::new(160, 30)).unwrap();
        term.draw(|f| draw(f, app, fleet, Utc::now())).unwrap();
        let w = term.backend().buffer().area.width as usize;
        let cells: Vec<&str> = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        cells.chunks(w).map(|l| l.concat() + "\n").collect()
    }

    #[tokio::test]
    async fn all_instances_each_show_their_own_state_and_a_down_hub_breaks_none() {
        let fleets = [
            Inst::new("alpha", Some(one_agent()), None),
            Inst::new("beta", None, None),
            Inst::new(
                "gamma",
                Some(fleet_state()),
                Some("hub unreachable; local replica"),
            ),
        ];
        let mut app = App::new();
        refresh(&mut app, &fleets).await;
        // The one on screen is live; every instance has its line, the down one says so.
        assert!(app.state.agents.contains_key("agent:h/s/builder") && app.warn.is_none());
        let text: Vec<(&str, &str)> = (app.instances.iter())
            .map(|s| (s.name.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(text[0], ("alpha", "1 agents, 1 working, 0 findings"));
        assert_eq!(text[1], ("beta", "no state"));
        assert!(text[2].1.starts_with("replica: "), "{text:?}");
        let shown = screen(&app, &fleets[0]);
        assert!(
            shown.contains("agent-fleet alpha · http://alpha"),
            "{shown}"
        );
        assert!(shown.contains(" beta  no state"), "{shown}");
        assert!(shown.contains("[ ] Instance"), "{shown}");

        // To the down one: its error, and none of alpha's rows under its name.
        assert!(press(&mut app, &fleets, KeyCode::Char(']')).await);
        refresh(&mut app, &fleets).await;
        assert_eq!(app.on, 1);
        assert!(app.state.agents.is_empty());
        assert_eq!(app.warn.as_deref(), Some("beta: connection refused"));
        let shown = screen(&app, &fleets[1]);
        assert!(
            shown.contains("agent-fleet beta") && !shown.contains("builder"),
            "{shown}"
        );
        assert!(shown.contains("beta: connection refused"), "{shown}");

        // On to the replica one, and `[` back; alpha is still live meanwhile.
        press(&mut app, &fleets, KeyCode::Char(']')).await;
        refresh(&mut app, &fleets).await;
        assert_eq!(app.warn.as_deref(), Some("hub unreachable; local replica"));
        assert!(app.state.tasks.contains_key("task:p/1"));
        press(&mut app, &fleets, KeyCode::Char('[')).await;
        press(&mut app, &fleets, KeyCode::Char('[')).await;
        refresh(&mut app, &fleets).await;
        assert_eq!(app.on, 0);
        assert!(app.state.agents.contains_key("agent:h/s/builder"));

        // One instance: no strip, and `[`/`]` do nothing, as before `--all`.
        let one = [Inst::new("alpha", Some(one_agent()), None)];
        let mut app = App::new();
        refresh(&mut app, &one).await;
        press(&mut app, &one, KeyCode::Char(']')).await;
        assert_eq!(app.on, 0);
        let shown = screen(&app, &one[0]);
        assert!(
            !shown.contains("[ ]") && !shown.contains("no state"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn an_action_goes_to_the_instance_on_screen() {
        let fleets = [
            Inst::new("alpha", Some(fleet_state()), None),
            Inst::new("beta", Some(fleet_state()), None),
        ];
        let mut app = App::new();
        refresh(&mut app, &fleets).await;
        press(&mut app, &fleets, KeyCode::Char(']')).await;
        refresh(&mut app, &fleets).await;

        // A key action: `d` on the first task, `y`. The prompt names the instance.
        app.tab = Tab::Tasks;
        press(&mut app, &fleets, KeyCode::Char('d')).await;
        assert!(screen(&app, &fleets[1]).contains("beta: mark done task:p/0?"));
        press(&mut app, &fleets, KeyCode::Char('y')).await;
        assert!(fleets[0].did.lock().unwrap().is_empty());
        assert_eq!(*fleets[1].did.lock().unwrap(), ["task:p/0"]);
        assert!(
            app.note
                .as_ref()
                .unwrap()
                .0
                .starts_with("beta: mark done: ")
        );

        // A typed command runs the CLI with beta's own flags.
        press(&mut app, &fleets, KeyCode::Char(':')).await;
        for c in "task done 1".chars() {
            press(&mut app, &fleets, KeyCode::Char(c)).await;
        }
        press(&mut app, &fleets, KeyCode::Enter).await;
        assert!(screen(&app, &fleets[1]).contains("beta: run agent-fleet task done 1?"));
        press(&mut app, &fleets, KeyCode::Char('y')).await;
        assert_eq!(app.jobs, ["beta: task done 1"]);
        let done = app.rx.recv_timeout(Duration::from_secs(10)).unwrap();
        finished(&mut app, done);
        assert!(app.jobs.is_empty());
        assert_eq!(
            app.note.as_ref().unwrap().0,
            "beta: task done: --instance beta task done 1"
        );
    }

    #[test]
    fn the_asks_tab_shows_the_inbox_and_a_finding_its_processes() {
        let mut app = App {
            tab: Tab::Asks,
            state: fleet_state(),
            ..App::new()
        };
        app.found = findings(&app.state, Utc::now());
        let mut term = Terminal::new(TestBackend::new(140, 30)).unwrap();
        let screen = |term: &Terminal<TestBackend>| -> String {
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        term.draw(|f| draw(f, &app, &Stub, Utc::now())).unwrap();
        for want in [
            "6 Asks",
            "open",
            "ship it?",
            "1. yes",
            "recommends: yes",
            "A Approve",
        ] {
            assert!(screen(&term).contains(want), "missing {want:?}");
        }
        app.tab = Tab::Findings;
        let hung = app.found.iter().position(|f| f.subject == "h:30").unwrap();
        app.tables[Tab::Findings as usize].select(Some(hung));
        term.draw(|f| draw(f, &app, &Stub, Utc::now())).unwrap();
        assert!(screen(&term).contains("proc 31: hung: no output"));
    }
}
