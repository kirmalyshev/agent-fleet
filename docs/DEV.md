# `agent-fleet dev`: per-worktree dev environments

Every git worktree of a repository gets a **slot**: its own ports and its own values (a database
name, a cache index). Services run per worktree; shared infrastructure (containers) runs once.
`dev up` in one worktree never touches another worktree's processes or ports. The repository
describes itself in `.agent-fleet/dev.toml`; a repository's `./dev` can be
`exec agent-fleet dev "$@"`.

## Commands

Run them anywhere inside the worktree.

| Command | What |
|---|---|
| `dev up [--<service>…] [--all]` | Start the infra, then the named services (`api` works as well as `--api`). Bare `up` starts the infra only. Refuses a port that something else listens on. Waits for each service's health; on failure prints the last 20 log lines, stops the service and exits non-zero. Prints the URLs. |
| `dev down [--release]` | Stop this worktree's services: TERM to the process group, KILL after 10 s. Never stops the infra. `--release` also frees the slot and deletes the worktree's state and logs. |
| `dev restart [--<service>…]` | Bare: stop and start the recorded set, every service `up` started in this worktree (it survives a crash and `down`). With names: only those. |
| `dev status` | Each service: running or stopped, healthy or not, and `env changed since start` when its rendered env differs from the one it started with. |
| `dev logs [service]` | `tail -F` one service's log, or all of them. |
| `dev env` | The rendered `[env]`, one `KEY=value` per line. |
| `dev url` | Each service's URL. |
| `dev info` | JSON: slot, id, root, main, ports, env, services (running, pid, url, log), the recorded set. |

## `.agent-fleet/dev.toml`

```toml
[slots]
max = 31                     # highest slot; slot 0 is the main checkout's (default 31)
step = 10                    # port = base + step * slot (default 10)
ports = { api = 8990, web = 8991 }

[env]                        # exported to every service and printed by `dev env`
DB_NAME = "app_{id}"
CACHE_DB = "{slot}"

[infra]                      # shared by all worktrees, never stopped by `down`
up = "docker compose up -d postgres redis"
health = ["pg_isready -h 127.0.0.1 -p 5435"]

[services.api]
dir = "server"               # relative to the worktree root (default: the root)
before = "make deps && make migrate"     # run before start, in dir; must exit 0
run = "make serve PORT={port.api}"
health = "http://127.0.0.1:{port.api}/health"   # URL (2xx) or a command (exit 0)
timeout = 120                # seconds to wait for health (default 60)
url = "http://localhost:{port.api}"      # shown by `dev url` and `dev up`

[services.web]
dir = "web"
run = "npm start"
env = { PORT = "{port.web}" }            # on top of [env], for this service only
```

Every value except `max`, `step` and the port bases is a template:

| Placeholder | Value |
|---|---|
| `{slot}` | The worktree's slot. |
| `{id}` | A short stable name: the worktree's directory name, lowercased, other characters `_`. |
| `{port.<name>}` | `ports.<name> + step * slot`. A name not in `ports` is an error. |
| `{root}` | The worktree's absolute path. |

Any other `{…}`, such as a shell's `${HOME}`, stays as it is. Commands run with `sh -c`, in the
service's `dir`, with the process environment plus `[env]` plus the service's `env`. Unknown keys,
a misspelt port and a port past 65535 at slot `max` are errors at load, with the line or the key.

The infra's `up` runs only when one of its `health` checks fails (or it has none); then `dev up`
waits up to 120 s for every check to pass.

A port counts as taken when something accepts a connection on `127.0.0.1:<port>` or it cannot be
bound. The ports a service's own templates name are checked before it starts, except the ones a
running service of the same worktree also names.

## Slots

- The main checkout is slot 0. A linked worktree claims the lowest free slot ≥ 1 on first use
  and keeps it while it exists; `dev down --release` frees it.
- Each user's claims are in `<data dir>/dev/slots.json` (worktree path → slot, for every
  repository), changed under an exclusive `flock`.
- Users on one machine share ports, so each user also publishes the slots they hold to
  `/var/tmp/agent-fleet-dev/<uid>.json` (temp file + rename, under the directory's `.lock`). A new
  slot must be free in every user's file. Nobody writes another user's file. The directory must be
  owned by root or by you, and sticky when others may write it; `dev` creates it `1777` when it is
  missing. For several users, create it once as root: `sudo install -d -m 1777
  /var/tmp/agent-fleet-dev`. `AGENT_FLEET_DEV_SHARED` names another directory.
- A worktree's state is in `<data dir>/dev/<slot>-<id>/`: `state.json` (the recorded set, and
  each running service's process group and env) and `<service>.log`. Nothing is written inside
  the worktree.

`<data dir>` is `AGENT_FLEET_HOME`, else the default instance's data directory.
