//! `agent-fleet tui`: the fleet on one screen, live, with the few actions an operator takes. Every
//! write goes through the same commands as the CLI; local actions (kill, remove) ask first.

use std::path::Path;
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
}

const TABS: [(Tab, &str); 5] = [
    (Tab::Agents, "1 Agents"),
    (Tab::Worktrees, "2 Worktrees"),
    (Tab::Tasks, "3 Tasks"),
    (Tab::Findings, "4 Findings"),
    (Tab::Resources, "5 Resources"),
];

/// An action waiting for `y`.
enum Pending {
    Kill(Vec<u32>, String),
    Remove { repo: String, path: String },
    Task(String, Known, &'static str),
}

enum Mode {
    Normal,
    Confirm(String, Pending),
    Tell { agent: String, buf: String },
    Help,
}

struct App {
    tab: Tab,
    tables: [TableState; 5],
    state: State,
    found: Vec<Finding>,
    warn: Option<String>,
    note: Option<(String, Instant)>,
    mode: Mode,
    loaded: Option<Instant>,
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
        "heavy" | "unpushed" | "forgotten" | "queued" | "refused" => Color::Yellow,
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
    fn at(&self) -> usize {
        self.tables[self.tab as usize].selected().unwrap_or(0)
    }

    fn rows(&self) -> usize {
        match self.tab {
            Tab::Agents => self.state.agents.len(),
            Tab::Worktrees => self.worktree_keys().len(),
            Tab::Tasks => self.state.tasks.len(),
            Tab::Findings => self.found.len(),
            Tab::Resources => self.state.resources.len() + self.state.limits.len(),
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

pub async fn run(fleet: &impl Fleet) -> Result<()> {
    let mut term = ratatui::init();
    let result = main_loop(&mut term, fleet).await;
    ratatui::restore();
    result
}

async fn main_loop(term: &mut DefaultTerminal, fleet: &impl Fleet) -> Result<()> {
    let mut app = App {
        tab: Tab::Agents,
        tables: Default::default(),
        state: State::default(),
        found: Vec::new(),
        warn: None,
        note: None,
        mode: Mode::Normal,
        loaded: None,
    };
    loop {
        if app.loaded.is_none_or(|t| t.elapsed() >= REFRESH) {
            match fleet.snapshot().await {
                Ok((s, warn)) => {
                    app.found = findings(&s, Utc::now());
                    app.state = s;
                    app.warn = warn;
                }
                Err(e) => app.warn = Some(format!("{e:#}")),
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
        let now = Utc::now();
        term.draw(|f| draw(f, &app, fleet, now))?;
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
        match std::mem::replace(&mut app.mode, Mode::Normal) {
            Mode::Help => {}
            Mode::Confirm(_, pending) if k.code == KeyCode::Char('y') => {
                let msg = act(fleet, pending).await;
                app.say(msg);
                app.loaded = None;
            }
            Mode::Confirm(..) => app.say("cancelled"),
            Mode::Tell { agent, mut buf } => match k.code {
                KeyCode::Esc => app.say("cancelled"),
                KeyCode::Enter if !buf.trim().is_empty() => {
                    let msg = match fleet.tell(&agent, buf.trim()).await {
                        Ok(id) => format!("rang {}: {id}", short(&agent)),
                        Err(e) => format!("tell failed: {e:#}"),
                    };
                    app.say(msg);
                    app.loaded = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                    app.mode = Mode::Tell { agent, buf };
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    app.mode = Mode::Tell { agent, buf };
                }
                _ => app.mode = Mode::Tell { agent, buf },
            },
            Mode::Normal => {
                if !key(&mut app, k.code, fleet).await {
                    return Ok(());
                }
            }
        }
    }
}

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
        KeyCode::Char(c @ '1'..='5') => app.tab = TABS[(c as u8 - b'1') as usize].0,
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
                app.mode = Mode::Tell {
                    agent: agent.clone(),
                    buf: String::new(),
                };
            }
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
            app.mode = Mode::Confirm(format!("{verb} {task}?"), Pending::Task(task, body, verb));
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
                },
            );
        }
        _ => {}
    }
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
        Pending::Task(task, body, verb) => match fleet.task(&task, body).await {
            Ok(id) => format!("{verb}: {id}"),
            Err(e) => format!("{verb} failed: {e:#}"),
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
        Pending::Remove { repo, path } => {
            let out = match hook(&repo) {
                Some(h) => {
                    tokio::process::Command::new(h)
                        .args(["retire", &path])
                        .output()
                        .await
                }
                None => {
                    tokio::process::Command::new("git")
                        .args(["-C", &repo, "worktree", "remove", &path])
                        .output()
                        .await
                }
            };
            match out {
                Ok(o) if o.status.success() => format!("removed {}", home(&path)),
                Ok(o) => format!(
                    "refused: {}",
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .last()
                        .unwrap_or("?")
                ),
                Err(e) => format!("could not run: {e}"),
            }
        }
    }
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
        Constraint::Length(1),
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
    f.render_widget(Paragraph::new(summary), head);

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

    let keys = match (&app.mode, app.tab) {
        (Mode::Confirm(q, _), _) => Line::styled(
            format!("{q}  y yes · any other key no"),
            Style::new().fg(Color::Yellow).bold(),
        ),
        (Mode::Tell { agent, buf }, _) => Line::from(vec![
            Span::styled(format!("tell {}: ", short(agent)), Style::new().bold()),
            Span::raw(buf.clone()),
            Span::styled("█", Style::new().fg(DIM)),
            Span::styled("   Enter send · Esc cancel", Style::new().fg(DIM)),
        ]),
        (_, tab) => {
            let note = app
                .note
                .as_ref()
                .filter(|(_, t)| t.elapsed() < Duration::from_secs(6))
                .map(|(n, _)| format!("{n}   "))
                .unwrap_or_default();
            let own = match tab {
                Tab::Agents => "t Tell · f Focus · ",
                Tab::Worktrees => "x Remove · K Kill · ",
                Tab::Tasks => "d Done · u Release · x Cancel · ",
                Tab::Findings => "Enter Go to · ",
                Tab::Resources => "",
            };
            Line::from(vec![
                Span::styled(note, Style::new().fg(Color::Cyan)),
                Span::raw(format!(
                    "↑↓/jk Move · ←→/1-5 Tab · {own}r Refresh · ? Help · q Quit"
                )),
            ])
        }
    };
    f.render_widget(
        Paragraph::new(vec![Line::styled(rule, Style::new().fg(DIM)), keys]),
        foot,
    );

    if matches!(app.mode, Mode::Help) {
        help(f);
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
            Row::new(vec![
                Cell::from(short(id).to_string()),
                Cell::from(a.host.clone()),
                Cell::from(state_name(a.state)).fg(agent_color(a.state)),
                Cell::from(ago(now, a.since)),
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
                    a.kind.as_deref().unwrap_or("-")
                )),
            ])];
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
            l.extend(finding_lines(app, id));
            l
        }
    };
    (
        vec![
            "AGENT", "HOST", "STATE", "FOR", "CPU", "MEM", "TASK", "BRANCH", "!",
        ],
        vec![
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(7),
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
        vec![
            Line::styled(
                format!("{} · {}", f.kind, home(&f.subject)),
                Style::new().bold(),
            ),
            Line::raw(f.detail.clone()),
        ]
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
    let at = app.at();
    let about = if let Some((key, r)) = s.resources.iter().nth(at) {
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
        Line::raw("← → / 1-5     tabs            r           refresh now (it refreshes every 2 s)"),
        Line::raw(""),
        Line::raw("Agents        t  ring the agent's doorbell (agent-fleet tell)"),
        Line::raw("              f  focus its herdr pane (same host and session)"),
        Line::raw(
            "Worktrees     x  remove a worktree no agent works in (the repo's .agent-fleet/hook retire,",
        ),
        Line::raw("                 else git worktree remove, which refuses unsaved work)"),
        Line::raw("              K  kill the processes left in it (TERM; only those still there)"),
        Line::raw("Tasks         d  done · u release · x cancel"),
        Line::raw("Findings      Enter  go to the agent, worktree or resource"),
        Line::raw("Resources     locks the hosts read: holder, queue, capacity rules"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Agent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
                usage: None,
            },
        );
        let app = App {
            tab: Tab::Agents,
            tables: Default::default(),
            found: findings(&state, now),
            state,
            warn: Some("hub unreachable; local replica".into()),
            note: None,
            mode: Mode::Normal,
            loaded: None,
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
        let mut tables: [TableState; 5] = Default::default();
        tables[Tab::Resources as usize].select(Some(1));
        let mut app = App {
            tab: Tab::Resources,
            tables,
            found: findings(&state, now),
            state,
            warn: None,
            note: None,
            mode: Mode::Normal,
            loaded: None,
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
}
