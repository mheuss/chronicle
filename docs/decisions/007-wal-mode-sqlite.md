# ADR-007: WAL Mode for SQLite

**Status:** Accepted
**Date:** 2026-03-21

## Context

Chronicle's daemon writes screenshots every 2-10 seconds while the UI may be
running search queries simultaneously. Default SQLite journal mode locks the
entire database during writes, which would cause UI search to stall.

## Decision

Enable WAL (Write-Ahead Logging) mode with `synchronous = NORMAL`. Set on
connection open via PRAGMAs.

## Consequences

- Concurrent readers are never blocked by writes. The UI can search while
  capture inserts are happening.
- `synchronous = NORMAL` is safe with WAL and avoids the performance cost of
  `FULL`. SQLite documentation explicitly recommends this pairing.
- WAL mode creates two additional files alongside the database
  (`chronicle.db-wal`, `chronicle.db-shm`). These are managed automatically by
  SQLite.
- WAL mode is persistent once set — it survives database close and reopen.