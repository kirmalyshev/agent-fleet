# src

`event.rs` envelope + core types + strict validation + `api schema` · `state.rs` the pure reducer ·
`store.rs` the append-only SQLite log · `hub.rs` the hub (sequencer, HTTP, SSE) · `client.rs`
hub client, spool, SSE follower · `host.rs` the host agent (herdr presence, replica, doorbells) ·
`ui.rs` text views · `main.rs` the CLI. Tests live next to the code they cover.
