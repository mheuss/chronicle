# Domain: storage

**Status:** Decided 2026-09-17, revised 2026-09-21
**Owns:** Persisting captures and querying them back — the SQLite schema, the rows, the FTS5 indexes, and the media tree on disk.
**Code:** `chronicle-daemon/crates/storage/src/`

## Boundary

- inside: the single handle every caller gets, and the async-over-`spawn_blocking` discipline behind it — `chronicle-daemon/crates/storage/src/lib.rs` — `Storage`
- inside: the schema and the ordered migration runner that applies it — `chronicle-daemon/crates/storage/src/schema.rs` — `migrate`
- inside: the public data types every caller speaks in — `chronicle-daemon/crates/storage/src/models.rs` — `ScreenshotMetadata`
- inside: the media tree under the base dir, and the only path that deletes from it — `chronicle-daemon/crates/storage/src/media.rs` — `MediaManager`
- inside: where a captured file lands, bucketed by date — `chronicle-daemon/crates/storage/src/files.rs` — `screenshot_path`
- inside: row access for each capture kind — `chronicle-daemon/crates/storage/src/screenshots.rs` — `update_ocr_text`
- inside: deleting rows past the cutoff, in batches, interruptibly — `chronicle-daemon/crates/storage/src/retention.rs` — `run_cleanup_interruptible`
- inside: the error vocabulary every caller matches on — `chronicle-daemon/crates/storage/src/error.rs` — `StorageError`
- inside: absorbed from the `search` candidate on 2026-09-17 — one unified FTS5 query across both index tables, with user-input sanitization and snippet extraction — `chronicle-daemon/crates/storage/src/search.rs` — `sanitize_fts5_query`
- outside: rendering results and turning FTS5's snippet markup into styled text — `chronicle-ui/Sources/ChronicleUI/SnippetAttributedString.swift` — `snippetAttributedString`
- outside: deciding when cleanup runs and whether it is due after a restart — `chronicle-daemon/src/retention_task.rs` — `run_cleanup_loop`
- outside: what a configured retention value means — `chronicle-daemon/crates/ipc/src/retention.rs` — `Retention`
- outside: the status shape that crosses the wire — `chronicle-daemon/crates/ipc/src/lib.rs` — `StorageStats`

**Placement test:** Does the file put a capture on disk, get it back, or define the shape it is stored in? Deciding *when* to do that, and showing the user what came back, belong elsewhere.
**Document comparison:** deliberately matches — `docs/guides/storage-engine.md:71` already draws this line. It states that the inner modules are `pub(crate)` and "callers only interact through the `Storage` struct". Measured 2026-09-17 and re-measured 2026-09-21: all seven importers reach the crate through `Storage` or a `models` type. None names a `pub(crate)` module.

## Owned Files

- chronicle-daemon/crates/storage/src/audio.rs
- chronicle-daemon/crates/storage/src/error.rs
- chronicle-daemon/crates/storage/src/files.rs
- chronicle-daemon/crates/storage/src/lib.rs
- chronicle-daemon/crates/storage/src/media.rs
- chronicle-daemon/crates/storage/src/migrations/001_initial_schema.sql
- chronicle-daemon/crates/storage/src/migrations/002_path_indexes.sql
- chronicle-daemon/crates/storage/src/migrations/003_null_marker_transcripts.sql
- chronicle-daemon/crates/storage/src/models.rs
- chronicle-daemon/crates/storage/src/retention.rs
- chronicle-daemon/crates/storage/src/schema.rs
- chronicle-daemon/crates/storage/src/screenshots.rs
- chronicle-daemon/crates/storage/src/search.rs

## Depends On

- IPC — `chronicle-daemon/crates/storage/src/lib.rs` — `Retention`

## Depended On By

- pipeline — `chronicle-daemon/src/pipeline.rs` — `AudioSegmentMetadata`
- pipeline — `chronicle-daemon/src/main.rs` — `StorageConfig`
- capture — `chronicle-daemon/src/capture_runtime.rs` — `Storage`
- background-work — `chronicle-daemon/src/retention_task.rs` — `CleanupStats`
- IPC — `chronicle-daemon/src/ipc_handler.rs` — `Storage`

## Seams

- SEAM-cleanup-schedule-key — background-work — the `last_cleanup_ms` row in this domain's `config` table, written and read only by the scheduler, which decides the key's name and its units; this domain owns the table the row sits in — owner: background-work — `chronicle-daemon/crates/storage/src/lib.rs` — `set_config`
- SEAM-storage-status-wire — IPC — `models.rs` declares `StorageStatus` and `crates/ipc/src/lib.rs` declares `StorageStats` independently, neither importing the other, and the daemon converts between them at `src/ipc_handler.rs`; this domain decides what a status can report — owner: storage — `chronicle-daemon/crates/storage/src/models.rs` — `StorageStatus`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/guides/storage-engine.md:71` | "The inner modules (`screenshots`, `audio`, `search`, `retention`, `files`, `schema`) are all `pub(crate)` — callers only interact through the `Storage` struct" | accurate — verified against `lib.rs:14-24`, and against all seven importers |
| `docs/guides/storage-engine.md:70-72` | Its `migrations/` listing shows `001_initial_schema.sql` and `002_path_indexes.sql` | stale — `003_null_marker_transcripts.sql` exists, is 123 lines, and is embedded at `schema.rs:8` alongside the other two. Added by CHR-38 |
| `docs/guides/storage-engine.md:139-140` | "Retention cleanup and orphan sweeping are two independent operations in `retention.rs`, with different callers. They are not two phases of one pass." | accurate |
| `docs/use-cases/storage.md` | Eight solutions, located in `migrations/001`, `schema.rs`, `media.rs`, `search.rs`, and `lib.rs` | accurate — including its `search.rs` entry, which the sitting-10 merge confirmed belongs here |
| `docs/use-cases/INDEX.md:9` | Code Location `chronicle-daemon/crates/storage/` | accurate — the whole crate, which after the sitting-10 merge is what this domain owns. The column is superseded regardless; see CHR-151 |
| `docs/use-cases/INDEX.md` | Has **no** `search` row; its six domains are `pipeline`, `background-work`, `storage`, `audio-encoding`, `transcription`, `ipc-compat` | accurate as an absence, and evidence for the merge. Control: the file's only match for "search\|fts5\|snippet" is the literal `FTS5` at `:9`, so the search would have matched had a row existed |
| `docs/guides/storage-engine.md:63, 79-111` | Lists `search.rs` in the storage module table as "Unified FTS5 search across screenshots and audio", and documents "How search works" as a section of the storage guide | accurate — no document anywhere treats search as a domain of its own |
| `docs/decisions/015-shared-value-types-live-in-the-ipc-crate.md` | This crate depends on `chronicle-ipc` so `Retention` can be shared, and the backwards arrow is deliberate | accurate — `crates/storage/Cargo.toml` carries `chronicle-ipc`, and it is this domain's only workspace dependency |
| `docs/decisions/011-chronicle-ui-sandbox-approach.md:26-27` | The daemon writes the database, screenshots and settings under `~/Library/Application Support/Chronicle/`, and sandboxing moves all of it | accurate — `MediaManager` is what moves |
| `.claude/audit/security.md:18-19` | Capture history is persisted unencrypted and FTS5 duplicates the sensitive text in searchable form | accurate — still true, and untracked. CHR-90 closed 2026-04-06 as an audit-sweep umbrella without addressing it. Re-filed as CHR-156 |

Counted 2026-09-17 and re-counted 2026-09-21: `crates/storage/src/` carries zero `design §` citations. So CHR-36
does not reach this domain. Control: the same pattern returns nine hits in
`src/provisioning.rs`.

## Offshoots Filed

- CHR-156 — capture history is unencrypted at rest and the CHR-90 that was said to track it is closed
