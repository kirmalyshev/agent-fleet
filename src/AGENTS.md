# src

`event.rs` envelope + core types + strict validation + `api schema` · `state.rs` the pure reducer ·
`store.rs` the append-only SQLite log · `hub.rs` the hub (sequencer, HTTP, SSE) · `client.rs`
hub client, spool, SSE follower · `host.rs` the host agent (herdr presence, replica, doorbells) ·
`ui.rs` text views · `stats.rs` metrics read off the log · `bench.rs` the hub load test ·
`scan.rs` processes and worktrees on this machine · `quota.rs` each agent program's usage quota and the statusline hook · `spawn.rs` fleet.toml routing, briefs, the host's spawn executor · `doctor.rs` findings read off the state · `stuck.rs` why a stuck agent is stuck, read off its screen, and the once-per-episode actions · `kill.rs` `agent-fleet kill` and the Bash hook · `tui.rs` the operator's screen ·
`main.rs` the CLI. Tests live next to the code they cover.
