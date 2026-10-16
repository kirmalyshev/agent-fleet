mod bench;
mod client;
mod doctor;
mod event;
mod host;
mod hub;
mod scan;
mod state;
mod stats;
mod store;
mod tui;
mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::Utc;
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
    /// Run the host agent for one herdr session: presence, disk, replica, doorbell delivery.
    Host {
        #[arg(long, env = "HERDR_SOCKET_PATH")]
        herdr_socket: PathBuf,
        #[arg(long, env = "HERDR_SESSION")]
        session: String,
        /// A repository whose worktrees to watch even when no agent works in it. Repeatable.
        #[arg(long = "repo")]
        repos: Vec<String>,
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
    /// The board: agents, tasks, blockers, asks, undelivered doorbells, hosts.
    Ui {
        #[arg(long, required = true)]
        text: bool,
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
enum DepCmd {
    Add { task: String, on: String },
    Rm { task: String, on: String },
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

struct Ctx {
    cli: Cli,
    paths: Paths,
}

impl Ctx {
    fn host_name(&self) -> String {
        self.cli.host_name.clone().unwrap_or_else(|| {
            rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned()
        })
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
        } => {
            let cfg = host::Config {
                host: ctx.host_name(),
                session: session.clone(),
                herdr_socket: herdr_socket.clone(),
                replica: ctx.paths.replica(),
                data_dir: ctx.paths.dir.clone(),
                repos: repos.clone(),
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
        Cmd::Ui { .. } => print!("{}", ui::board(&ctx.state().await?, now)),
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
