# The fleet protocol, v1

fleet coordinates many coding agents running in terminal panes across a few machines. It answers
two questions: *what is everyone doing?* and *did my message actually arrive?*

Three rules carry the design:

1. **The protocol is an event schema.** Every fact is a typed event in an append-only log.
   State is a fold over the events (`state = fold(events)`), and every view is a function of the
   state. A new feature is a new event type plus one reducer case.
2. **Messages are doorbells; events are the truth.** A prompt typed into an agent's terminal can
   be lost. So the message is an event first (`msg.sent`), and delivery is a second event
   (`msg.delivered`). A lost doorbell is visible as sent and not delivered, and it is retried.
3. **Humans are authenticated, agents do not speak for them.** An event whose actor is a human
   may only enter through a human-only channel. v1 has no such channel, so the hub refuses
   `actor.kind = human` outright.

## Topology

```
        fleet hub  (one process: SQLite log, reducer, HTTP + SSE, the only sequencer)
           ▲  events / SSE
           │
   fleet host (one per machine) ── herdr socket ──▶ agent panes
           ▲
   fleet CLI (agents) ── local spool when the hub is unreachable
```

- **Hub** (`fleet hub`): appends events, folds them, serves `/state` and an SSE feed. It is the
  single sequencer for the one exclusive operation in v1, `task.assigned`. It binds a specific
  address (default `127.0.0.1:7710`) and refuses `0.0.0.0` / `::`.
- **Host agent** (`fleet host`): one per machine and herdr session. It publishes presence from
  herdr's push events, reports disk health, keeps a full local replica of the log, and delivers
  doorbells to its agents.
- **CLI** (`fleet …`): what agents use. Ordinary writes go to a local spool when the hub is down
  and replay later. Exclusive writes refuse instead.

The hub is never on a hot path: local locks, builds and merges do not depend on it. Hub down
means degraded visibility, not stopped work.

## Envelope

```jsonc
{
  "id": "01K…",                       // ULID chosen by the writer; replaying it is a no-op
  "v": 1,                              // schema version of this type
  "ts": "2026-01-01T12:00:00Z",
  "type": "blocker.raised",
  "actor": { "kind": "agent", "id": "agent:host1/main/builder" },
  "project": "demo",
  "task": "task:demo/42",              // optional
  "re": "01K…",                        // optional: the event this one answers or closes
  "data": { … }                        // type-specific
}
```

Ids are stable strings: `agent:<host>/<session>/<name>`, `host:<name>`, `human:<handle>`,
`task:<project>/<name>`. `<host>` is `FLEET_HOST` (default: the system hostname); `fleet host`
and the CLI on one machine read the same setting, so an agent's writes and its presence carry
the same id. `fleet api schema` prints the full JSON Schema; it is committed as
[`schema/fleet.schema.json`](../schema/fleet.schema.json) and a test fails when it goes stale.

## Types (v1)

| Family | Type | `data` | Envelope needs | Written by |
|---|---|---|---|---|
| presence | `agent.up` | agent, host, kind?, state, pane? | | host |
| | `agent.state` | agent, state (`idle` `working` `blocked` `done` `unknown`) | | host |
| | `agent.down` | agent, reason? | | host |
| | `host.health` | host, disk_free, disk_total, agents | | host |
| task | `task.created` | title, url? | task | agent, host |
| | `task.assigned` | agent, lead? — **exclusive** | task | agent, host |
| | `task.unassigned` | reason? | task | agent, host |
| | `task.state` | state (`submitted` `working` `input-required` `completed` `failed` `canceled`) | task | agent, host |
| | `task.dep.added` / `task.dep.removed` | on (task is blocked by `on`) | task | agent, host |
| blocker | `blocker.raised` | what, owner?, unblocks_when? | task | agent, host |
| | `blocker.resolved` | note? | re = the raise | agent, host |
| question | `ask.raised` | question, options[], recommendation?, context? | task | agent |
| comms | `msg.sent` | to, kind, ref, text | | agent, host |
| | `msg.delivered` | to, observed | re = the send | host |
| | `handoff` | to, next, done?, evidence[], learned? | task | agent |

Task states follow the A2A task lifecycle names, so tools that know A2A read them as-is.

## Rules the hub enforces

- **Strict on write.** `POST /events` accepts only known types at `v: 1`, with no unknown fields
  in the envelope or in `data`, with the envelope fields the type needs, and from an allowed
  actor kind. A malformed event is `400`; a forbidden actor (any `human`, any `hub`, or a kind
  not in the table) is `403`.
- **Exclusive assignment.** `task.assigned` for a task that already has an owner and is not
  finished (`completed`, `failed`, `canceled`) is `409`. The check and the append happen under
  one lock. Release with `task.unassigned` or finish the task to free it. Claiming a finished
  task reopens it (`working`), so it is exclusive again.
- **Idempotent append.** An event whose `id` is already in the log is acknowledged as
  `{"duplicate": true}` and not appended again, before any other check. Replay is always safe.
- **Bearer token.** Every endpoint needs `Authorization: Bearer <token>`. The hub creates a
  random token file (mode 0600) on first start; clients read the same file.

## Tolerant on read

A reader keeps an event of a type it does not know (or a newer version of a known one) verbatim,
skips it in the fold, and counts it in `state.unknown`. It never crashes on it and never drops it
from the log, so the hub and the CLIs upgrade independently.

## HTTP API

| Method | Path | |
|---|---|---|
| `POST` | `/events` | append one event; `200 {"id", "duplicate"}` or `400/401/403/409` with `{"error"}` |
| `GET` | `/events?after=<id>&limit=<n>` | the log in arrival order after the given event |
| `GET` | `/events/stream?after=<id>` | SSE: the backlog after the cursor, then live events |
| `GET` | `/state` | the folded state |
| `GET` | `/schema` | the same document as `fleet api schema` |

The cursor is an event id, but order is **arrival** order at the hub. A spooled event with an old
ULID that arrives late is still returned after the cursor, so a follower never misses it.

## State

`GET /state` returns `hosts`, `agents` (state + since), `tasks` (state + since, owner, lead,
blocked_by, handoffs), open `blockers` and `asks` keyed by their raising event id, undelivered
`msgs` keyed by their `msg.sent` id, the id of the `last` event folded, and the `unknown` count.

## Doorbells

`fleet tell <agent> <TYPE> <ref> <text>` writes `msg.sent`. The host agent of the recipient's
machine types it into the agent as

```
[fleet <from>→<to>] <TYPE> <ref>: <text> (event <id>)
```

The `[fleet` prefix marks the turn as machine-sent, so it is never mistaken for a human
instruction. The host agent rings only an `idle` or `done` agent, never a busy or blocked one. It
writes `msg.delivered` only after it has itself observed and logged the agent turning `working`
after the prompt. A prompt that was merely submitted proves nothing. A failed ring is retried at
most once a minute.

## Presence (herdr)

The host agent talks to herdr over its socket API (newline-delimited JSON, herdr protocol 22).
It checks the protocol at connect and disables presence on a mismatch rather than misread events.

- `agent.list` at start gives `agent.up` for every agent. Agents that the log believes are up in
  this session but herdr no longer has are marked `agent.down`.
- `events.subscribe` to `pane.agent_detected`, `pane.closed`, `pane.exited`,
  `workspace.created` and `workspace.closed` drives `agent.up` and `agent.down`.
- One `pane.agent_status_changed` subscription per agent pane drives `agent.state`, on change
  only. Presence is push, not polling.

## Spool and replica

- **Spool.** When the hub is unreachable, or answers with anything that is not a verdict on the
  event (401, 404, 429, 5xx…), an ordinary write is appended to `spool.jsonl` in the data
  directory. The next write from any `fleet` process on that machine, and the host agent's sync
  loop, replay it in order before anything new. One process replays at a time; the spool is
  rewritten by write-then-rename, and replay is idempotent, so a crash mid-replay is harmless.
  Only a verdict (400, 403, 409) or an unreadable line moves an event to `spool.rejected.jsonl`.
- **Exclusive writes never spool.** `fleet task add` with the hub unreachable fails: two
  partitioned copies cannot both grant the same task.
- **Replica.** The host agent pulls `GET /events?after=<cursor>` and then follows the SSE feed
  into its own SQLite copy. Its own writes go into the replica at once, unless the hub refused
  them. Copies merge by event id,
  with no conflicts. Read commands (`ps`, `task ls/show`, `state`, `ui`) ask the hub and fall
  back to the replica when it is unreachable.

## Files

Everything lives in one data directory: `FLEET_HOME`, or the platform data directory
(`directories` crate). `hub.db`, `replica.db`, `spool.jsonl`, `token`.

## Not in v1

Decisions and the human channel (`ask.answered`, `decision.*`), data-defined extension types,
the web board, code-host ingest, a NATS transport, `fleet run`/`kill` process ownership, Windows.
These are v2.
