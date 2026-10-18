# The fleet protocol, v1

agent-fleet coordinates many coding agents running in terminal panes across a few machines. It answers
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
        agent-fleet hub  (one process: SQLite log, reducer, HTTP + SSE, the only sequencer)
           ▲  events / SSE
           │
   agent-fleet host (one per machine and herdr session) ── herdr socket ──▶ agent panes
           ▲
   fleet CLI (agents) ── local spool when the hub is unreachable
```

- **Hub** (`agent-fleet hub`): appends events, folds them, serves `/state` and an SSE feed. It is the
  single sequencer for the one exclusive operation in v1, `task.assigned`. It binds a specific
  address (default `127.0.0.1:7710`) and refuses `0.0.0.0` / `::`.
- **Host agent** (`agent-fleet host`): one per machine and herdr session. It publishes presence from
  herdr's push events, reports disk health, keeps a full local replica of the log, and delivers
  doorbells to its agents.
- **CLI** (`agent-fleet …`): what agents use. Ordinary writes go to a local spool when the hub is down
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
`task:<project>/<name>`; an issue's task is `task:<repo name>/<issue>`, so a spawn's assignment and the
agent's own `AGENT_FLEET_PROJECT=<repo name> task add <issue>` name the same task. `<host>` is `AGENT_FLEET_HOST` (default: the system hostname); `agent-fleet host`
and the CLI on one machine read the same setting, so an agent's writes and its presence carry
the same id. `actor.id` must start with its kind (`agent:`, `host:`).

`agent-fleet api schema` prints the JSON Schema of every event and of the state; it is committed as
[`schema/agent-fleet.schema.json`](../schema/agent-fleet.schema.json) and a test fails when it goes stale.
The schema describes the shape. The rules below are stricter: required envelope fields per type,
allowed writers, ULID ids, `v: 1`, no unknown envelope keys, and a size limit.

## Types (v1)

| Family | Type | `data` | Envelope needs | Written by |
|---|---|---|---|---|
| presence | `agent.up` | agent, host, kind?, state, pane? | | host |
| | `agent.state` | agent, state (`idle` `working` `blocked` `done` `unknown`) | | host |
| | `agent.down` | agent, reason? | | host |
| | `host.health` | host, disk_free, disk_total, agents, stats? (every minute) | | host |
| | `agent.usage` | agent, rss_kb?, cpu_pct?, procs?, cwd? (each scan) | | host |
| | `agent.role` | agent, level? (`root-coordinator` `domain-lead` `implementation-lead` `implementer`; absent `level` removes the role), lead?, domain? | | agent, host |
| machine | `worktree.state` | host, path, repo, branch?, head, unpushed, dirty, files[] (only what still differs from the default branch), last_commit, in_main? (on change) | | host |
| | `worktree.gone` | host, path | | host |
| | `worktree.procs` | host, path, procs, rss_kb, cpu_pct, names[], pids[], orphaned (each scan; `procs: 0` once when they end; Linux only — `ps` reports no working directory on macOS) | | host |
| | `agent.quota` | host, kind, windows[] ({name, used_pct, resets_at?}), plan?, exhausted, read_at (each minute, when anything but `read_at` changes) | | host |
| resource | `resource.taken` | host, resource, pid, cwd?, scope?, since | | host |
| | `resource.waiting` | host, resource, pid, cwd?, scope?, since | | host |
| | `resource.released` | host, resource, pid? (without `pid`: nobody holds or waits) | | host |
| | `resource.limit` | host, name, resources[], max, until? (empty `resources` removes it) | | agent, host |
| safety | `kill.refused` | host, pid, name, reason | | agent, host |
| task | `task.created` | title, url? | task | agent, host |
| | `task.assigned` | agent, lead? — **exclusive** | task | agent, host |
| | `task.unassigned` | reason? | task | agent, host |
| | `task.state` | state (`submitted` `working` `input-required` `completed` `failed` `canceled`) | task | agent, host |
| | `task.dep.added` / `task.dep.removed` | on (task is blocked by `on`) | task | agent, host |
| blocker | `blocker.raised` | what, owner?, unblocks_when? | task | agent, host |
| | `blocker.resolved` | note? | re = the raise | agent, host |
| question | `ask.raised` | question, options[], recommendation?, context? | task | agent |
| comms | `msg.sent` | to, kind, ref, text | | agent, host |
| | `msg.delivered` | to, observed, rung?, attempts? | re = the send | host |
| | `handoff` | to, next, done?, evidence[], learned? | task | agent |
| spawn | `spawn.requested` | name, agent, host, session, repo, issue?, branch, profile, brief, level?, lead?, worktree?, replaces?, why | | agent, host |
| | `spawn.result` | agent, ok, pane?, worktree?, error? | re = the request | host |
| | `agent.unmanaged` | host, pid, program?, cwd?, since? — no `program` clears the report | | host |
| | `agent.stuck` | agent, cause?, detail, until? — no `cause` clears the record | | host |

Task states follow the A2A task lifecycle names, so tools that know A2A read them as-is.

`agent.role` declares an agent's level of responsibility — `root-coordinator`, then `domain-lead`,
`implementation-lead`, `implementer` — and the lead it reports to, with `domain` naming what a
domain lead owns. Whoever spawns the agent, or the agent itself, writes it
(`agent-fleet role <agent> <level> --lead <lead> --domain <domain>`); it is never guessed from a
name or a pane. A role can be declared before the agent's first `agent.up`, survives the agent
going down and coming back, and only an `agent.role` without `level` removes it.

`agent.quota` is the usage quota of one agent program on one host — each machine is logged in to
its own account, so `kind` (`claude`, `codex`, `devin`, or anything new) is scoped per host.
The host samples three sources every minute: the newest session log under
`~/.codex/sessions` for codex, the machine-wide `claude-rate-limits.json` in the fleet data
directory root for claude (written by `agent-fleet hook statusline`, the status-line wrapper
that stores the `rate_limits` it is fed and passes the input on to the command it wraps), and
the newest cached `devin auth status` under `~/.cache/devin/cli` for devin. `read_at` stamps the
data itself, not the scan, so the views can say how stale the numbers are.

## Rules the hub enforces

- **Strict on write.** `POST /events` accepts only known types at `v: 1`, with no unknown fields
  in the envelope or in `data`, with the envelope fields the type needs, at most 64 KiB, and from
  an allowed actor kind, and with every recipient (`msg.sent.to`, `handoff.to`,
  `task.assigned.agent`, `agent.role.agent`, `agent.role.lead`) a full agent id. A malformed or oversized event is `400`; a forbidden actor (any `human`,
  any `hub`, or a kind not in the table) is `403`. Clients apply the same rules before sending,
  so an invalid event is refused locally and never spooled.
- **Exclusive assignment.** `task.assigned` for a task that already has an owner and is not
  finished (`completed`, `failed`, `canceled`) is `409`. The check and the append happen under
  one lock. Release with `task.unassigned` or finish the task to free it. Claiming a finished
  task reopens it (`working`), so it is exclusive again. Reopening it through `task.state`
  clears the old owner.
- **Idempotent append.** An event whose `id` is already in the log is acknowledged as
  `{"duplicate": true}` and not appended again, before any other check, so replaying an
  accepted event never fails, even after the rules tighten.
- **A readable log.** The hub refuses to start on a log holding events it cannot fold: that
  means the binary and the log drifted apart, and a skipped `task.assigned` would let a task be
  granted twice.
- **Bearer token.** Every endpoint needs `Authorization: Bearer <token>`. The hub creates a
  random token file (mode 0600) on first start; clients read the same file.

## Tolerant on read

A reader keeps an event of a type it does not know, or of a known type at a newer `v`, verbatim,
skips it in the fold, and counts it in `state.unknown`. It never crashes on it and never drops it
from the log, so host agents and CLIs upgrade independently of the hub.

## HTTP API

| Method | Path | |
|---|---|---|
| `POST` | `/events` | append one event; `200 {"id", "duplicate"}`, or `400/401/403/409/500` with `{"error"}` |
| `GET` | `/events?after=<id>&limit=<n>` | the log in arrival order after the given event, at most `limit` (default 1000); `410` if the cursor is not in the log |
| `GET` | `/events/stream?after=<id>` | SSE: the backlog after the cursor, then live events; `410` for an unknown cursor |
| `GET` | `/state` | the folded state |
| `GET` | `/schema` | the same document as `agent-fleet api schema` |
| `GET` | `/board` | the state, `doctor`'s findings and the worktree each agent is in: what the board page reads |
| `GET` | `/` | the board page itself, without a token: it holds no data and reads `/board` with the token it is given |
| `GET` | `/stats?since=<s>` | the hub's vitals and numbers read off the last `s` seconds of the log (see Metrics) |

The cursor is an event id, but order is **arrival** order at the hub. A spooled event with an old
ULID that arrives late is still returned after the cursor, so a follower never misses it.

## State

`GET /state` returns `hosts`, `agents` (state + since), `roles` (level, lead, domain, by, since)
keyed by agent id, `spawns` (the request, who asked, the result) keyed by `spawn.requested` id
— kept for 24 h after its answer (or its request, when unanswered), and never while the agent
is still up — `quotas` (windows, plan, exhausted, read_at) keyed by `<host>:<kind>`,
`tasks` (state + since, owner, lead,
blocked_by, handoffs), open `blockers` and `asks` (with their context link) keyed by their raising event id, undelivered
`msgs` keyed by their `msg.sent` id, the id of the `last` event folded, and the `unknown` count.

## Doorbells

`agent-fleet tell <agent> <TYPE> <ref> <text>` writes `msg.sent`. The host agent of the recipient's
machine types it into the agent as

```
[agent-fleet <from>→<to>] <TYPE> <ref>: <text> (event <id>)
```

The `[fleet` prefix marks the turn as machine-sent, so it is never mistaken for a human
instruction. The host agent rings only an `idle` or `done` agent, never a busy or blocked one. It
writes `msg.delivered` only after it has itself observed and logged the agent turning `working`
after the prompt. A prompt that was merely submitted proves nothing. A failed ring is retried at
most once a minute; if `msg.delivered` itself cannot be written, the message stays pending.

## Presence (herdr)

The host agent talks to herdr over its socket API (newline-delimited JSON, herdr protocol 22).
It checks the protocol at connect. On a mismatch, or when herdr is down or restarts, presence
retries with backoff instead of misreading events; replica sync and delivery keep running.

- `agent.list` gives `agent.up` for every agent. Agents the hub believes are up in this session
  but herdr no longer has are marked `agent.down`.
- `events.subscribe` to `pane.agent_detected` drives `agent.up`; `pane.closed`, `pane.exited`
  and `workspace.closed` drive `agent.down`.
- One `pane.agent_status_changed` subscription per agent drives `agent.state`, on change only.
  Right after subscribing it reads the agent's current state, so a change made before the
  subscription existed is not missed. A dropped subscription is re-armed with backoff.
  Presence is push, not polling.

If any of the host agent's loops stops (a panic, a poisoned lock), the process exits, so a
supervisor restarts it rather than leaving a host that looks healthy and no longer delivers.

## Spool and replica

- **Spool.** When the hub is unreachable, or answers with anything that is not a verdict on the
  event (401, 404, 429, 5xx…), an ordinary write is appended to `spool.jsonl` in the data
  directory. The next write from any `agent-fleet` process on that machine, and the host agent's sync
  loop, replay it in order before anything new. One process replays at a time; the spool is
  rewritten by write-then-rename, and replay is idempotent, so a crash mid-replay is harmless.
  Only a verdict (400, 403, 409, 413, 415, 422 with the hub's error body) or an unreadable line
  moves an event to `spool.rejected.jsonl`. While events wait, every later write says why.
- **Exclusive writes never spool.** `agent-fleet task add` with the hub unreachable fails: two
  partitioned copies cannot both grant the same task. It also waits for the spool to drain, so
  it never overtakes a spooled release.
- **Replica.** The host agent pulls `GET /events?after=<cursor>` and then follows the SSE feed
  into its own SQLite copy. Its own writes go into the replica at once, unless they could not
  be written. Copies merge by event id, with no conflicts. If the hub answers `410` (its log was
  replaced), the host re-reads the whole log. Read commands (`ps`, `task ls/show`, `state`,
  `ui`) ask the hub and fall back to the replica only when the hub is unreachable, saying how
  old the replica's last event is.

## Metrics

The log is the record, so most numbers are read off it; the hub stamps each event's arrival
time, and the host agent reports its own vitals in `host.health.stats`. `agent-fleet stats` shows
them, `GET /stats` returns them as JSON.

- **Doorbells.** Sent, delivered, pending, and lost (undelivered for over 10 minutes). Time to
  delivery splits into `wait` (sent until the ring that worked: the agent was busy) and
  `react` (that ring until the agent turned `working`), from `msg.delivered.rung`. A delivery
  that took more than one ring (`attempts`) counts as retried.
- **Presence.** Agents seen, ups and downs, state changes, flaps (a state that lasted under
  5 s), and changes into `unknown`.
- **Lag.** Hub arrival minus the writer's `ts`; spooled events are the long tail.
- **Hosts.** Reports received against those expected, the longest silence, restarts (a drop
  in `uptime_s`), and the last vitals: memory, CPU, rings and failed rings, herdr errors,
  events spooled or not written at all.
- **Hub.** Uptime, memory, CPU, log size, followers, the time each recent `POST /events`
  spent in the hub (lock wait included), and posts by outcome (accepted, duplicate, 400, 409…).

`agent-fleet bench` runs a throwaway hub in-process and measures it with the same client the CLI
uses: post latency alone and under contention, throughput, how far a follower lags a writer,
and whether racing claims stay exclusive. It measures the hub and the client, not herdr or
the agents.

## Watching the machine

Every 5 minutes the host agent scans its machine. It writes facts, never verdicts:

- **Agents' cost.** For each agent, the pane's whole process tree: memory, CPU since the last
  scan, process count and working directory (`agent.usage`), from `/proc` on Linux and one `ps`
  call on macOS.
- **Worktrees.** Every git worktree of every repository an agent works in, plus `--repo`
  extras: branch, HEAD, commits no remote has, dirty files, files changed against the default
  branch, last commit (`worktree.state`, sent when it changes; `worktree.gone` when removed).
- **What repositories run.** A repository runs its own processes: dev servers, bundlers,
  databases, test suites. Processes whose working directory is in a worktree, outside every
  agent's tree, are grouped per worktree (`worktree.procs`). A process still running in a
  *deleted* directory beside the worktrees is an orphan: nothing that could stop it knows it is
  there. Git calls run with `GIT_OPTIONAL_LOCKS=0`, so a scan never takes an agent's index lock.

- **Resources.** Every 5 seconds, each lock given as `--lock <name>=<dir>`. A lock is a
  directory, held while it exists. The holder's pid is in its `owner` file, else in `pid`; it
  counts while either process is alive. Optional `cwd` and `scope` files say where and what.
  Waiters are files in `<dir>.waiting/`, named `<unix seconds>.<pid>`, with pid, scope and cwd on
  their first three lines. Changes go out as `resource.taken`, `resource.waiting` and
  `resource.released`; a host starting to watch a lock first releases it in full, so nothing it
  missed while down survives. The host only reads: the scripts keep doing the locking.

A capacity rule (`agent-fleet resource limit <name> <resources>… --max N --until HH:MM`) says
how many worktrees may hold any of a set of resources at once.

`agent-fleet doctor` (and the board) reads verdicts off that state:

| Finding | When |
|---|---|
| `stale` | an agent idle, done or blocked for 2 h |
| `hung` | working for 30 min at under 2% CPU, its worktree unchanged as long |
| `heavy` | an agent's tree, or a worktree's processes, over 4 GB or 200% CPU |
| `duplicate` | two agents in one worktree, or two agents' worktrees changing the same files (lockfiles aside) |
| `forgotten` | a worktree no agent works in, with unsaved work for an hour, or clean and 3 days untouched; running processes are named |
| `unpushed` | an agent's worktree holding commits no remote has for an hour |
| `orphan` | processes running in a deleted worktree, with their pids |
| `breach` | more worktrees hold a rule's resources than its `max`, while the rule lasts |
| `queued` | a process waiting 10 min for a resource, with who holds it |
| `refused` | `agent-fleet kill` refused a pid in the last hour, with the reason |
| `role` | an agent's declared lead is down, the lead's level is not above its own, or a second root coordinator is up |
| `quota` | a program's quota is exhausted, or a window ≥ 90% used resets in the future |
| `stalled` | an agent idle or blocked for 15 min after it last worked |

`hung` and `stalled` are also pushed, not only read: the host writes one `msg.sent` per episode
— `HUNG` or `STALLED`, ref the agent's name, the finding's own detail for text — to the agent's
declared lead, else to an up `root-coordinator` (from `state.roles`), else nobody; it never
messages an agent about itself. A hand-started agent has no role and no lead, so its notices go
to the root coordinator. An episode is the stretch since the agent's current state began,
deduped against `msg.sent` already in the log — a restart does not re-report — and each
finding's detail says who was told, or that nobody was.

## Killing processes

`agent-fleet kill <pid>…` is how an agent stops a process. It reads the process table (`ps`) and
refuses, killing nothing, when any pid is:

- a terminal multiplexer (`herdr`), a pane's shell, an agent (`claude`, `devin`, `codex`, or what
  a pane's shell runs), an `agent-fleet` daemon, or an ancestor of any of them;
- the caller's own ancestor (its shell, its script's parent, its agent);
- inside another agent's process tree.

It kills the caller's own processes (its agent's tree) and orphans `doctor` proved (on Linux, still
in a deleted directory): TERM, then KILL after `--grace` seconds. Each refusal is logged as
`kill.refused`.

`agent-fleet hook bash` is a PreToolUse hook for Claude Code (tool `Bash`) and Devin CLI (tool
`exec`, which also reads hooks from `.claude/settings.json`) that refuses (exit 2) a shell command
running a raw `kill`, `pkill` or `killall`, also behind `sudo`, `xargs`, `timeout`, `sh -c`,
`eval`, `find -exec` or `ssh <host>`. `kill -0` and `kill -l` pass. It guards against a slip, not
against an agent set on evading it.

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash|exec",
  "hooks": [ { "type": "command", "command": "agent-fleet hook bash" } ] } ] } }
```

## Every agent runs in herdr

Agents are pane processes: the fleet knows an agent only because herdr hosts it. Two guards
enforce it. `agent-fleet hook bash` (the Claude Code / Devin CLI `PreToolUse` hook) refuses a
shell command that starts `claude`, `devin` or `codex` for a session — behind `sudo`, `nohup`,
`xargs`, `timeout`, `sh -c`, `eval`, `env` or `&` — and points at `agent-fleet spawn` and `herdr
agent start`; the read-only and admin forms (`--version`, `--help`, `auth status`, `doctor`,
`models`, `mcp list`/`mcp get`, `update --check`) pass, and so do one-shot runs that answer and
exit (`claude -p`, `codex exec`) when the caller is itself in herdr (`HERDR_ENV=1`): they run in
the caller's pane tree, so the host counts them as managed. And the host's process scan reports every
agent-program process it finds outside all of herdr's pane trees as `agent.unmanaged` (a process
is managed when any ancestor is `herdr`, and the host's own process tree is excluded; the
process's directory is known on Linux only), re-sent without `program` once the process is gone;
`agent-fleet doctor` shows them as `unmanaged` findings.

## Spawning

The fleet starts agents itself. `<checkout>/.agent-fleet/fleet.toml` in the project says how
(`docs/fleet.example.toml` ships one): a `session`, a `base` ref (default `origin/main`), a
`brief` template next to the file with `@@KEY@@` placeholders, an optional `ready` script run
inside a new worktree before the agent starts, `[vars]` for extra tokens, `[profiles]` —
a `kind` (the `agent.start`/`agent.quota` program), `args`, `unset_env`, a `prefix` for agent
names, an optional per-repo `cap`, a `fallback`, and `handoff_min` — and `[[rules]]`, first
match wins, a rule with no `match` being the default. `match` keywords are case-insensitive
whole words against the issue's title and labels — never the `--scope` text, which narrows the
work and never picks the worker; `label:x` matches only
a label named x. Config errors name the line that is wrong: an unknown profile, a fallback
cycle, two defaults, no default.

`agent-fleet spawn <owner/repo>#<issue>` reads the issue with `gh issue view` (`AGENT_FLEET_GH`
names the binary), picks the rule — `--profile`, `--host` and `--level` override it — then walks
the profile's fallback chain past a profile at `cap` (live agents of the profile for this repo plus pending
requests) or whose kind's quota on the target host is exhausted — a quota counts as exhausted
while a 100% window's reset is still ahead or the `exhausted` read is under 30 min old;
every step lands in `why`. The brief renders in one pass over the template: tokens
NAME, KIND (the profile name), PROGRAM (the kind), SLUG, ISSUE, TITLE, BRANCH, SCOPE, HOST,
LEVEL, LEAD, HANDOFF (empty unless a handoff), the `[vars]` and `--set KEY=VALUE`; a value is
never re-expanded and a placeholder with no value refuses the spawn. `--dry-run` prints the
decision and the brief and writes nothing; otherwise `spawn.requested` is written and the CLI
follows the hub until `spawn.result` arrives (10 min, `--no-wait` skips the wait).

The host agent serves requests naming its host and session, against a checkout given as
`--repo <owner/repo>=<dir>` (a bare `--repo <dir>` watches a checkout and derives the slug from
its `origin` remote). A request names the `profile`; the host reads `kind`, `args`, `unset_env`,
`base` and the checkout's `ready` from its OWN `.agent-fleet/fleet.toml` — a request never
carries executable configuration, and a handoff's worktree is not a config source — and refuses
(`ok=false`) a `name` or `branch` that starts with `-`, or a `branch` `git check-ref-format
--branch` rejects. For a new worktree it fetches
`origin`, calls `worktree.create` (label `<name> #<issue>`, no focus, trust the repository); a
create that fails on an existing branch is retried once after reclaiming it, and only when it
holds no work — no commit the remotes lack, a clean tree. With `worktree` set on the request (a
handoff), it opens a `workspace.create` there instead and never removes the worktree. Then
`ready` runs (new worktrees only, 15 min), each `unset_env` is typed into the pane's shell,
`agent.start` retries `agent_pane_busy` for 10 tries 2 s apart, and the brief goes out the way
that works: `agent.wait` for `idle` (2 min),
`agent.prompt` waiting for working|blocked|done (30 s), one Enter and another wait (15 s) on a
stall, then one re-send — never twice to an agent that started working. On success the host
writes `agent.role` (level, lead from the request) and `task.assigned` for
`task:<repo name>/<issue>` (a 409 is not a failure), answers `spawn.result`, and closes a
`replaces` agent's pane and workspace — never its worktree. Any failure undoes what the spawn
took and answers `ok=false`; a request older than 15 min when first seen is answered `expired`.

`agent-fleet handoff <agent> [--to <profile>] [--note TEXT]` gives the agent's issue, branch and
existing worktree to a new agent of `--to` (default: its profile's fallback) with a HANDOFF note.
The host does the same itself, once per agent on its own host and session, when its spawn
record's profile has a fallback and the agent never reached `working` — either its kind is out
of quota on this host, or `handoff_min` minutes passed since its successful spawn. An agent
that worked, is `working` or `done`, or whose task closed is never handed off; a replacement
request that failed does not block a retry.

## The operator's screen

`agent-fleet tui` shows the state live (every 2 s): agents with their CPU, memory, task and branch;
worktrees with unpushed and dirty work and what they run; tasks; findings; resources with their
holder, queue and rules. Writes go through the
same events as the CLI (`msg.sent`, `task.state`, `task.unassigned`). Two actions are local to
the machine and ask first: removing a worktree no agent works in, and killing the processes
left in one (only those still running there, so a reused pid is never hit).

**Repository hook.** A repository knows what fleet cannot: which database, caches and
simulators a worktree holds, and how its stack stops. If its main checkout has an executable
`.agent-fleet/hook`, removing a worktree runs `.agent-fleet/hook retire <path>` instead of
`git worktree remove`; the hook does the repository's own teardown and exits non-zero to refuse.

## The board

`agent-fleet ui` opens the hub's page in the default browser, already signed in: the token rides
in the URL fragment, which a browser never sends to a server, and the page keeps it in local
storage and drops it from the address bar. `--url` prints the address without the token;
anyone else opening it pastes the token once. The page refreshes every 3 s. Its first tab is a
tree: each repository, its worktrees with unpushed and dirty work, what they run and their
findings, the agents in them with state, CPU and memory, and the tasks those agents hold with
their blockers; then agents outside a known worktree, and open tasks with no agent up, by project.

## Instances

One machine can run several fleets, one per project or product. `--instance <name>` (or
`AGENT_FLEET_INSTANCE`) picks one; each has its own directory, so its own hub log, token, spool and
replica. `agent-fleet hub` writes its URL into the instance's directory, and every other command
of that instance finds its hub there, unless `AGENT_FLEET_HUB` says otherwise. Two hubs on one
machine need two `--listen` ports. A host agent serves one instance and one herdr session.
`agent-fleet instances` lists them.

## Files

Everything of an instance lives in one data directory: `AGENT_FLEET_HOME`, or the platform data
directory (`directories` crate) for the default instance and `instances/<name>` under it for
the others: `hub.db`, `replica.db`, `spool.jsonl`, `spool.rejected.jsonl`, `token`, `hub` (the
hub's URL), and the lock files `spool.lock` and `spool.flush`. `AGENT_FLEET_TOKEN_FILE` and `agent-fleet hub --db` move
the token and the hub database elsewhere.

## Not in v1

Decisions and the human channel (`ask.answered`, `decision.*`), data-defined extension types,
the web board, code-host ingest, a NATS transport, `agent-fleet run`/`kill` process ownership, Windows.
These are v2.
