# fleet: notes for agents

- One crate, one binary. `src/` holds the code, `docs/PROTOCOL.md` the spec, `schema/` the
  generated JSON Schema.
- A new event type = a variant in `src/event.rs` (`Known`, `type_name`, `writers`) + a case in
  `State::apply` (`src/state.rs`) + a row in `docs/PROTOCOL.md` + `fleet api schema >
  schema/fleet.schema.json`. The schema test fails until the last step is done.
- The reducer is pure: no I/O, no clock. Test it by folding events.
- Before a commit: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt`.
- This repo is public. No hostnames, user or home paths, tokens, or private project names in
  code, docs, tests or commit messages.
