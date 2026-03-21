# ADR-004: JSON Over Unix Socket for IPC

**Status:** Accepted
**Date:** 2026-03-20

## Context

The Rust daemon and Swift UI need to communicate. The daemon serves search
results, screenshots, audio, and status information to the UI.

## Decision

Newline-delimited JSON over a Unix domain socket at
`~/Library/Application Support/Rewind/rewind.sock`.

We considered gRPC with protobuf but decided against it.

## Consequences

**Good:**
- Simple to implement in both Rust and Swift.
- Easy to debug — you can talk to the daemon with `socat` or `nc`.
- No protobuf compilation step, no generated code, no additional dependencies.
- Sufficient performance for local IPC with a single client.

**Bad:**
- No schema enforcement — the message format is a convention, not a contract.
  (Mitigated by tests and shared type definitions.)
- If we ever needed multiple concurrent clients or high-throughput streaming,
  we'd need to revisit. (Unlikely for this use case.)
