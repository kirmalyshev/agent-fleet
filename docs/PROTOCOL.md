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
   may only enter through a human-only channel. The one channel is the board's answer route,
   `POST /board/answer`, behind the shared hub token; `POST /events` refuses `actor.kind = human` outright.

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
| | `host.health` | host, disk_free, disk_total, agents, stats?, commit? (the commit its binary was built from), behind_since? (since when its 5-minute fetch has found `origin/main` ahead of that commit; every minute) | | host |
| | `agent.usage` | agent, rss_kb?, cpu_pct?, procs?, cwd? (each scan) | | host |
| | `agent.role` | agent, level? (`root-coordinator` `domain-lead` `implementation-lead` `implementer`; absent `level` removes the role), lead?, domain? | | agent, host |
| machine | `worktree.state` | host, path, repo, branch?, head, unpushed, dirty, dirty_submodules? (of dirty, moved submodule pointers), files[] (only what still differs from the default branch), last_commit, in_main?, herdr? (a linked worktree under herdr's worktree directory: `[worktrees] directory` in its config, default `~/.herdr/worktrees`) (on change) | | host |
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
| blocker | `blocker.raised` | what, owner?, unblocks_when?, pid?, started? (`pid`: a process on the raiser's host it waits on, `started` when that process started) | task | agent, host |
| | `blocker.resolved` | note? (the raiser's host writes one once the blocker's `pid` is gone or started at another time) | re = the raise | agent, host |
| question | `ask.raised` | question, options[], recommendation?, context? | task | agent |
| comms | `msg.sent` | to (an agent id, or `coordinator`), kind, ref, text | | agent, host |
| | `ask.answered` | answer, approve?, approves?, msg — **human only**, via `POST /board/answer` | re = the ask | human |
| | `msg.delivered` | to, observed, rung?, attempts? | re = the send | host |
| | `handoff` | to, next, done?, evidence[], learned? | task | agent |
| spawn | `spawn.requested` | name, agent, host, session, repo, issue?, branch, profile, brief, level?, lead?, worktree?, replaces?, why | | agent, host |
| | `spawn.result` | agent, ok, pane?, worktree?, error? | re = the request | host |
| | `agent.unmanaged` | host, pid, program?, cwd?, since? — no `program` clears the report | | host |
| | `proc.orphaned` | host, pid, cwd?, why?, since? — no `why` clears the report | | host |
| | `proc.hung` | host, pid, pids[]?, agent?, command?, started?, evidence?, since?, checked? — the topmost hung process an agent started and the hung ones under it (the watch tick, every 2 min); no `evidence` clears the report; folded only from the host actor it names (`host:<host>`) | | host |
| | `agent.stuck` | agent, cause?, detail, until? — no `cause` clears the record | | host |
| | `agent.signal` | agent, program, signal, detail? — `prompt`/`turn_end` clear the agent's stored signal | | agent |
| update | `update.requested` | host? — every host (or only `host`) pulls, rebuilds and restarts; written by `agent-fleet self-update` as `host:<this machine>`, and by a host's own timer for itself | | host |
| | `update.result` | host, ok, commit?, error? — folded only from the host actor it names, first answer per host | re = the request | host |

`ask.answered` closes an `ask.raised` from the board's Inbox. The hub writes it together with the
`msg.sent` (kind `ANSWER`, or `APPROVED` when `approve` is true; `ref` = the ask) that rings the
asker's doorbell, so an answer is never recorded without its message. `msg` names that send, and
the host's `msg.delivered` for it is the ack: the Inbox item reads *sent*, then *acked*. `approve`
is a separate, explicit act, never implied by picking an option, and `approves` holds the exact
question text it applies to. Only asks from a `root-coordinator`, `domain-lead` or
`implementation-lead` are in the Inbox and answerable; an implementer's lead answers for it.

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
  `task.assigned.agent`, `agent.role.agent`, `agent.role.lead`) a full agent id — or, for
  `msg.sent.to` only, the role name `coordinator`. A malformed or oversized event is `400`; a forbidden actor (any `human`,
  any `hub`, or a kind not in the table) is `403`. The board's `POST /board/answer`
  (`{ask, answer, approve?}`) is `400` for an unknown ask or an empty answer, `403` for an ask
  outside the Inbox, and `409` for one already answered. Clients apply the same rules before sending,
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
- **Bearer token.** Every endpoint needs `Authorization: Bearer <token>`; a missing or unknown
  token is `401`. The hub creates a random shared token file (mode 0600) on first start;
  clients read the same file.
- **The token binds the actor.** `agent-fleet mint-token <scope> --out <file>`, run where the
  hub runs, writes a new token to `<file>` (mode 0600) and appends `<scope> <token>` to the
  hub's token table, `hub-tokens`. The hub reads the table when it starts. A `host:<name>`
  token may post as `host:<name>` and as agents `agent:<name>/…`; an `agent:<name>` token only
  as agents `agent:<name>/…`. The shared token may post as any actor, except a host that has a
  `host:` token of its own. Any other actor is `403`, checked after the duplicate check, so a
  replay is still acknowledged. Every valid token reads; only the shared token answers the board.
  Upgrading changes nothing until the operator mints tokens: no table, the shared token keeps
  every right it had. Migrate one host at a time: mint `host:<name>`, copy the file to that
  host, run its `agent-fleet host --token-file <file>` (the flag, not `AGENT_FLEET_TOKEN_FILE`,
  which its agents would inherit), restart the hub. From then on only that file posts as that
  host. To take the shared token away from a host's agents, mint `agent:<name>` and make it
  that host's `token` file. A token file guards against other hosts, not against a process of the
  same Unix user on the same machine, which can read it (see Who can post as a host).

## Who can post as a host

A `host:<name>` token stops other machines from posting as `host:<name>`. It does not stop that
host's own agents. The host agent runs as the Unix user its agents run as, and that user can read
every token file the host reads. So an agent can post as its own host, for example a `proc.hung`.
That report alone kills nothing: `agent-fleet kill` checks every hung report against the live
process itself (see Killing processes), so a forged report cannot kill a process that is not
idle by the watch's rules.

The host cannot yet run as a separate user. Each of these host features needs the agents' uid,
and each runs in the host agent, not in the agents' CLI:

- **herdr.** herdr creates its socket with mode 0600, owned by the agents' user. Another
  non-root user cannot connect, so presence, doorbells, spawns, stuck causes and handoffs stop.
- **The process scan and the watch tick.** On Linux, `/proc/<pid>/cwd` and `/proc/<pid>/fd` of
  another user's process are unreadable without ptrace access, so `agent.usage` loses its cwd and
  `worktree.procs` and the orphan proofs miss processes. `lsof` run as another user exits 0 and
  leaves out the agents' sockets and output files. The watch tick then sees a server or a writer
  as idle and reports it `proc.hung`: a false proof that `kill` accepts. On macOS, `lsof` needs root
  for another user's processes.
- **Spawns.** The host runs `git fetch`, `git worktree remove`/`prune` and `git branch -D` in the
  agents' checkout, and the `ready` script in the new worktree. As another user these fail on
  permissions or leave files the agents cannot change, and git refuses a repository another user
  owns unless `safe.directory` names it.
- **Quotas.** The host reads `~/.codex/sessions`, `~/.cache/devin/cli` and
  `claude-rate-limits.json` in its own data directory, and runs `devin auth status`, all as its own
  user. As another user it reads another account, or nothing.
- **Device orphans (macOS).** `simctl list devices booted` lists the caller's simulators. As
  another user the list is empty, so a tool on a booted simulator looks orphaned when it is not.

Root reaches herdr and every process, but then `git` and `ready` write root-owned files into the
agents' repositories, and quotas and simulators are read for root. So root is no answer either.

The host never signals an agent's process: `agent-fleet kill` runs in the agent's own CLI as the
agent, the TUI's kill runs as the operator, and the host signals only its own update build.
Self-update needs only the host's clone, `git` and `cargo`.

What the operator can do today:

1. Mint `host:<name>` on the hub machine and run that host with `--token-file <file>` (see The
   token binds the actor). Other machines can no longer post as that host.
2. Mint `agent:<name>` and make it the host's `token` file in its data directory, the file every
   agent CLI on the machine reads by default. Its agents lose the shared token, but they can
   still read the host's own file.
3. Keep the hub out of the agents' reach: run it on a machine, or as a user, no agent runs as,
   from a binary and a data directory agents cannot write. Its data directory holds the shared
   `token` and `hub-tokens`, every scoped token. The hub follows its binary, so a binary an agent
   can replace runs the agent's code as the hub. Agents that cannot read the hub's URL file need
   `AGENT_FLEET_HUB`.

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
is still up — `updates` (who asked, each host's `update.result`) keyed by `update.requested` id,
kept for 24 h — `quotas` (windows, plan, exhausted, read_at, exhausted_since — when the reports
first showed the account out) keyed by `<host>:<kind>`,
`tasks` (state + since, owner, lead,
blocked_by, handoffs), open `blockers` and `asks` (with their context link) keyed by their raising event id, undelivered
`msgs` keyed by their `msg.sent` id, the id of the `last` event folded, and the `unknown` count.

## Doorbells

`agent-fleet tell <agent> <TYPE> <ref> <text>` writes `msg.sent`. `<agent>` is an agent that is
up — a full id or its short name — or `coordinator`; anything else (an agent or pane that is
gone, `coordinator` while no agent that is up holds the role) is refused with a non-zero exit
and nothing is written. `coordinator` stays in the event: the host resolves it at each delivery
attempt to the up agent holding the `root-coordinator` role (of several, the latest declared),
so a message already queued follows a coordinator that restarts in another pane or hands over.
The host agent of the recipient's machine types it into the agent as

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
| `hung` | working for 30 min at under 2% CPU, its worktree unchanged as long; or (subject `<host>:<pid>`) a process the watch tick reported in `proc.hung` |
| `heavy` | an agent's tree, or a worktree's processes, over 4 GB or 200% CPU |
| `duplicate` | two agents in one worktree, or two agents' worktrees changing the same files (lockfiles aside) |
| `forgotten` | a worktree herdr made (`herdr`; spawn makes its worktrees through herdr) that no agent works in, with unsaved work for an hour, or clean and 3 days untouched, or clean with processes still running 10 min after it last changed; running processes are named, dirty files that are only moved submodule pointers say so, and `agent-fleet kill` accepts them unless the worktree is a main checkout |
| `unpushed` | an agent's worktree holding commits no remote has for an hour |
| `orphan` | processes running in a deleted worktree, with their pids |
| `breach` | more worktrees hold a rule's resources than its `max`, while the rule lasts |
| `queued` | a process waiting 10 min for a resource, with who holds it |
| `refused` | `agent-fleet kill` refused a pid in the last hour, with the reason; a pid already gone (`no such process`) is no finding |
| `role` | an agent's declared lead is down, the lead's level is not above its own, or a second root coordinator is up |
| `quota` | a program's quota is exhausted, or a window ≥ 90% used resets in the future |
| `stalled` | an agent idle or blocked for 15 min after it last worked |
| `update` | a host's latest self-update that it answered `ok:false` (for 2 hours, and for as long as the host's `host.health` still names the commit the failure reports: nothing asks it again until `origin/main` moves; the hub keeps, past its usual day, the update a host answered last if that answer failed on its current commit; a host that has not sent `host.health` for a day is gone, not failing), or that a live host (`host.health` in the last 3 min) has not answered 75 min after it was asked (the wait for another update's lock, 35 min, plus a build, 30, plus git). A request already older than 15 min when the host process started was ignored by it, not left unanswered. A later success clears it. Subject `host:<host>`, with who asked and the error |
| `build` | hosts that sent `host.health` in the last 10 min report different commits (or one reports none); each such host, with the others' commits. When they all agree: each one whose `behind_since` is 15 min old or more (the fleet is on a commit `origin/main` has left; its self-update did not take), with how long |

`hung` and `stalled` are also pushed, not only read: the host writes one `msg.sent` per episode
— `HUNG` or `STALLED`, ref the agent's name, the finding's own detail for text — to the agent's
declared lead, else to an up `root-coordinator` (from `state.roles`), else nobody; it never
messages an agent about itself. A hand-started agent has no role and no lead, so its notices go
to the root coordinator. An episode is the stretch since the agent's current state began,
deduped against `msg.sent` already in the log — a restart does not re-report — and each
finding's detail says who was told, or that nobody was.

## Stuck causes

Every 5 seconds the host looks at each of its `idle` or `blocked` agents — and at a
`working` one `doctor` calls `hung` — and names ONE cause. The cause is authoritative only
when it comes from a structured signal: the agent's own `agent.signal` (the latest one
stored for the agent — `prompt`, `turn_end` and turning `working` clear it, so what remains
belongs to this episode, and one older than the last working stretch is a replay), or herdr's
`blocked` and the fleet's own holds. For Devin, which
emits no hooks, an exhausted `agent.quota` for `<host>:devin` plus the agent sitting idle
*is* the structured quota signal — but only when the agent's current stop began after the
account ran out and its task is still open: an agent already idle or finished is just idle. The screen is still read, but only as a hint: it may
ride along in the message to the lead ("screen hints …"), and it never decides a cause or
drives an action. The result lands in `agent.stuck` (`cause`, `detail`, `until`), written
when the cause appears or changes, and once with no `cause` when the agent works again.
State `stuck` is keyed by agent; `ps`, the TUI and the board show the cause next to the
state, `doctor` puts it in the `hung`/`stalled` detail, and the push names it too — for
`permission`, `waiting` and `unknown` it also carries the screen's last lines.

| Cause | Signal |
|---|---|
| `quota` | `agent.signal` `quota`, or for Devin an exhausted `agent.quota` for `<host>:devin`. `until` is the spent window's `resets_at` (from `agent.quota`) when known. |
| `rate_limit` | `agent.signal` `rate_limit`. `until` is the wait named in `detail` (5 min when unparseable). |
| `question` | `agent.signal` `question` — the program is asking the principal; `detail` is its text. |
| `context_full` | `agent.signal` `context_full` (the statusline wrapper at ≥ 90% context) or `compacting`. |
| `error` | `agent.signal` `error`. |
| `account` | `agent.signal` `account` — billing, hold or auth; no retry cures it. |
| `permission` | `agent.signal` `permission`, or herdr reports `blocked`: a permission or trust prompt waits. |
| `resource` | its worktree holds or queues for a resource: "waiting for sim behind the e2e run, ~21m". A legitimate wait, never `hung` or `stalled`. |
| `held` | its open task is deliberately held: an open `blocker.raised` (with `what`/`owner`/`unblocks_when`) or an unfinished `task.dep`. |
| `waiting` | idle after it worked, no signal (finished, or asked something) — also `agent.signal` `idle`. |
| `unknown` | none of the above — including a spawned agent that never reached `working`. |

Each cause earns at most one action per episode, logged as events (`msg.sent`,
`spawn.requested`), and never on an agent that is `working` unless `hung`, nor on one whose
task is finished. Actions that touch an agent — a `continue` doorbell, a handoff — only
run under `host --stuck-act`; without it the host still classifies and records `agent.stuck`,
and every would-be action becomes one tell to the lead naming what it would have done. And
nothing acts or tells before the state has lasted `--stall-min`:

- `quota` with `until` KNOWN and more than 30 min away: hand the work to the profile's
  `on_quota` (a fleet.toml key), but only when that profile's kind still has quota on this
  host. `until` within 30 min or unknown: tell the lead it is out of quota until `until` —
  an unknown reset is not a reason to move the work. A target whose kind is the agent's own
  is never a quota handoff —
  a `haiku` profile's kind is `claude`, it shares the Claude account's limits, so a Claude
  agent out of quota is never handed to it on the same host.
- `rate_limit`: wait; `continue` once after `until`.
- `question`: tell the lead, and list it on the board under "Waiting for the principal" —
  the principal's only channel is the board; nothing pushes.
- `resource`: no action — the wait is legitimate — until it beats 2× the holder's stated
  ETA (a "~21m" in its scope) or 60 min; then one tell to the lead.
- `held`: nothing at all — no push, no escalation; `doctor` shows `held`, not `stalled`.
- `permission`, `waiting`, `unknown`: the `HUNG`/`STALLED` push tells the lead, with the
  cause and the screen's last lines.
- `context_full`, `error`, `account`, `unsent` (screen hint only) and the never-started
  agent: one tell to the lead. No keystrokes, no handoffs — those two were the dangerous
  guesses: an Escape could wipe a long pasted brief, and an automatic handoff on anything
  but quota spawned agents the fleet could not pay for.

When a stalled agent's tell got no reaction — the lead stayed idle/done for `--stall-min`
after the `HUNG`/`STALLED` push, an escalation, or the agent's own message was delivered —
the host sends ONE `ESCALATED` `msg.sent` to the lead's own lead, and so on up the
`agent.role` chain to an up root-coordinator, never past it. Each hop waits its own
`--stall-min` and dedupes like every tell: once per episode, by the log.

## Signals

`agent.signal` is the structured channel a program speaks through. `agent-fleet hook signal`
reads a harness's hook JSON on stdin and writes the event — identified from the herdr
environment exactly as `agent-fleet`'s own `me` does (`HERDR_ENV`, `HERDR_PANE_ID`,
`HERDR_SESSION`, `HERDR_SOCKET_PATH`, and `agent.get` for the pane's name and program).
Outside herdr, on unmapped events, on any error — argument errors included — it exits 0
silently. The event lands in the spool first and the client then flushes what the budget
allows: a hub that never answers still cannot lose the signal, and the whole pass is capped
at one second so the hooked program is never slowed. Claude Code is the complete map;
Devin CLI and Codex match where their hook payloads share the shape.

| Hook | Maps to `signal` |
|---|---|
| `StopFailure` `error: rate_limit` | `rate_limit` |
| `StopFailure` `error: overloaded`, `server_error`, anything else | `error` |
| `StopFailure` `error: billing_error`, `account_on_hold`, `authentication_failed`, `oauth_org_not_allowed` | `account` |
| `Notification` `permission_prompt` | `permission` |
| `Notification` `idle_prompt` | `idle` |
| `Notification` `agent_needs_input`, `elicitation_dialog`, `elicitation_url_dialog` | `question` (`detail` = `message`) |
| `Notification` `quota_auto_resume_stale`, `quota_auto_resume_disabled` | `quota` |
| `Notification` `quota_auto_resume_fired` | `resumed` |
| `PreCompact` `trigger: auto` | `compacting` |
| `Stop` | `turn_end` |
| `UserPromptSubmit` | `prompt` |
| statusline `context_window.used_percentage` ≥ 90 | `context_full`, once per session (deduped in `claude-context-full.json` next to `claude-rate-limits.json`, keyed by `session_id`) |

`StopFailure`'s `detail` is `error_details` plus the first line of `last_assistant_message`.
`prompt` and `turn_end` mark a new episode: they clear the stored signal instead of storing
one. An unknown hook event writes nothing.

Install all of them in the project's `.claude/settings.json` (Devin CLI reads the same file).
Every command ends in `|| true`: a Stop or UserPromptSubmit hook exiting 2 blocks Claude
Code, and an older `agent-fleet` without `hook signal` exits 2 on the unknown subcommand —
the `|| true` keeps an old binary from blocking the program it hooks.

```json
{ "hooks": {
  "Stop":             [ { "hooks": [ { "type": "command", "command": "agent-fleet hook signal || true" } ] } ],
  "StopFailure":      [ { "hooks": [ { "type": "command", "command": "agent-fleet hook signal || true" } ] } ],
  "Notification":     [ { "hooks": [ { "type": "command", "command": "agent-fleet hook signal || true" } ] } ],
  "PreCompact":       [ { "hooks": [ { "type": "command", "command": "agent-fleet hook signal || true" } ] } ],
  "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": "agent-fleet hook signal || true" } ] } ] } }
```

`agent-fleet hook statusline` stays the status-line wrapper: it stores the `rate_limits`
it is fed for the host's `agent.quota` read, emits `context_full` as above when the process
sits in a herdr pane, and passes the input on to the command it wraps.

## Killing processes

`agent-fleet kill <pid>…` is how an agent stops a process. It reads the process table (`ps`) and
refuses, killing nothing, when any pid is:

- a terminal multiplexer (`herdr`), a pane's shell, an agent (`claude`, `devin`, `codex`, or what
  a pane's shell runs), an `agent-fleet` daemon, or an ancestor of any of them;
- the caller's own ancestor (its shell, its script's parent, its agent);
- inside another agent's process tree, unless a `proc.hung` report names it and `kill`'s own
  look at the process agrees (below).

It kills the caller's own processes (its agent's tree) and orphans `doctor` proved: processes in
a deleted worktree or a `forgotten` linked worktree (never a repository's main checkout) (on
Linux, still in that directory). It kills an orphan (parent pid 1, `systemd` or `launchd`) whose
working directory is inside the caller's worktree, unless that path is also inside a worktree
recorded for another agent. The caller's worktree is the one from its most recent `spawn.result`
answered ok on this host, never one it requested for itself and never its pane's live cwd. An
agent no one else spawned (started by hand in a pane) has instead the worktree holding the first
`agent.usage` cwd its own host reported for it, fixed until `agent.down`. A repository's main
checkout (a worktree whose path is its repo) is never the caller's worktree, from either source:
its processes may be the human's. It still counts as recorded for the agent there. With no such
record, a record that is a main checkout, or a spawn that failed or is unanswered, the orphan is
refused. It also kills the pids of a `proc.hung` report on the caller's
host, also inside another agent's tree, when the live process started before the report's
`since` (a later one with that pid is not the one that hung) and the report was `checked` in
the last 6 minutes (one nobody re-checks proves nothing), and only when `kill`'s own look at the
live process agrees. A report is a claim: any process that can read the host's token file can
post one. So, for each named pid a report covers, `kill` reads the process table twice 5 s
apart, then runs the watch tick's `lsof` reads once, and refuses unless, by the watch's own
rules:

- the pid is still there and started before the report's `since`;
- it is a process the watch classifies at all: not an agent's direct child (its tool shell or
  an MCP server) and not an MCP server started through a shell;
- nothing in its tree used over 1% CPU in the 5 s, is a device tool, started a child, waits in
  a watched lock's queue (as the log has it), or has a listening TCP socket;
- nothing in its tree started, and its output file did not change, in the last 4 minutes (the
  watch reports a hang only after 2 idle 2-minute ticks);
- nothing in its command (the shell its agent runs it in, so all of a pipeline) holds an
  established TCP connection;
- `lsof` answered.

Five seconds makes the 1% line 50 ms of CPU, several clock ticks on Linux and macOS, and is
short enough to wait for; the 4-minute look-back covers output and child processes that come and
go between samples. The refusal names the reason (`the hung report is not confirmed: pid N
(java) used 200 ms of CPU in 5000 ms`). The same check applies to every hung report, inside
another agent's tree or not. TERM, then KILL after `--grace` seconds. Each refusal is logged as
`kill.refused`.

`agent-fleet hook bash` is a PreToolUse hook for Claude Code (tool `Bash`) and Devin CLI (tool
`exec`, which also reads hooks from `.claude/settings.json`) that refuses (exit 2) a shell command
running a raw `kill`, `pkill` or `killall`, also behind `sudo`, `xargs`, `timeout`, `sh -c`,
`eval`, `find -exec` or `ssh <host>`. `kill -0` and `kill -l` pass. It guards against a slip, not
against an agent set on evading it.

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash|exec",
  "hooks": [ { "type": "command",
    "command": "command -v agent-fleet >/dev/null 2>&1 || exit 0; agent-fleet hook bash" } ] } ] } }
```

The guard passes everything when the binary is absent, and an exit 2 refusal still
blocks the tool call when it is there.

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
is managed when any ancestor is `herdr`, the host's own process tree is excluded, and so is
a program's own infrastructure — an `app-server` or `daemon` in the subcommand slot (the
first argument; a `daemon` inside a `-p` prompt is no daemon), as in `codex app-server
daemon` or `claude daemon`, and everything under such a process; the
process's directory is known on Linux only), re-sent without `program` once the process is gone;
`agent-fleet doctor` shows them as `unmanaged` findings.

`doctor` also reads GitHub, not the log, for `orphan-pr` findings: for each repository in
`[orphan_pr] repos` of fleet.toml (absent: the machine's connected `[repos]`; `[]`: off), an open PR whose head branch is no live agent's worktree branch and that has had no
update for `days` (default 3) is named with its age, the issues it closes and their state. `doctor --apply` closes it, with a comment and the branch kept, only when every issue it closes is closed or closed by a newer
merged PR in the same repo; anything else is reported. A draft, or a PR touching a `guarded` path, is never
closed; `--apply` tells the root coordinator of each orphan it does not close, again on a later run once the earlier
tell is delivered; a tell still pending is not repeated. The default `guarded` is
`AGENTS.md`, `CLAUDE.md`, `README.md`, `docs/`, `schema/`; setting it replaces that list. A PR whose
file list hits `gh`'s cap of 100 counts as guarded. Nothing is closed while a live agent's
branch is unknown; a repo or row `gh` cannot read is an `orphan-pr` error line, and so is a
fleet.toml `doctor` cannot read (no fleet.toml, or no checkout, means the check is off). With `[orphan_pr] apply = true` (default off) the host's watch loop runs the same step every 30 minutes
from the fleet.toml of each spawn checkout the host knows (`[repos]` and `--repo`) (never its working directory: a service starts it in `/`), on its own task with a 5-minute limit: it closes the superseded PRs and tells the root
coordinator of each other orphan, again 7 days after the last tell and never while one is still pending (the log is the dedupe); with no root coordinator it tells nobody, and the closes still run.
When the TUI removes a worktree whose branch still has an open PR, its outcome line says so and
the root coordinator is told: the PR is a coordinator item from then on.

The same scan proves orphans and reports them as `proc.orphaned` (`why` says which proof
held; no `why` clears it). A process reparented to init that is a device or test tool —
`xcodebuild`, `maestro` (named `maestro`, or `java` whose args carry `maestro.cli.AppKt` or
`/.maestro/lib/`), `simctl`, `idb`, `xctest`, a `WebDriverAgentRunner`, or `log stream` —
whose process-group leader is dead (the group id names no live process) is an orphan once it
is two minutes old — the floor only spares a script mid-`exec`; its descendants are orphans
with it. When the leader is alive the older proof remains: two hours and a UDID that is
neither connected (`devicectl list devices`) nor booted (`simctl list devices booted`); a tool
that names no device, or a list the toolchain cannot produce, proves nothing that way. And a
lock's recorded holder still alive under init while its `owner` file names a dead process is
an orphan whatever program it runs — the tool list does not apply — and so is every member of
the lock's recorded `pgid` group whose ancestry reaches init or a proved pid: an owner that
exits normally releases the lock first, so a member of its group under a still-held lock is
an orphan by definition, with no age or device check. The device list is only read when a
reparented tool exists to check. Doctor shows them as `orphan` findings and `agent-fleet
kill` accepts them.

### The watch tick

Every 2 minutes the host classifies every process an agent started: everything under the
shells its agent program runs (an agent's own process, its pane shell and its other direct
children — MCP servers — are never classified), plus the proved orphans, each claimed by the
agent whose worktree it runs in. A process under 2 minutes old or with no start time is not
classified, nor is one with a listening TCP socket (a server it keeps) or `mcp` in a non-flag word of its argv (an
MCP server started through a shell), or anything under them. Nor is a command with an
established TCP connection anywhere under its shell (a network wait such as `gh pr checks
--watch | tail`: a poller and its pipe readers sit at 0% CPU between polls). Each gets one state:

- `working`: over 1% CPU over the interval, its stdout/stderr file changed, a child started
  since the last tick, a device tool other than `simctl bootstatus` (`xcodebuild`, `maestro`,
  `simctl`, `idb`, `xctest`, …), or a descendant that is any of these, waits, or listens;
- `waiting`: in a watched lock's queue (`<lock>.waiting/`), with the lock and since when;
  nothing under it is classified;
- `hung`: none of the above for 2 ticks in a row. Output on a pipe or terminal is not seen, so
  it does not count as work. When `lsof` gives no answer (an error, a timeout), nothing is hung and the tick does not count;
  a tick after a gap of more than two intervals (the machine slept) only re-reads the table;
- `orphan`: a `proc.orphaned` proof holds.

The topmost hung process of a chain is reported as `proc.hung` with every hung pid under it
(tops of one agent in one process group, the members of a pipeline, are one report: the
lowest pid's),
its command, start, and evidence (CPU, the output file's last change, the deepest descendant's
command, the lock it holds), re-sent every tick that confirms it with `checked`. The report
clears when its pid is gone or is now another process, when a parent's report covers it, or
when its agent's tree was walked and found it working or waiting, or when no tick confirmed it
for 6 minutes and it is not idle now; until then a tree the tick could not read, or a process that left the tree,
leaves it standing. A `hung` or `orphan` process, and a
`waiting` one past 30 minutes, are told once per episode — a `msg.sent` of kind `HUNG`,
`ORPHAN` or `WAITING` with `ref` `<host>:<pid>` — to the agent that started it, its lead and
the root coordinator; a hung tell waits until its report is in the log. An orphan proved by a
dead lock owner also lists the simulators booted on the host, which may keep its holder alive,
with the `xcrun simctl shutdown` that releases them. Doctor shows `proc.hung` as `hung` findings
(subject `<host>:<pid>`), `agent-fleet kill` accepts its pids as above, and `agent-fleet proc
<pid>` prints what the log says of a pid on this host: `hung`, `orphan`, `waiting`, `no report`
or `gone`. Nothing is killed automatically.

## Spawning

The fleet starts agents itself. A `fleet.toml` says how (`docs/fleet.example.toml` is the
commented reference, and the template `agent-fleet init` writes): a `session` (absent: the herdr session the CLI runs
in, from `HERDR_SESSION`), a `base` ref (default `origin/main`), a
`brief` template next to the file with `@@KEY@@` placeholders, an optional `ready` script run
inside a new worktree before the agent starts, `[vars]` for extra tokens, `[profiles]` —
a `kind` (the `agent.start`/`agent.quota` program), `args`, `unset_env`, a `prefix` for agent
names, an optional per-repo `cap`, a `fallback`, and `handoff_min` — and `[[rules]]`, first
match wins, a rule with no `match` being the default. `match` keywords are case-insensitive
whole words against the issue's title and labels — never the `--scope` text, which narrows the
work and never picks the worker; `label:x` matches only
a label named x. Config errors name the line that is wrong: an unknown profile, a fallback
cycle, two defaults, no default.

The machine's rules are `fleet.toml` and its brief in the user config directory
(`~/.config/agent-fleet` on Linux, `AGENT_FLEET_CONFIG_DIR` overrides it). `agent-fleet init`
writes both from `docs/fleet.example.toml` and `docs/brief.example.md`, with a profile for each
agent program on `PATH` (`claude`, `codex`, `devin`; all three when none is found) and a default
rule on the first; it never overwrites a file. It also lists the user-level skills
(`$CLAUDE_CONFIG_DIR` or `~/.claude`, `$CODEX_HOME` or `~/.codex`, each `/skills/*/SKILL.md`)
that name herdr, tmux or cmux, and warns that in a fleet agents message each other with `tell`
and start agents with `spawn`; it changes nothing about them. A checkout with its own
`.agent-fleet/fleet.toml` uses that file instead, wholesale; otherwise the machine's applies.
Config errors name the file that was read.

**Connected repositories.** The machine's `fleet.toml` has a `[repos]` table, `"owner/repo" =
"<checkout>"`, read only from that file. `agent-fleet repo add <owner/repo> [<dir>]` records the
git toplevel of `<dir>` (default: the current directory); it refuses a directory outside a git
checkout and a checkout whose `origin` is not a GitHub remote of `<owner/repo>` (compared
case-insensitively), because the host would create that other repository's worktrees and
branches. It edits that one line and keeps every comment; adding the same checkout again changes
nothing. `repo rm <owner/repo>` removes the line, `repo ls` lists the table. `repo add` lists the
checkout's project-level skills that name a terminal multiplexer the same way, from
`.claude/skills` and `.codex/skills`. `repo add` needs
`init` first.

`agent-fleet spawn <owner/repo>#<issue>` reads the issue with `gh issue view` (`AGENT_FLEET_GH`
names the binary), picks the rule — `--profile`, `--host` and `--level` override it. A profile at
`cap` (live agents of the profile for this repo plus pending requests) has no seat: the spawn is
refused, never moved to another profile. A profile whose kind's quota on the target host is
exhausted — while a 100% window's reset is still ahead or the `exhausted` read is under 30 min
old — passes to its `fallback`, unless `--profile` named it: an explicit profile is started as
named or refused. Every step lands in `why`. The brief renders in one pass over the template: tokens
NAME, KIND (the profile name), PROGRAM (the kind), SLUG, ISSUE, TITLE, BRANCH, SCOPE, HOST,
LEVEL, LEAD (`coordinator` when the lead is the root coordinator, else the lead's agent id),
HANDOFF (empty unless a handoff), the `[vars]` and `--set KEY=VALUE`; a value is
never re-expanded and a placeholder with no value refuses the spawn. `--dry-run` prints the
decision and the brief and writes nothing; otherwise `spawn.requested` is written and the CLI
follows the hub until `spawn.result` arrives (10 min, `--no-wait` skips the wait).

Work with no issue: `agent-fleet spawn <owner/repo> --title "<what>"`. The rules match on the
title, the default name is the profile's prefix plus the title's words (32 characters at most),
ISSUE renders empty, and the task is `task:<repo name>/<name>`. The CLI creates that task and,
once `spawn.result` is ok, assigns it to the new agent; under `--no-wait` it stays unassigned.
Every brief also has REF (`owner/repo#N`, or `owner/repo` with no issue) and TASK (the task id).

The host agent serves requests naming its host and session, against the checkout of the
request's repository: the machine's `[repos]`, re-read at each spawn and each scan so a repository
connected after the host started is served without a restart, merged with `--repo
<owner/repo>=<dir>` flags (a bare `--repo <dir>` watches a checkout and derives the slug from its
`origin` remote). A flag wins over `[repos]` for the same repository. With neither, the request
fails with `no checkout for <owner/repo> on this host: run agent-fleet repo add`. A request names
the `profile`; the host reads `kind`, `args`, `unset_env`, `base` and `ready` from that
checkout's `.agent-fleet/fleet.toml`, else the machine's — a request never carries executable
configuration, and a handoff's worktree is not a config source — and refuses
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
The host does the same itself only for the quota case and only under `--stuck-act` — an agent
that never reached `working` whose kind went out of quota on this host before it stopped, with
the reset known and far out, goes to `on_quota` as under
Stuck causes; without `--stuck-act` the would-be handoff is a tell, deduped like every tell.
Any other never-started agent (`handoff_min` minutes since its successful spawn
without reaching `working`) is a tell to the lead, never a handoff. An agent
that worked, is `working` or `done`, or whose task closed is never handed off; a replacement
request that failed does not block a retry.

## Self-update

A host runs the binary `cargo install --path <clone>` built from its own clone. The build embeds
the clone's commit (`build.rs`, `git rev-parse HEAD`) and the clone's path (`CARGO_MANIFEST_DIR`);
`host.health` reports the commit, `agent-fleet ps` shows it, `agent-fleet --version` prints it
after the crate version, and `doctor` reports `build` when live hosts disagree, or agree on a
commit that has been behind `origin/main` for 15 min.

`agent-fleet self-update` writes `update.requested`. Each host that sees it within 15 min and
has not answered it yet, in the clone its binary was built from: refuses a clone not on `main`
or with uncommitted changes to tracked files, runs `git pull --ff-only`, and, unless the
pulled head is the commit it already runs, runs `cargo install --locked --path <clone> --root
<tmp>` (the `cargo` beside its own binary, else the one on `PATH`) with `<tmp>` in its binary's
directory, runs the build's `--version` and refuses one that does not name the pulled head
(the old binary stays, the answer is not ok), and renames the build over its binary — never overwriting it in place, which kills
a running binary on macOS. It answers `update.result` (ok, the commit it will run, or on failure the commit it still runs, and the
error), then exits with status 75 so its supervisor (systemd `Restart=on-failure` or launchd
`KeepAlive`) starts the new binary. The CLI waits (20 min) until each host that sent
`host.health` in the last 3 min has refused, or reports in `host.health` the commit it named,
and prints each one's commit. `update.requested` makes every host rebuild and restart. Its writers list names `host` only, and
the CLI writes it as `host:<this machine>`, but that is a schema rule, **not access control**:
the hub has one shared token and an actor names itself, so anyone holding the token can still
write it, as any other event. Treat the token as the right to restart the fleet.

**Automatic trigger.** Every 5 minutes a host fetches `origin/main` of its clone. When the commit
its binary was built from is an ancestor of `origin/main` and not equal to it, it writes
`update.requested` with `host` set to itself, so no other host runs it; it runs like any other
request. It asks once per `origin/main` commit: a build that failed is not asked for again until
`origin/main` moves. It asks nothing while a request for this host waits, while an update holds
the clone's lock, or when its commit is not an ancestor of `origin/main` (a build from
elsewhere) or unknown. From the first fetch that finds it behind until one finds it is not, its
`host.health` carries `behind_since`; a failed fetch or a held lock leaves it as it was.

The update is hardened:

- **Work in flight.** A spawn execution or handoff running when the build finishes would be cut
  off by the exit and run again from scratch. The host answers `update.result`, then stops
  handing out guards for new spawns and handoffs (a request that arrives later is run by the
  new process), waits until the running ones end, then exits. It waits at most 10 minutes (under the
  15 at which a spawn request is answered `expired`, so one that arrived as the drain began is
  still fresh for the new process), then tells the root coordinator what was cut off and exits
  anyway, so a hung one cannot keep the old binary running. If the answer could not be
  written, the host still restarts: the new process finds the request unanswered and answers it.
- **Time limits.** `git pull` 2 min, other git 1 min, `cargo install` 30 min; the process group
  is killed on a timeout and the update answers with the error. No command reads stdin, and git
  never prompts (`GIT_TERMINAL_PROMPT=0` for every command it runs; ssh `BatchMode` for git, unless
  `GIT_SSH_COMMAND`, `GIT_SSH` or the clone's `core.sshCommand` is set).
- **One at a time.** An `flock` in the clone's git dir, taken before the checks and held through
  pull and build: a second update in the same clone (two hosts on one machine) waits for the
  first (up to 35 minutes), then runs; it is never raced, and a wait that times out answers with
  an error. The timer's fetch does not wait: it skips its turn.
- **Rollback.** The previous binary is kept as `<binary>.prev` (a hard link, else a copy, made
  under a temporary name and renamed into place, so a failed copy leaves the previous rollback
  copy whole); one generation, replaced by each update. If it cannot be kept the update fails
  before the rename.
  To roll back, rename it over the binary and restart the host. The hub on that machine does
  not follow a rollback to an older commit (an old binary refuses a log that holds event types it
  does not know): it keeps running the newer code; restart it by hand to roll it back too, only
  while the log holds no newer event types.
- **Leftovers.** At start a host removes `.agent-fleet-update-<pid>` directories from its
  binary's directory that are older than 45 minutes and whose process is gone (a killed update
  leaves one; a live pid is another host's build in progress).

**The hub follows the binary.** Every 5 seconds the hub looks whether the file it runs from was
replaced (a new device or inode at its path). The same new file must be seen on two polls in a row
(a copy still being written changes it more than once). Then three checks, all of which must pass:

- `<binary> --version` exits 0 and prints `agent-fleet <version> <commit>` (the commit it was
  built from; outside a git checkout it prints no commit, and so cannot be followed);
- the commit is the hub's own or one built on it (`git merge-base --is-ancestor` in the clone the
  hub was built from). An older or unrelated build (a rollback), a commit git does not know, no
  clone to ask, or a hub that does not know its own commit: **not followed**, unknown means no;
- `<binary> hub --check` (the hub's own arguments, plus `--check`) exits 0: it opens the log
  read-only and folds it, and parses the token table, the two things a new hub does before it
  listens that can stop it from starting and leave none (an event this binary cannot read, a
  table line it rejects). Nothing is written, and a missing log is not created.

A replacement that fails any of them is not followed: the hub keeps running and logs why. When
the failure is definite (an older or unrelated build, a log or table the binary cannot read, a
line with no commit) it looks again only when the file is replaced again (restart the hub by
hand to run it anyway). When nothing was proved (the binary or git did not answer in time, git is
missing, a commit it has not fetched) it asks again every minute and logs when the reason
changes. The file is looked at once more after the checks, and if it was replaced meanwhile the
new one is checked instead.
Otherwise it waits for the request it is serving to finish and replaces its process with the new
binary, with the same arguments and environment (`exec`: the same pid, so a supervisor sees no
exit, and a hub started by hand needs none). If the exec fails it exits with status 75, for a
supervisor to start. So a host's self-update brings the hub on the same machine along (the host
replaces the binary and exits; the hub notices within seconds), and so does any other
replacement of the binary, `cargo install` or `cargo build` included. A change to the protocol
that adds a field still needs the hub on the new binary before the hosts that write it: until it
is, it refuses what they write.

**Logs.** The host logs one line when its timer asks itself to update (both commits), and one
when an update ends: built and installed (the new and old commit), already current, or failed
with the cause.

**Tools.** The watch tick finds `lsof` at `/usr/sbin/lsof` (where macOS keeps it) or
`/usr/bin/lsof` before falling back to `PATH`: a supervisor's `PATH` (launchd's default) lacks
`/usr/sbin`.

## The operator's screen

`agent-fleet tui` shows the state live (every 2 s): agents with their CPU, memory, task and branch;
worktrees with unpushed and dirty work and what they run; tasks; findings, with what `proc` says
of each pid a finding names; resources with their holder, queue and rules; and the board's Inbox.
Every action asks first and shows its outcome line.

**Everything the CLI does.** `:` opens a command line that runs the CLI itself: this binary, with
the screen's `--instance`, `--hub`, `--token-file`, `--as`, `--project` and `--host-name`, in the
background. So each action writes the same events, with the same checks and refusals, as the
command typed in a shell. Tab completes commands, agents, tasks, blockers, levels and the
profiles of fleet.toml. A command that only reads (`stats`, `doctor`, `proc`, `ps`, `ui`, `task
ls|show`, `role <agent>`, `api schema`, `help`, `worktree` without `--execute`
and any `--help`) runs at once; any other asks first. `spawn` runs its `--dry-run` first
and spawns on `y`, following until `spawn.result` as the CLI does. A failure, or an output longer
than one line, opens over the screen; `o` opens the last one again. The services (`hub`, `host`)
and the hooks are refused there. Keys fill the command line from the selected row: role and
handoff for an agent; task add, assign, block, unblock, dep add and rm for a task; kill for a hung
or orphaned process finding (on this host); a capacity rule; spawn; a tell to the coordinator.

**Asks.** The Asks tab is the board's Inbox: `a` answers and `A` approves an open question
through `POST /board/answer`, so it needs the shared token, as the board does.

Two actions are local to the machine: removing a worktree no agent works in, and killing the
processes left in one (only those still running there, so a reused pid is never hit).

**Repository hook.** A repository knows what fleet cannot: which database, caches and
simulators a worktree holds, and how its stack stops. If its main checkout has an executable
`.agent-fleet/hook`, removing a worktree runs `.agent-fleet/hook retire <path>` instead of
`git worktree remove`; the hook does the repository's own teardown and exits non-zero to refuse.

**Cleaning worktrees.** `agent-fleet worktree ls|clean [--execute] [--repo owner/repo]` reads the
worktrees and local branches of the machine's `[repos]`, of the main checkouts the host reports
in `worktree.state`, and of the checkout it runs in. A branch is finished when its tip is the head
of one of your merged or closed PRs (`pr`), is in the default branch (`in main`), or is in
`origin/<branch>` (`pushed`); an open PR keeps it. Only `pr` retires a worktree, through the
repository hook as above, and the outcome line reports an open PR on its branch as `x` does. A
worktree is kept when it is the main checkout, is locked (the reason first), is the caller's,
holds uncommitted files, or a live agent works in it: its cwd (`agent.usage`) is in it or it was
spawned into it (`spawn.result`). `doctor` uses the same agents for `forgotten`. A lock whose
reason names a pid that is gone (Claude Code's `claude agent <id> (pid N start S)`) is called
stale, with its `git worktree unlock` command; clean never unlocks or forces. Processes whose cwd
is in a worktree clean retires, read live from the process table (Linux; macOS reports no cwd),
are stopped before it goes: TERM, then KILL after 5 s, each re-read just before its signal so a
pid reused by another process (another start time or cwd) is never hit. `ls` and the dry run list
them, pid and command. A herdr pane, anything under an agent program (`claude`, `codex`, `devin`)
and the caller and its ancestors are never stopped; one of them in the worktree keeps it. Without
a hub or replica, no agent is known. `:worktree` runs at once from the screen unless it has
`--execute`.

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
the others: `hub.db`, `replica.db`, `spool.jsonl`, `spool.rejected.jsonl`, `token`,
`hub-tokens` (the hub's scoped tokens), `hub` (the
hub's URL), and the lock files `spool.lock` and `spool.flush`. `AGENT_FLEET_TOKEN_FILE` and `agent-fleet hub --db` move
the token and the hub database elsewhere.

## Not in v1

Decisions (`decision.*`) and any human channel beyond the Inbox answer, data-defined extension types,
the web board, code-host ingest, a NATS transport, `agent-fleet run`/`kill` process ownership, Windows.
These are v2.
