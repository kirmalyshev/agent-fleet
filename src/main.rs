mod bench;
mod client;
mod doctor;
mod event;
mod host;
mod hub;
mod kill;
mod quota;
mod scan;
mod signal;
mod spawn;
mod state;
mod stats;
mod store;
mod stuck;
mod tui;
mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::json;

use client::{Client, Paths, Sent};
use event::*;
use state::State;
use store::Store;

/// One event log for a fleet of coding agents: presence, tasks, blockers, asks and doorbells.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Which fleet: one instance per project or product, each with its own hub, log, token,
    /// spool and replica.
    #[arg(long, global = true, env = "AGENT_FLEET_INSTANCE", default_value = client::DEFAULT_INSTANCE)]
    instance: String,
    /// Hub URL [default: the one this instance's hub wrote, else http://127.0.0.1:7710].
    #[arg(long, global = true, env = "AGENT_FLEET_HUB")]
    hub: Option<String>,
    /// Bearer token file [default: <data dir>/token].
    #[arg(long, global = true, env = "AGENT_FLEET_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Who is writing: `agent:<host>/<session>/<name>`. Derived from herdr when run inside a pane.
    #[arg(long = "as", global = true, env = "AGENT_FLEET_AGENT")]
    me: Option<String>,
    /// Project for events that do not name one through a task id.
    #[arg(
        long,
        global = true,
        env = "AGENT_FLEET_PROJECT",
        default_value = "default"
    )]
    project: String,
    /// This machine's name in agent and host ids; `agent-fleet host` and the CLI must agree on it
    /// [default: the system hostname].
    #[arg(long, global = true, env = "AGENT_FLEET_HOST")]
    host_name: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the hub: the log, the fold and the sequencer, over HTTP + SSE.
    Hub {
        #[arg(long, default_value = "127.0.0.1:7710")]
        listen: SocketAddr,
        /// [default: <data dir>/hub.db]
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Run the host agent for one herdr session: presence, disk, replica, doorbell delivery,
    /// and spawn requests for this host.
    Host {
        #[arg(long, env = "HERDR_SOCKET_PATH")]
        herdr_socket: PathBuf,
        #[arg(long, env = "HERDR_SESSION")]
        session: String,
        /// A repository whose worktrees to watch even when no agent works in it, as
        /// `<owner/repo>=<checkout>` also the checkout a `spawn.requested` runs against.
        /// Repeatable.
        #[arg(long = "repo", value_parser = parse_repo)]
        repos: Vec<(Option<String>, PathBuf)>,
        /// A lock directory to report as a resource, `<name>=<dir>`. Repeatable.
        #[arg(long = "lock", value_parser = parse_lock)]
        locks: Vec<(String, PathBuf)>,
        /// Minutes an agent may idle or block after working before its lead is told STALLED.
        #[arg(long = "stall-min", default_value_t = spawn::STALL_MIN)]
        stall_min: u64,
        /// Act on a stuck agent — Enter/Escape/continue keystrokes and handoffs. Off, the
        /// host only classifies, records `agent.stuck`, and tells the lead.
        #[arg(long)]
        stuck_act: bool,
    },
    /// Protocol metadata.
    Api {
        #[command(subcommand)]
        cmd: ApiCmd,
    },
    /// What every agent is doing.
    Ps,
    /// The agent-fleet instances on this machine and the hub each one uses.
    Instances,
    /// The fleet on one screen, live: agents, worktrees, tasks, findings, and the actions on them.
    Tui,
    /// Tasks: `add` claims (exclusive), `done` finishes, `rm` cancels or releases.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Dependencies between tasks: <task> is blocked by <on>.
    Dep {
        #[command(subcommand)]
        cmd: DepCmd,
    },
    /// Raise a blocker on a task. Prints the blocker id.
    Block {
        task: String,
        what: String,
        /// Who or what has to act.
        #[arg(long)]
        owner: Option<String>,
        /// The condition that clears it.
        #[arg(long)]
        until: Option<String>,
    },
    /// Signal processes safely: TERM, then KILL after --grace. Refuses agents, panes, the fleet's
    /// daemons and anything above them, your own ancestors and other agents' processes; kills
    /// only your own processes and orphans `doctor` proved. A refusal kills nothing and is logged.
    Kill {
        #[arg(required = true)]
        pids: Vec<u32>,
        /// Seconds between TERM and KILL.
        #[arg(long, default_value_t = 5)]
        grace: u64,
    },
    /// Hooks for agent harnesses.
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Capacity rules for the resources hosts report.
    Resource {
        #[command(subcommand)]
        cmd: ResourceCmd,
    },
    /// Resolve a blocker by its id.
    Unblock {
        blocker: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Raise a question for an operator.
    Ask {
        task: String,
        question: String,
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long = "rec")]
        recommendation: Option<String>,
        #[arg(long)]
        context: Option<String>,
    },
    /// Declare an agent's level of responsibility (root-coordinator, domain-lead,
    /// implementation-lead, implementer), or with no level show its role, its chain of leads
    /// and its direct reports.
    Role {
        /// `me`, a full `agent:` id, or the short name of an agent that is up.
        agent: String,
        /// `root-coordinator`, `domain-lead`, `implementation-lead` or `implementer`.
        #[arg(value_name = "LEVEL", value_parser = parse_level)]
        level: Option<Level>,
        /// Whom the agent reports to [default: the caller, when it is not the subject].
        #[arg(long, conflicts_with = "clear")]
        lead: Option<String>,
        /// What a domain lead owns, e.g. `payments`.
        #[arg(long, conflicts_with = "clear")]
        domain: Option<String>,
        /// Remove the agent's role.
        #[arg(long, conflicts_with = "level")]
        clear: bool,
    },
    /// Spawn an agent on an issue: the project's `.agent-fleet/fleet.toml` picks the profile
    /// and host, the host agent runs it (worktree, env scrub, agent start, the brief) and
    /// answers `spawn.result`. Follows the hub until the result arrives (10 min).
    Spawn {
        /// `<owner/repo>#<issue>`
        target: String,
        /// Extra context for the rules and the `@@SCOPE@@` token.
        #[arg(long)]
        scope: Option<String>,
        /// The agent's name [default: the profile's prefix + the issue number].
        #[arg(long)]
        name: Option<String>,
        /// The new worktree's branch [default: the name].
        #[arg(long)]
        branch: Option<String>,
        /// Skip the rules and use this profile.
        #[arg(long)]
        profile: Option<String>,
        /// Spawn on this host [default: the rule's, else this machine].
        #[arg(long)]
        host: Option<String>,
        /// The agent.role level [default: the rule's, else implementer].
        #[arg(long, value_parser = parse_level)]
        level: Option<Level>,
        /// An extra brief token, `KEY=VALUE`. Repeatable.
        #[arg(long = "set", value_parser = parse_set)]
        set: Vec<(String, String)>,
        /// Print the decision and the rendered brief; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Write `spawn.requested` and return without waiting for the result.
        #[arg(long)]
        no_wait: bool,
    },
    /// Hand an agent's issue, branch and existing worktree to a new agent of `--to`
    /// (default: its profile's fallback), then close the replaced agent's pane and workspace.
    Handoff {
        /// The agent: a name or full id with a spawn record.
        agent: String,
        /// The profile to hand to.
        #[arg(long = "to")]
        to: Option<String>,
        /// Added to the handoff note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Ring an agent's doorbell: `[agent-fleet <from>→<to>] TYPE ref: text (event <id>)`.
    Tell {
        agent: String,
        #[arg(value_name = "TYPE")]
        kind: String,
        #[arg(value_name = "REF")]
        reference: String,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Numbers about the fleet: doorbell latency and loss, event lag, host and hub vitals.
    Stats {
        /// Window back from now: `90s`, `30m`, `24h`, `7d`.
        #[arg(long, default_value = "24h", value_parser = parse_window)]
        since: i64,
        #[arg(long)]
        json: bool,
    },
    /// What needs a look: stale, hung or heavy agents, duplicate work, forgotten worktrees,
    /// unpushed commits.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Load-test a throwaway in-process hub: post latency, throughput, follower lag, 409 races.
    Bench {
        #[arg(long, default_value_t = 1000)]
        events: usize,
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
    },
    /// The folded state.
    State {
        #[arg(long, required = true)]
        json: bool,
    },
    /// The board in the browser (live, every status), or with --text here.
    Ui {
        /// Print the board here instead of opening it in the browser.
        #[arg(long)]
        text: bool,
        /// Print the board's address instead of opening it (the token is not in it).
        #[arg(long, conflicts_with = "text")]
        url: bool,
    },
}

impl tui::Fleet for Ctx {
    async fn snapshot(&self) -> Result<(State, Option<String>)> {
        self.snapshot_state().await
    }

    async fn task(&self, task: &str, body: Known) -> Result<String> {
        self.task_event(task, body).await
    }

    async fn tell(&self, agent: &str, text: &str) -> Result<String> {
        let body = MsgSent {
            to: agent.to_string(),
            kind: "NOTE".into(),
            reference: "tui".into(),
            text: text.to_string(),
        };
        self.write(Envelope::new(
            self.me().await?,
            &self.cli.project,
            Known::MsgSent(body),
        ))
        .await
    }

    fn instance(&self) -> &str {
        &self.cli.instance
    }

    fn hub(&self) -> String {
        self.cli.hub.clone().unwrap_or_else(|| self.paths.hub_url())
    }

    fn host(&self) -> String {
        self.host_name()
    }
}

fn read_token(ctx: &Ctx) -> Result<String> {
    let file = ctx
        .cli
        .token_file
        .clone()
        .unwrap_or_else(|| ctx.paths.token());
    client::read_token(&file)
}

/// Opens the hub's board in the default browser, signed in: the token rides in the URL fragment,
/// which the browser never sends to a server, and the page drops it from the address bar.
fn open_board(hub: &str, token: &str) -> Result<()> {
    // `$BROWSER` (a command, maybe with arguments) wins, as for any Unix tool that opens a page.
    let default = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let browser = std::env::var("BROWSER").unwrap_or_default();
    let mut words = browser.split_whitespace();
    let opener = words.next().unwrap_or(default).to_string();
    let url = format!("{hub}/#token={token}");
    let status = std::process::Command::new(&opener)
        .args(words)
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("opened {hub}/ in the browser");
            Ok(())
        }
        _ => bail!(
            "could not run {opener}; open {hub}/ yourself and paste the token from the token file"
        ),
    }
}

fn parse_window(s: &str) -> Result<i64, String> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().map_err(|_| format!("not a window: {s}"))?;
    match unit {
        "" | "s" => Ok(n),
        "m" => Ok(n * 60),
        "h" => Ok(n * 3600),
        "d" => Ok(n * 86400),
        _ => Err(format!("unknown unit in {s}; use s, m, h or d")),
    }
}

#[derive(Subcommand)]
enum ApiCmd {
    /// Print the JSON Schema of events and state.
    Schema,
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Create a task if new and assign it. Refused (409) if someone holds it; never spooled.
    Add {
        task: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        url: Option<String>,
        /// Assignee [default: you].
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        lead: Option<String>,
    },
    /// Finish a task: completed, or with --failed failed. Frees it for a new claim.
    Done {
        task: String,
        #[arg(long)]
        failed: bool,
    },
    /// Cancel a task, or with --release only give it up.
    Rm {
        task: String,
        #[arg(long)]
        release: bool,
    },
    Ls {
        /// Include completed, failed and canceled tasks.
        #[arg(long)]
        all: bool,
    },
    Show {
        task: String,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// PreToolUse hook for Claude Code and Devin CLI: reads the tool call on stdin and refuses
    /// (exit 2) a shell command that runs a raw kill, pkill or killall.
    Bash,
    /// Claude Code status-line command: records the `rate_limits` it is fed for the host to
    /// publish as `agent.quota`, then runs an optional wrapped command on the same stdin and
    /// passes its stdout through. Always exits 0.
    Statusline {
        /// The status line to wrap, as `-- <cmd> <args>…`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Harness hook (Claude Code `StopFailure`/`Notification`/`PreCompact`/`Stop`/
    /// `UserPromptSubmit`, and any harness whose payload matches): maps the hook JSON on
    /// stdin to an `agent.signal` event and writes it. Never fails; outside herdr it exits 0.
    Signal,
}

#[derive(Subcommand)]
enum ResourceCmd {
    /// At most --max worktrees hold any of <resources> at once; `doctor` flags a breach.
    Limit {
        name: String,
        #[arg(required = true)]
        resources: Vec<String>,
        #[arg(long, default_value_t = 1)]
        max: u32,
        /// Expiry: `HH:MM` today, local time, or an RFC 3339 timestamp.
        #[arg(long, value_parser = parse_until)]
        until: Option<DateTime<Utc>>,
        /// [default: this machine]
        #[arg(long)]
        host: Option<String>,
    },
    /// Remove a rule.
    Unlimit {
        name: String,
        #[arg(long)]
        host: Option<String>,
    },
}

fn parse_until(s: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let t = chrono::NaiveTime::parse_from_str(s, "%H:%M")
        .map_err(|_| format!("not HH:MM or RFC 3339: {s}"))?;
    chrono::Local::now()
        .date_naive()
        .and_time(t)
        .and_local_timezone(chrono::Local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| format!("{s} does not exist today"))
}

fn parse_level(s: &str) -> Result<Level, String> {
    serde_json::from_value(json!(s)).map_err(|_| {
        format!(
            "not a level: {s} (root-coordinator, domain-lead, implementation-lead, implementer)"
        )
    })
}

fn parse_lock(s: &str) -> Result<(String, PathBuf), String> {
    match s.split_once('=') {
        Some((name, dir)) if !name.is_empty() && !dir.is_empty() => {
            Ok((name.to_string(), PathBuf::from(dir)))
        }
        _ => Err(format!("not <name>=<dir>: {s}")),
    }
}

/// `owner/repo` exactly: one slash, two non-empty parts, not a path.
fn is_repo_slug(s: &str) -> bool {
    !s.starts_with('/') && s.split('/').count() == 2 && s.split('/').all(|p| !p.is_empty())
}

/// `<owner/repo>=<checkout>` (watched and spawnable) or a bare `<dir>` (watched only). An `=`
/// whose left side is not `owner/repo` is part of the path.
fn parse_repo(s: &str) -> Result<(Option<String>, PathBuf), String> {
    match s.split_once('=') {
        Some((slug, dir)) if is_repo_slug(slug) && !dir.is_empty() => {
            Ok((Some(slug.to_string()), PathBuf::from(dir)))
        }
        _ if !s.is_empty() => Ok((None, PathBuf::from(s))),
        _ => Err(format!("not <owner/repo>=<dir> or <dir>: {s}")),
    }
}

fn parse_set(s: &str) -> Result<(String, String), String> {
    let Some((k, v)) = s.split_once('=') else {
        return Err(format!("not KEY=VALUE: {s}"));
    };
    if k.is_empty()
        || !k
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!("brief keys are A-Z, 0-9 and _: {k}"));
    }
    Ok((k.to_string(), v.to_string()))
}

#[derive(Subcommand)]
enum DepCmd {
    Add { task: String, on: String },
    Rm { task: String, on: String },
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn host_name(cli: &Cli) -> String {
    cli.host_name.clone().unwrap_or_else(|| {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    })
}

struct Ctx {
    cli: Cli,
    paths: Paths,
}

impl Ctx {
    fn host_name(&self) -> String {
        host_name(&self.cli)
    }

    fn client(&self) -> Result<Client> {
        let token_file = self
            .cli
            .token_file
            .clone()
            .unwrap_or_else(|| self.paths.token());
        Ok(Client::new(
            &self.cli.hub.clone().unwrap_or_else(|| self.paths.hub_url()),
            client::read_token(&token_file)?,
            self.paths.spool(),
        ))
    }

    /// `task:<project>/<n>`; a bare name is put in the current project.
    fn task_id(&self, t: &str) -> String {
        if t.starts_with("task:") {
            t.to_string()
        } else {
            format!("task:{}/{t}", self.cli.project)
        }
    }

    fn project_of(&self, task: &str) -> String {
        task.strip_prefix("task:")
            .and_then(|t| t.split_once('/'))
            .map_or(self.cli.project.clone(), |(p, _)| p.to_string())
    }

    async fn me(&self) -> Result<Actor> {
        if let Some(id) = &self.cli.me {
            return Ok(Actor {
                kind: ActorKind::Agent,
                id: id.clone(),
            });
        }
        if let (Ok("1"), Ok(pane), Ok(session), Ok(sock)) = (
            std::env::var("HERDR_ENV").as_deref(),
            std::env::var("HERDR_PANE_ID"),
            std::env::var("HERDR_SESSION"),
            std::env::var("HERDR_SOCKET_PATH"),
        ) {
            // The pane id stands in only for an agent herdr knows to be unnamed, as `agent-fleet host`
            // does; a failed lookup must not quietly give this agent a second identity.
            let got = host::herdr_call(sock.as_ref(), "agent.get", json!({ "target": pane }))
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "could not ask herdr which agent this pane holds ({err:#}); pass --as or set AGENT_FLEET_AGENT"
                    )
                })?;
            let name = got["agent"]["name"].as_str().map_or(pane, str::to_string);
            return Ok(Actor {
                kind: ActorKind::Agent,
                id: format!("agent:{}/{session}/{name}", self.host_name()),
            });
        }
        bail!("who is writing? pass --as agent:<host>/<session>/<name> or set AGENT_FLEET_AGENT")
    }

    /// The hub's state, or the local replica's when the hub is unreachable.
    async fn state(&self) -> Result<State> {
        let (s, note) = self.snapshot_state().await?;
        if let Some(note) = note {
            eprintln!("agent-fleet: {note}");
        }
        Ok(s)
    }

    /// The hub's state, or the local replica's with a note saying so when the hub is unreachable.
    async fn snapshot_state(&self) -> Result<(State, Option<String>)> {
        let unreachable = |err: &anyhow::Error| {
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_connect() || e.is_timeout())
        };
        match self.client()?.state().await {
            Ok(s) => Ok((s, None)),
            // Only an unreachable hub falls back; a 401 or a 500 is a problem to see, not hide.
            Err(err) if unreachable(&err) && self.paths.replica().exists() => {
                let s = State::fold(&Store::open(&self.paths.replica())?.all()?);
                let age = s
                    .last
                    .as_deref()
                    .and_then(|id| ulid::Ulid::from_string(id).ok())
                    .map(|u| ui::ago(Utc::now(), chrono::DateTime::<Utc>::from(u.datetime())))
                    .unwrap_or_else(|| "?".into());
                let note = format!("hub unreachable ({err}); local replica, last event {age} ago");
                Ok((s, Some(note)))
            }
            Err(err) => Err(err),
        }
    }

    /// A full agent id from a short name, among the agents that are up.
    async fn resolve_agent(&self, name: &str) -> Result<String> {
        // A pane id (`w1:p2`) has a colon too; only `agent:` makes a full id.
        if name.starts_with("agent:") {
            return Ok(name.to_string());
        }
        let s = self.state().await?;
        let found: Vec<&String> = s.agents.keys().filter(|a| short(a) == name).collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no agent named {name} is up (see `agent-fleet ps`)"),
            many => bail!(
                "{name} is ambiguous: {}",
                many.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// `me`, a full `agent:` id, or the short name of an agent that is up. A role can be
    /// declared before an agent's first `agent.up`, so a name that is not up is an error that
    /// asks for the full id.
    async fn role_agent(&self, name: &str) -> Result<String> {
        if name == "me" {
            return Ok(self.me().await?.id);
        }
        self.resolve_agent(name).await.map_err(|err| {
            if err.to_string().starts_with("no agent named") {
                anyhow::anyhow!(
                    "{err:#}; pass the full id (agent:<host>/<session>/<name>) to declare a role before the agent's first agent.up"
                )
            } else {
                err
            }
        })
    }

    async fn write(&self, e: Envelope<Known>) -> Result<String> {
        let (ty, id) = (e.body.type_name(), e.id.clone());
        match self.client()?.write(&e).await? {
            Sent::Accepted { duplicate: false } => println!("{ty} {id}"),
            Sent::Accepted { duplicate: true } => println!("{ty} {id} (already in the log)"),
            Sent::Spooled(why) => {
                println!("{ty} {id} spooled ({why}); replays on the next write")
            }
        }
        Ok(id)
    }

    async fn task_event(&self, task: &str, body: Known) -> Result<String> {
        let task = self.task_id(task);
        self.write(Envelope::new(self.me().await?, self.project_of(&task), body).task(task))
            .await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Api {
        cmd: ApiCmd::Schema,
    } = cli.cmd
    {
        println!("{}", serde_json::to_string_pretty(&api_schema())?);
        return Ok(());
    }
    if let Cmd::Hook { cmd } = &cli.cmd {
        // A hook must never break the program it wraps: its paths may be missing, and every
        // error is swallowed inside.
        if let Ok(paths) = Paths::from_env(&cli.instance) {
            match cmd {
                HookCmd::Bash => {
                    let mut input = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
                    if let Some(msg) = kill::hook(&input) {
                        eprintln!("{msg}");
                        std::process::exit(2);
                    }
                }
                HookCmd::Statusline { cmd } => {
                    quota::statusline(
                        cmd,
                        &paths,
                        cli.hub.as_deref(),
                        cli.token_file.as_deref(),
                        &cli.project,
                        &host_name(&cli),
                    )
                    .await
                }
                HookCmd::Signal => {
                    let mut input = String::new();
                    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
                    signal::hook(
                        &paths,
                        cli.hub.as_deref(),
                        cli.token_file.as_deref(),
                        &cli.project,
                        &host_name(&cli),
                        &input,
                    )
                    .await;
                }
            }
        }
        return Ok(());
    }
    let ctx = Ctx {
        paths: Paths::from_env(&cli.instance)?,
        cli,
    };
    let now = Utc::now();
    match &ctx.cli.cmd {
        Cmd::Api { .. } => unreachable!(),
        Cmd::Hub { listen, db } => {
            let token_file = ctx
                .cli
                .token_file
                .clone()
                .unwrap_or_else(|| ctx.paths.token());
            let token = client::read_or_create_token(&token_file)?;
            let db = db.clone().unwrap_or_else(|| ctx.paths.hub_db());
            hub::serve(*listen, &db, &ctx.paths.hub_file(), token).await?;
        }
        Cmd::Host {
            herdr_socket,
            session,
            repos,
            locks,
            stall_min,
            stuck_act,
        } => {
            let cfg = host::Config {
                host: ctx.host_name(),
                session: session.clone(),
                herdr_socket: herdr_socket.clone(),
                replica: ctx.paths.replica(),
                data_dir: ctx.paths.dir.clone(),
                repos: repos
                    .iter()
                    .map(|(_, d)| d.to_string_lossy().into_owned())
                    .collect(),
                checkouts: repos
                    .iter()
                    .filter_map(|(slug, d)| {
                        slug.clone()
                            .or_else(|| spawn::slug_of(d))
                            .map(|s| (s, d.clone()))
                    })
                    .collect(),
                locks: locks.clone(),
                stall_min: *stall_min,
                stuck_act: *stuck_act,
            };
            host::run(cfg, ctx.client()?).await?;
        }
        Cmd::Tui => tui::run(&ctx).await?,
        Cmd::Instances => {
            for (name, hub) in Paths::instances()? {
                let mark = if name == ctx.cli.instance { "*" } else { " " };
                println!("{mark} {name:<20} {hub}");
            }
        }
        Cmd::Ps => print!("{}", ui::ps(&ctx.state().await?, now)),
        Cmd::Stats { since, json } => {
            let v = ctx.client()?.stats(*since).await?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", ui::stats(&v)?);
            }
        }
        Cmd::Doctor { json } => {
            let f = doctor::findings(&ctx.state().await?, now);
            if *json {
                println!("{}", serde_json::to_string_pretty(&f)?);
            } else {
                print!("{}", ui::doctor(&f));
            }
        }
        Cmd::Bench {
            events,
            concurrency,
        } => print!("{}", bench::run(*events, *concurrency).await?),
        Cmd::State { .. } => println!("{}", serde_json::to_string_pretty(&ctx.state().await?)?),
        Cmd::Ui { text: true, .. } => print!("{}", ui::board(&ctx.state().await?, now)),
        Cmd::Ui { url, .. } => {
            let hub = tui::Fleet::hub(&ctx);
            if *url {
                println!("{hub}/");
            } else {
                open_board(&hub, &read_token(&ctx)?)?;
            }
        }
        Cmd::Task { cmd } => match cmd {
            TaskCmd::Add {
                task,
                title,
                url,
                to,
                lead,
            } => {
                let task = ctx.task_id(task);
                let project = ctx.project_of(&task);
                let me = ctx.me().await?;
                let agent = match to {
                    Some(a) => ctx.resolve_agent(a).await?,
                    None => me.id.clone(),
                };
                let client = ctx.client()?;
                let known = client.state().await.map_err(|err| {
                    anyhow::anyhow!("hub unreachable ({err}); task add is exclusive, so it is refused, not spooled")
                })?;
                if !known.tasks.contains_key(&task) {
                    let title = title.clone().unwrap_or_else(|| task.clone());
                    let created = Envelope::new(
                        me.clone(),
                        &project,
                        Known::TaskCreated(TaskCreated {
                            title,
                            url: url.clone(),
                        }),
                    );
                    client.write_exclusive(&created.task(&task)).await?;
                }
                if known.owner_of(&task) == Some(agent.as_str()) {
                    println!("{task} is already assigned to {agent}");
                    return Ok(());
                }
                let assigned = Envelope::new(
                    me,
                    &project,
                    Known::TaskAssigned(TaskAssigned {
                        agent: agent.clone(),
                        lead: lead.clone(),
                    }),
                );
                let id = assigned.id.clone();
                client.write_exclusive(&assigned.task(&task)).await?;
                println!("task.assigned {id}: {task} → {agent}");
            }
            TaskCmd::Rm {
                task,
                release: true,
            } => {
                ctx.task_event(task, Known::TaskUnassigned(TaskUnassigned { reason: None }))
                    .await?;
            }
            TaskCmd::Rm {
                task,
                release: false,
            } => {
                ctx.task_event(
                    task,
                    Known::TaskState(TaskStateChanged {
                        state: TaskState::Canceled,
                    }),
                )
                .await?;
            }
            TaskCmd::Done { task, failed } => {
                let state = if *failed {
                    TaskState::Failed
                } else {
                    TaskState::Completed
                };
                ctx.task_event(task, Known::TaskState(TaskStateChanged { state }))
                    .await?;
            }
            TaskCmd::Ls { all } => print!("{}", ui::tasks(&ctx.state().await?, *all, now)),
            TaskCmd::Show { task } => {
                let id = ctx.task_id(task);
                match ui::task(&ctx.state().await?, &id, now) {
                    Some(text) => print!("{text}"),
                    None => bail!("no task {id}"),
                }
            }
        },
        Cmd::Dep { cmd } => {
            let (task, body) = match cmd {
                DepCmd::Add { task, on } => (
                    task,
                    Known::DepAdded(Dep {
                        on: ctx.task_id(on),
                    }),
                ),
                DepCmd::Rm { task, on } => (
                    task,
                    Known::DepRemoved(Dep {
                        on: ctx.task_id(on),
                    }),
                ),
            };
            ctx.task_event(task, body).await?;
        }
        Cmd::Block {
            task,
            what,
            owner,
            until,
        } => {
            let body = BlockerRaised {
                what: what.clone(),
                owner: owner.clone(),
                unblocks_when: until.clone(),
            };
            ctx.task_event(task, Known::BlockerRaised(body)).await?;
        }
        Cmd::Hook { .. } => unreachable!("handled before the context"),
        Cmd::Kill { pids, grace } => {
            let t = kill::table()?;
            let me = std::process::id();
            let host = ctx.host_name();
            let state = ctx.state().await.ok();
            let orphans: std::collections::BTreeSet<u32> =
                state.as_ref().map_or_else(Default::default, |s| {
                    doctor::findings(s, now)
                        .iter()
                        .filter(|f| f.kind == "orphan")
                        .filter_map(|f| s.worktree_procs.get(&f.subject))
                        .filter(|p| p.procs.host == host)
                        .flat_map(|p| p.procs.pids.iter().copied())
                        .collect()
                });
            // The scan is minutes old: on Linux the pid must still be in a deleted directory.
            let still_orphaned = |pid: u32| {
                orphans.contains(&pid)
                    && (!cfg!(target_os = "linux")
                        || std::fs::read_link(format!("/proc/{pid}/cwd"))
                            .is_ok_and(|c| c.to_string_lossy().ends_with(" (deleted)")))
            };
            let refused: Vec<(u32, String)> = pids
                .iter()
                .filter_map(|&pid| {
                    kill::judge(&t, me, pid, || still_orphaned(pid))
                        .err()
                        .map(|why| (pid, why))
                })
                .collect();
            if !refused.is_empty() {
                let actor = ctx.me().await;
                for (pid, why) in &refused {
                    eprintln!("agent-fleet kill: REFUSED {why}");
                    let body = Known::KillRefused(KillRefused {
                        host: host.clone(),
                        pid: *pid,
                        name: t.get(pid).map_or("?".into(), |p| p.name.clone()),
                        reason: why.clone(),
                    });
                    let logged = match &actor {
                        Ok(a) => ctx
                            .write(Envelope::new(a.clone(), &ctx.cli.project, body))
                            .await
                            .map(|_| ()),
                        Err(err) => Err(anyhow::anyhow!("{err:#}")),
                    };
                    if let Err(err) = logged {
                        eprintln!("agent-fleet kill: refusal not logged: {err:#}");
                    }
                }
                bail!(
                    "killed nothing: {} of {} refused",
                    refused.len(),
                    pids.len()
                );
            }
            let signal = |pid: u32, sig| unsafe { libc::kill(pid as i32, sig) };
            for &pid in pids {
                signal(pid, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(*grace);
            let mut left: Vec<u32> = pids.clone();
            while !left.is_empty() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                left.retain(|&p| scan::alive(p));
            }
            for &pid in &left {
                signal(pid, libc::SIGKILL);
            }
            for &pid in pids {
                let how = if left.contains(&pid) {
                    format!("KILL after {grace}s")
                } else {
                    "TERM".into()
                };
                println!(
                    "killed {pid} ({}): {how}",
                    t.get(&pid).map_or("?", |p| &p.name)
                );
            }
        }
        Cmd::Resource { cmd } => {
            let (name, host, resources, max, until) = match cmd {
                ResourceCmd::Limit {
                    name,
                    resources,
                    max,
                    until,
                    host,
                } => (name, host, resources.clone(), *max, *until),
                ResourceCmd::Unlimit { name, host } => (name, host, Vec::new(), 0, None),
            };
            let body = Known::ResourceLimit(ResourceLimit {
                host: host.clone().unwrap_or_else(|| ctx.host_name()),
                name: name.clone(),
                resources,
                max,
                until,
            });
            let e = Envelope::new(ctx.me().await?, &ctx.cli.project, body);
            ctx.write(e).await?;
        }
        Cmd::Unblock { blocker, note } => {
            let e = Envelope::new(
                ctx.me().await?,
                &ctx.cli.project,
                Known::BlockerResolved(BlockerResolved { note: note.clone() }),
            );
            ctx.write(e.re(blocker)).await?;
        }
        Cmd::Ask {
            task,
            question,
            options,
            recommendation,
            context,
        } => {
            let body = AskRaised {
                question: question.clone(),
                options: options.clone(),
                recommendation: recommendation.clone(),
                context: context.clone(),
            };
            ctx.task_event(task, Known::AskRaised(body)).await?;
        }
        Cmd::Role {
            agent,
            level,
            lead,
            domain,
            clear,
        } => {
            let id = ctx.role_agent(agent).await?;
            if let (None, false) = (level, clear) {
                print!("{}", ui::role(&ctx.state().await?, &id, now));
                return Ok(());
            }
            let me = ctx.me().await?;
            let data = if *clear {
                AgentRole {
                    agent: id,
                    level: None,
                    lead: None,
                    domain: None,
                }
            } else {
                let level = level.unwrap();
                let lead = if level == Level::RootCoordinator {
                    if lead.is_some() {
                        bail!("a root-coordinator takes no lead; drop --lead");
                    }
                    None
                } else {
                    match lead {
                        Some(l) => Some(ctx.role_agent(l).await?),
                        None if id != me.id => Some(me.id.clone()),
                        None => bail!("who does {id} report to? a {level} needs --lead <agent>"),
                    }
                };
                AgentRole {
                    agent: id,
                    level: Some(level),
                    lead,
                    domain: domain.clone(),
                }
            };
            ctx.write(Envelope::new(me, &ctx.cli.project, Known::AgentRole(data)))
                .await?;
        }
        Cmd::Spawn {
            target,
            scope,
            name,
            branch,
            profile,
            host,
            level,
            set,
            dry_run,
            no_wait,
        } => {
            let (repo, issue_n) = spawn::parse_target(target)?;
            let issue = spawn::gh_issue(&repo, issue_n).await?;
            let (cfg, cfg_dir) = spawn::find()?;
            // With no fleet state, caps and quotas are consulted as empty — say so.
            let (state, note) = ctx.snapshot_state().await.unwrap_or_else(|e| {
                (
                    State::default(),
                    Some(format!(
                        "no fleet state ({e:#}); caps and quotas not consulted"
                    )),
                )
            });
            if let Some(n) = note {
                eprintln!("agent-fleet: {n}");
            }
            let d = spawn::decide(
                &cfg,
                &issue,
                &spawn::Overrides {
                    profile: profile.as_deref(),
                    host: host.as_deref(),
                    level: *level,
                },
                &state,
                &ctx.host_name(),
            )?;
            let prof = &cfg.profiles[&d.profile];
            let name = name
                .clone()
                .unwrap_or_else(|| format!("{}{}", prof.prefix, issue.number));
            if name.is_empty() || name.contains(['/', ':', ' ']) {
                bail!("agent name {:?} is not usable in an agent id", name);
            }
            let branch = branch.clone().unwrap_or_else(|| name.clone());
            let agent = format!("agent:{}/{}/{name}", d.host, cfg.session);
            if state.agents.contains_key(&agent) {
                bail!("{agent} is already up");
            }
            if state.spawns.values().any(|s| {
                s.result.is_none()
                    && s.request.agent == agent
                    && Utc::now() - s.since <= spawn::REQUEST_MAX_AGE
            }) {
                bail!("a spawn for {agent} is already pending");
            }
            let me = ctx.me().await;
            let lead = match &me {
                Ok(a) => a.id.clone(),
                Err(_) if *dry_run => "?".into(),
                Err(e) => return Err(anyhow::anyhow!("{e:#}")),
            };
            let mut vals = cfg.vars.clone();
            for (k, v) in set {
                vals.insert(k.clone(), v.clone());
            }
            for (k, v) in [
                ("NAME", name.as_str()),
                ("KIND", d.profile.as_str()),
                ("PROGRAM", prof.kind.as_str()),
                ("SLUG", repo.as_str()),
                ("ISSUE", &issue.number.to_string()),
                ("TITLE", issue.title.as_str()),
                ("BRANCH", branch.as_str()),
                ("SCOPE", scope.as_deref().unwrap_or("")),
                ("HOST", d.host.as_str()),
                ("LEVEL", &d.level.to_string()),
                ("LEAD", lead.as_str()),
                ("HANDOFF", ""),
            ] {
                vals.insert(k.into(), v.to_string());
            }
            let src = std::fs::read_to_string(cfg_dir.join(&cfg.brief))
                .with_context(|| format!("read {}", cfg_dir.join(&cfg.brief).display()))?;
            let brief = spawn::render(spawn::brief_body(&src), &vals)?;
            if *dry_run {
                println!("spawn {repo}#{} \"{}\"", issue.number, issue.title);
                println!("rule      {}", d.why);
                println!("profile   {} (kind {})", d.profile, prof.kind);
                println!("host      {}", d.host);
                println!("session   {}", cfg.session);
                println!("name      {name}");
                println!("branch    {branch}");
                println!("level     {}", d.level);
                println!("lead      {lead}");
                println!("--- brief ---");
                print!("{brief}");
                return Ok(());
            }
            println!(
                "spawn {repo}#{} → {} ({}) on {} as {name} · {}",
                issue.number, d.profile, prof.kind, d.host, d.why
            );
            let me = me.map_err(|e| anyhow::anyhow!("{e:#}"))?;
            let task = spawn::task_id(&repo, issue.number);
            if !state.tasks.contains_key(&task) {
                let created = Envelope::new(
                    me.clone(),
                    spawn::task_project(&repo),
                    Known::TaskCreated(TaskCreated {
                        title: issue.title.clone(),
                        url: Some(format!("https://github.com/{repo}/issues/{}", issue.number)),
                    }),
                );
                ctx.client()?.write_exclusive(&created.task(&task)).await?;
            }
            let req = SpawnRequested {
                name,
                agent,
                host: d.host.clone(),
                session: cfg.session.clone(),
                repo: repo.clone(),
                issue: Some(issue.number),
                branch,
                profile: d.profile.clone(),
                brief,
                level: Some(d.level),
                lead: Some(lead),
                worktree: None,
                replaces: None,
                why: d.why.clone(),
            };
            let id = ctx
                .write(Envelope::new(
                    me,
                    &ctx.cli.project,
                    Known::SpawnRequested(Box::new(req)),
                ))
                .await?;
            if !no_wait {
                spawn::wait_result(&ctx.client()?, &id).await?;
            }
        }
        Cmd::Handoff { agent, to, note } => {
            let state = ctx.state().await?;
            let Some(sp) = spawn::find_spawn(&state, agent).cloned() else {
                bail!("{agent} has no spawn record; it was not spawned by the fleet");
            };
            if spawn::replacement_open(&state, &sp.request.agent) {
                bail!("{} was already handed off", sp.request.name);
            }
            let (cfg, cfg_dir) = spawn::find()?;
            let to = match to {
                Some(t) => t.clone(),
                None => cfg
                    .profiles
                    .get(&sp.request.profile)
                    .and_then(|p| p.fallback.clone())
                    .with_context(|| {
                        format!(
                            "profile {:?} has no fallback; pass --to",
                            sp.request.profile
                        )
                    })?,
            };
            let title = match sp.request.issue {
                Some(i) => spawn::gh_issue(&sp.request.repo, i)
                    .await
                    .map(|x| x.title)
                    .unwrap_or_else(|_| format!("#{i}")),
                None => String::new(),
            };
            let req = spawn::handoff_request(
                &cfg,
                &cfg_dir,
                &sp,
                &to,
                note.as_deref().unwrap_or(""),
                &title,
            )?;
            if state.agents.contains_key(&req.agent) {
                bail!("{} is already up", req.agent);
            }
            let id = ctx
                .write(Envelope::new(
                    ctx.me().await?,
                    &sp.project,
                    Known::SpawnRequested(Box::new(req)),
                ))
                .await?;
            spawn::wait_result(&ctx.client()?, &id).await?;
        }
        Cmd::Tell {
            agent,
            kind,
            reference,
            text,
        } => {
            let to = ctx.resolve_agent(agent).await?;
            let body = MsgSent {
                to,
                kind: kind.clone(),
                reference: reference.clone(),
                text: text.join(" "),
            };
            ctx.write(Envelope::new(
                ctx.me().await?,
                &ctx.cli.project,
                Known::MsgSent(body),
            ))
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_arg_with_an_equals_inside_a_path_stays_a_path() {
        assert_eq!(
            parse_repo("o/r=/co/dir").unwrap(),
            (Some("o/r".into()), PathBuf::from("/co/dir"))
        );
        assert_eq!(
            parse_repo("/plain/dir").unwrap(),
            (None, PathBuf::from("/plain/dir"))
        );
        // `foo=2` is a path containing '=', not a slug: `foo` has no slash.
        assert_eq!(parse_repo("foo=2").unwrap(), (None, PathBuf::from("foo=2")));
        // `/p=2`: the left side starts with '/', so it is a path.
        assert_eq!(parse_repo("/p=2").unwrap(), (None, PathBuf::from("/p=2")));
    }
}
