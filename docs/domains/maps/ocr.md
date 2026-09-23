# Domain: OCR

**Status:** Decided 2026-09-16, edited 2026-09-18, revised 2026-09-21
**Owns:** Turning an image file into text, and nothing about when that should happen.
**Code:** `chronicle-daemon/crates/ocr/src/`

## Boundary

- inside: extracting text from an image on disk — `chronicle-daemon/crates/ocr/src/lib.rs` — `extract_text`
- inside: the vocabulary for an extraction failure — `chronicle-daemon/crates/ocr/src/lib.rs` — `OcrError`
- outside: deciding which images get extracted and when — `chronicle-daemon/src/pipeline.rs` — `ocr_loop`
- outside: moving extraction off the async runtime — `chronicle-daemon/src/pipeline.rs` — `spawn_blocking`
- outside: storing the extracted text — `chronicle-daemon/crates/storage/src/lib.rs` — `update_ocr_text`
- outside: counting what the OCR queue did, which is pipeline's bookkeeping published by IPC — `chronicle-daemon/crates/ipc/src/lib.rs` — `OcrStats`

**Placement test:** Does the file turn an image into text? Deciding when to do that, where the image came from, or what happens to the text belongs to whoever calls it.
**Document comparison:** differs — `docs/use-cases/pipeline.md` treats `ocr_loop` as a pipeline pattern. No document describes the crate's boundary. The distinction between the extraction and its scheduling is drawn here for the first time.

## Owned Files

- chronicle-daemon/crates/ocr/src/lib.rs

## Depends On

- None

## Depended On By

- pipeline — `chronicle-daemon/src/pipeline.rs` — `extract_text`

## Seams

- SEAM-ocr-extraction — pipeline — a blocking synchronous call taking a path and returning text, invoked from an async loop through `spawn_blocking`; this domain decides the signature and the error vocabulary, pipeline decides the scheduling — owner: OCR — `chronicle-daemon/crates/ocr/src/lib.rs` — `extract_text`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/pipeline.md` | Names `ocr_loop` among the downstream async tasks a `CaptureRuntime` bundles, under "Drop Engine Before Joining Tasks" | accurate about the loop, and the loop is outside this domain |
| `docs/use-cases/INDEX.md` | Does not name OCR; it is not one of the six candidates assigned a code location | stale by omission |
| `docs/project-description.md` | Names OCR among the daemon's components | accurate |
| `docs/guides/storage-engine.md` | Mentions OCR in the context of stored text | accurate, and describes the consumer rather than this domain |
| `docs/decisions/` | Two ADRs mention OCR in passing; none decides anything about it | accurate |

## Offshoots Filed

- none found
