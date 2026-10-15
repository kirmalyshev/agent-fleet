# fleet

A communication protocol and tooling for fleets of AI coding agents spread over several
machines, projects and human operators.

- Every fact is a typed event in an append-only log; state is a fold over the events; every UI
  is a function of the state.
- Messages to agents are doorbells; the events are the truth.
- Human decisions enter only through human-only channels.

Status: v1 in progress. Linux and macOS. MIT licensed.
