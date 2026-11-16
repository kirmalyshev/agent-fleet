# Security

## Report a vulnerability

Use GitHub's private vulnerability reporting for this repository: the **Security** tab, then
**Report a vulnerability**. Do not open a public issue for a vulnerability.

## Trust model

agent-fleet coordinates coding agents that run with your user's rights. It keeps honest agents
from making mistakes, such as killing each other's processes, and it keeps other machines from
posting as your host. It does not contain an agent that sets out to attack you: an agent can
already do everything your Unix user can do.

[docs/PROTOCOL.md](docs/PROTOCOL.md) has the exact rules. In short:

**Tokens.** Every hub endpoint needs `Authorization: Bearer <token>`. On first start the hub
writes a random shared token file (mode 0600). The shared token may post as any actor, except as
a host that has a token of its own. `agent-fleet mint-token` makes scoped tokens:

- `host:<name>` may post only as that host and its agents. Give it to that host with
  `agent-fleet host --token-file <file>`. Other machines can then no longer post as that host.
- `agent:<name>` may post only as the agents on that host. Make it the host's default `token`
  file, and its agents lose the shared token.

No actor, whatever its token, may post as a human or as the hub. The only way in for a human's
answer is the board, behind the shared token.

**Agents can read their host's token.** The host agent runs as the same Unix user as its agents,
so an agent can read every token file its host reads and post as that host. The host cannot run
as a separate user: herdr's socket, `/proc` and `lsof` of the agents' processes, the agents'
git checkouts and quota files all need the agents' user, and root would write root-owned files
into their repositories. See
[Who can post as a host](docs/PROTOCOL.md#who-can-post-as-a-host).

Keep the hub out of the agents' reach: run it on a machine, or as a user, that no agent runs as,
from a binary and a data directory agents cannot write. The hub's data directory holds the shared
token and every scoped token, and the hub restarts into a replaced binary, so a binary an agent
can replace runs the agent's code as the hub.

**`agent-fleet kill`** refuses, and kills nothing, when a pid is herdr, a pane's shell, an agent,
an agent-fleet daemon, an ancestor of any of these, the caller's own ancestor, or inside another
agent's process tree. It kills the caller's own processes and orphans `doctor` proved. A
`proc.hung` report is a claim anyone with the host's token can post, so `kill` checks each hung
pid against the live process itself (two samples 5 s apart and the watch's `lsof` reads) and
refuses unless the process is idle by the watch's rules. Every refusal is logged as
`kill.refused`. `hook bash` blocks raw `kill`, `pkill` and `killall` in agents; it guards
against slips, not against an agent set on evading it. See
[Killing processes](docs/PROTOCOL.md#killing-processes).

**Network.** The hub binds the addresses you give it (default `127.0.0.1:7710`) and refuses
`0.0.0.0` and `::`. It speaks plain HTTP: there is no TLS, so the token and every event cross
the network in clear text. Bind it to a non-loopback address only on a network you trust.

**`self-update`.** Anyone with the shared token can write `update.requested`, which makes every
host pull `main`, rebuild and restart. Treat the token as the right to restart the fleet, and
`main` of your clone as code every host will run.
