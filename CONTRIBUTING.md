# Contributing

Issues and pull requests are welcome. For a large change, open an issue first so we can agree on
the shape before you write it.

## Set up

You need stable Rust. To run the fleet itself you also need the tools in the README's
[Prerequisites](README.md#prerequisites); the tests do not need herdr, `gh` or a running hub.

```sh
git clone https://github.com/kirmalyshev/agent-fleet
cd agent-fleet
cargo build
```

## Build, test, lint

Run all three before every commit. CI runs the same on Linux and macOS.

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Try a change against a throwaway fleet, never against one you depend on: a temporary
`AGENT_FLEET_HOME`, an `--instance` of its own and a free port, as in the README's quick start.

## Code map

One crate, one binary. `docs/PROTOCOL.md` is the spec, `schema/` the generated JSON Schema.

| File | What |
|---|---|
| `src/main.rs` | The CLI. |
| `src/event.rs` | The envelope, the event types, strict validation, `api schema`. |
| `src/state.rs` | The pure reducer: `state = fold(events)`. |
| `src/store.rs` | The append-only SQLite log. |
| `src/hub.rs` | The hub: sequencer, HTTP, SSE, tokens, following a replaced binary. |
| `src/client.rs` | Hub client, spool, SSE follower, data directories. |
| `src/host.rs` | The host agent: herdr presence, replica, doorbells. |
| `src/scan.rs` | Processes and worktrees on this machine. |
| `src/watch.rs` | The watch tick: classifies agent-started processes as working, waiting, idle, hung or orphan. |
| `src/stuck.rs` | Why a stuck agent is stuck, read off its screen, and the once-per-episode actions. |
| `src/signal.rs` | `hook signal`: harness hook events become `agent.signal`. |
| `src/quota.rs` | Each agent program's usage quota and `hook statusline`. |
| `src/spawn.rs` | `fleet.toml` routing, briefs, the host's spawn executor. |
| `src/kill.rs` | `agent-fleet kill` and `hook bash`. |
| `src/doctor.rs` | Findings read off the state. |
| `src/orphanpr.rs` | Open PRs no live agent owns, read off `gh`. |
| `src/update.rs` | `self-update`: the host pulls its clone, rebuilds and restarts. |
| `src/ui.rs`, `src/board.html` | Text views and the browser board. |
| `src/tui.rs` | The operator's screen. |
| `src/stats.rs`, `src/bench.rs` | Metrics read off the log, and the hub load test. |
| `build.rs` | Embeds the commit the binary is built from. |
| `tests/` | Tests that run the built binary. |

## Conventions

- **A new event type** is a variant in `src/event.rs` (`Known`, `type_name`, `writers`), a case
  in `State::apply` (`src/state.rs`), a row in `docs/PROTOCOL.md`, and a regenerated schema:
  `cargo run -q -- api schema > schema/agent-fleet.schema.json`. The schema test fails until the
  last step is done.
- **The reducer is pure**: no I/O, no clock. Test it by folding events.
- **Tests live next to the code** they cover, in a `#[cfg(test)]` module. Tests that need the
  built binary go in `tests/`. Use temp directories, never a real data or config directory.
- **Docs change in the same commit as the code.** `docs/PROTOCOL.md` describes what the code
  does; a behaviour change without its PROTOCOL.md change is not done. If a command's `--help`
  changes, update the README's command table from `agent-fleet --help`.
- **This repo is public.** No hostnames, user names, home paths, tokens, or private project names
  in code, docs, tests, commit messages or PR text. Use `owner/repo`, `/path/to/checkout`,
  `laptop`, `/Users/you`.

## Pull requests

Branch from `main` and open the PR against `main`. Keep it to one change. To catch up with
`main`, merge it into your branch. The PR body states:

- **What changed and why**, and the issue it closes.
- **Checks run**, with their results (`cargo fmt --check`, `clippy`, `cargo test`, any manual
  run).
- **What you did not verify**, for example macOS when you tested on Linux, or a multi-machine
  setup.

## CI

`.github/workflows/rust.yml` runs on every push to `main` and every PR against it. On Linux it
checks formatting, runs clippy with warnings as errors, builds and tests; on macOS it runs
clippy, builds and tests. CI must be green before a PR merges.
