# src

`event.rs` envelope + core types + strict validation + `api schema` · `state.rs` the pure reducer ·
`store.rs` the append-only SQLite log · `hub.rs` the hub (sequencer, HTTP, SSE) · `client.rs`
hub client, spool, SSE follower · `host.rs` the host agent (herdr presence, replica, doorbells) ·
`ui.rs` text views · `stats.rs` metrics read off the log · `bench.rs` the hub load test ·
`scan.rs` processes and worktrees on this machine · `quota.rs` each agent program's usage quota and the statusline hook · `spawn.rs` fleet.toml routing, briefs, the host's spawn executor · `doctor.rs` findings read off the state · `stuck.rs` why a stuck agent is stuck, read off its screen, and the once-per-episode actions · `kill.rs` `agent-fleet kill` and the Bash hook · `watch.rs` the host watch tick: classifies agent-started processes as working, waiting, idle, hung or orphan, and reports them · `update.rs` self-update: pull, build and atomically swap the binary at `origin/main`, then restart through the service manager · `tui.rs` the operator's screen ·
`main.rs` the CLI. Tests live next to the code they cover.
