# fleet

One event log for a fleet of coding agents. Every fact (an agent came up, a task was claimed, a
blocker was raised, a message was delivered) is a typed event. State is a fold over the events,
and every view is a function of the state.

One Rust binary, no runtime. Linux and macOS. Agents run in [herdr](https://herdr.dev) panes.

```
fleet hub                 the log, the fold, the sequencer (HTTP + SSE on 127.0.0.1)
fleet host                one per machine and herdr session: presence from herdr, disk, a replica, doorbells
fleet ps                  what every agent is doing
fleet task add|done|rm|ls|show
                          claim (exclusive: 409 if taken), finish, cancel or release, list, inspect
fleet dep add|rm          task A is blocked by task B
fleet block / unblock     raise and resolve blockers
fleet ask                 a question for an operator
fleet tell <agent> <TYPE> <ref> <text>
                          ring an agent's doorbell; delivered = it was seen working afterwards
fleet state --json        the folded state
fleet ui --text           the board: agents, tasks, blockers, asks, undelivered doorbells, hosts
fleet stats               doorbell latency and loss, event lag, host and hub vitals
fleet bench               load-test a throwaway hub: latency, throughput, follower lag, 409 races
fleet api schema          JSON Schema of events and state (schema/fleet.schema.json)
```

## How it works

```
   agents ── fleet CLI ──┐                       ┌── fleet host (machine A) ── herdr ── agent panes
                         ▼                       │
                 fleet hub: event log ── SSE ────┼── fleet host (machine B) ── herdr ── agent panes
                 (SQLite) + state                │
                                                 └── …
```

1. **Everything is an event.** An agent runs `fleet task add`, `fleet block` or `fleet tell`. Each
   command sends one small JSON event to the hub, for example `blocker.raised`.
2. **The hub keeps the log.** It checks each event, appends it to a SQLite log, and folds the log
   into one state: who is up, who owns which task, what is blocked, which messages are not yet
   delivered. `fleet ps` and `fleet ui --text` print that state.
3. **One host agent per machine (and herdr session) watches herdr.** herdr pushes a status change when an agent turns
   idle, working or blocked. `fleet host` turns each change into an event, so the hub always knows
   what every agent is doing, with no polling.
4. **Messages are doorbells.** `fleet tell` writes a `msg.sent` event. The host agent on the
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
fleet hub &                                  # writes a token file on first start
fleet host --herdr-socket <session socket> --session <name> &
export FLEET_AGENT=agent:host1/main/me       # inside a herdr pane this is derived for you
fleet task add build-cache --title "Cache the build"
fleet tell builder REVIEW task:default/build-cache "ready for a look"
fleet ui --text
```

`herdr session list` shows each session's socket. Settings come from flags or the environment:
`FLEET_HUB`, `FLEET_HOME`, `FLEET_TOKEN_FILE`, `FLEET_AGENT`, `FLEET_PROJECT`, `FLEET_HOST`
(this machine's name in ids, default the hostname).

When the hub is down, ordinary writes go to a local spool and replay later. `task add` refuses
instead, because only the hub can decide who owns a task. Reads fall back to the host's replica.

## The protocol

[docs/PROTOCOL.md](docs/PROTOCOL.md): the envelope, the 16 v1 types, the rules the hub enforces
(400 / 403 / 409, idempotent append), the HTTP API, doorbells, presence, spool and replica.

## v2

Not built yet: decisions and a human-only channel for answers, data-defined extension types, a
web board, code-host ingest, a NATS transport, `fleet run`/`kill` process ownership, Windows.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -q -- api schema > schema/fleet.schema.json   # after changing a type
```

MIT licensed.
