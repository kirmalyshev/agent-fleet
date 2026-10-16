# agent-fleet

One event log for a fleet of coding agents. Every fact (an agent came up, a task was claimed, a
blocker was raised, a message was delivered) is a typed event. State is a fold over the events,
and every view is a function of the state.

One Rust binary, no runtime. Linux and macOS. Agents run in [herdr](https://herdr.dev) panes.

```
agent-fleet hub                 the log, the fold, the sequencer (HTTP + SSE on 127.0.0.1)
agent-fleet host                one per machine and herdr session: presence from herdr, disk, a replica, doorbells
agent-fleet ps                  what every agent is doing, and who holds or waits for each resource
agent-fleet task add|done|rm|ls|show
                          claim (exclusive: 409 if taken), finish, cancel or release, list, inspect
agent-fleet dep add|rm          task A is blocked by task B
agent-fleet block / unblock     raise and resolve blockers
agent-fleet ask                 a question for an operator
agent-fleet resource limit|unlimit
                          a capacity rule over the locks hosts report (host --lock name=dir)
agent-fleet tell <agent> <TYPE> <ref> <text>
                          ring an agent's doorbell; delivered = it was seen working afterwards
agent-fleet kill <pid>...       stop your own processes or proved orphans; refuses agents and their ancestors
agent-fleet hook bash           Claude Code PreToolUse hook: blocks raw kill/pkill/killall
agent-fleet state --json        the folded state
agent-fleet tui                 the fleet on one screen, live: agents, worktrees, tasks, findings,
                          with tell, focus, task done/release/cancel, remove a worktree, kill orphans
agent-fleet ui                  the board in the browser, live: a tree of repositories, worktrees,
                          agents and tasks with every status, plus agents, findings, doorbells, hosts
agent-fleet ui --text           the board: agents, tasks, blockers, asks, undelivered doorbells, hosts
agent-fleet stats               doorbell latency and loss, event lag, host and hub vitals
agent-fleet doctor              stale, hung or heavy agents, duplicate work, forgotten worktrees,
                          unpushed commits, orphaned processes, capacity breaches, long queues, refused kills
agent-fleet instances           one fleet per project or product: each has its own hub, log and token
agent-fleet bench               load-test a throwaway hub: latency, throughput, follower lag, 409 races
agent-fleet api schema          JSON Schema of events and state (schema/agent-fleet.schema.json)
```

## How it works

```
   agents ── fleet CLI ──┐                       ┌── agent-fleet host (machine A) ── herdr ── agent panes
                         ▼                       │
                 agent-fleet hub: event log ── SSE ────┼── agent-fleet host (machine B) ── herdr ── agent panes
                 (SQLite) + state                │
                                                 └── …
```

1. **Everything is an event.** An agent runs `agent-fleet task add`, `agent-fleet block` or `agent-fleet tell`. Each
   command sends one small JSON event to the hub, for example `blocker.raised`.
2. **The hub keeps the log.** It checks each event, appends it to a SQLite log, and folds the log
   into one state: who is up, who owns which task, what is blocked, which messages are not yet
   delivered. `agent-fleet ps` and `agent-fleet ui --text` print that state.
3. **One host agent per machine (and herdr session) watches herdr.** herdr pushes a status change when an agent turns
   idle, working or blocked. `agent-fleet host` turns each change into an event, so the hub always knows
   what every agent is doing, with no polling.
4. **Messages are doorbells.** `agent-fleet tell` writes a `msg.sent` event. The host agent on the
   recipient's machine types it into that agent's prompt. When it sees the agent start working,
   it writes `msg.delivered`. A message with no `delivered` is visibly lost, and the host retries
   it.
5. **Only one thing needs the hub's permission.** Claiming a task is exclusive: if someone already
   holds it, the hub answers 409. Everything else can wait. When the hub is down, the CLI keeps
   writes in a local spool and sends them later; replaying is safe because every event has a
   unique id.
6. **Every machine has a copy.** The host agent copies the whole log locally. When the hub is
   down, reads still work from that copy.

## Quick start

```sh
cargo build --release
agent-fleet hub &                                  # writes a token file on first start
agent-fleet host --herdr-socket <session socket> --session <name> &
export AGENT_FLEET_AGENT=agent:host1/main/me       # inside a herdr pane this is derived for you
agent-fleet task add build-cache --title "Cache the build"
agent-fleet tell builder REVIEW task:default/build-cache "ready for a look"
agent-fleet ui --text
```

`herdr session list` shows each session's socket. Settings come from flags or the environment:
`AGENT_FLEET_INSTANCE` (which fleet, default `default`), `AGENT_FLEET_HUB`, `AGENT_FLEET_HOME`,
`AGENT_FLEET_TOKEN_FILE`, `AGENT_FLEET_AGENT`, `AGENT_FLEET_PROJECT`, `AGENT_FLEET_HOST` (this machine's name in ids,
default the hostname).

When the hub is down, ordinary writes go to a local spool and replay later. `task add` refuses
instead, because only the hub can decide who owns a task. Reads fall back to the host's replica.

## The protocol

[docs/PROTOCOL.md](docs/PROTOCOL.md): the envelope, the v1 types, the rules the hub enforces
(400 / 403 / 409, idempotent append), the HTTP API, doorbells, presence, spool and replica.

## v2

Not built yet: decisions and a human-only channel for answers, data-defined extension types, a
web board, code-host ingest, a NATS transport, `agent-fleet run`/`kill` process ownership, Windows.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -q -- api schema > schema/agent-fleet.schema.json   # after changing a type
```

MIT licensed.
