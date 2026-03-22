# ADR-006: External Content FTS5 for Full-Text Search

**Status:** Accepted
**Date:** 2026-03-21

## Context

Chronicle needs full-text search across OCR text (from screenshots) and
transcripts (from audio segments). SQLite FTS5 offers three modes: standalone
(duplicates text), external content (references main tables), and contentless
(index only, no text retrieval).

## Decision

Use external content FTS5 tables that reference the main `screenshots` and
`audio_segments` tables. Sync maintained via INSERT/UPDATE/DELETE triggers.

## Consequences

- No data duplication — text lives in one place (main tables), FTS indexes
  reference it.
- Triggers add a small amount of schema boilerplate but are a well-established
  SQLite pattern.
- FTS5 snippet and highlight functions work normally since the content is
  accessible via the main tables.
- If triggers are ever dropped or bypassed, search indexes go stale. All schema
  changes must preserve the trigger chain.