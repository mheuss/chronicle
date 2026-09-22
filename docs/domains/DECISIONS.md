# Decisions

Decisions made while executing the CHR-150 walk, recorded as they happen.

**How to use this file.** It is chronological and flat, oldest first. Entries are
dated and bolded, so `grep -n '^- \*\*\[' DECISIONS.md` gives a table of contents.
Three kinds are worth knowing about before you start:

- **Why a boundary sits where it does.** The arguments a future dispute needs —
  `audio-encoding` against `capture` for the encoder, `retention` across three
  crates, `search` into `storage`, `ipc_handler.rs` into IPC. If you disagree with
  a line in a domain file, the reasoning is here.
- **Corrections.** Several entries revise an earlier one in this same file and
  say so. The revision is the useful half. The original is kept so the mistake
  stays legible rather than vanishing. A **Superseded** marker means the text
  above it no longer describes the tree.
- **What is still owed.** Plan and design defects found during execution and
  never fixed. `REGISTER.md`'s `## Close-out` → Still open is the short list.
  The detail is here.

The map itself is `SYNTHESIS.md`. This file is why it looks the way it does.

This file lives in `docs/domains/` rather than at the bottom of the plan,
because the plan is in `docs/plans/`, which is gitignored. Task 1 Step 5 routes
session handoff notes there for the same reason. Decisions are the same
category.

**Superseded 2026-09-22.** Entries below refer to `check7_baseline.sh` and
`BASELINE.txt`, a tripwire that failed on any commit under `docs/`. It existed to
keep the walk from editing documents it was only meant to read. Both were deleted
when the map was committed, along with `RECIPE.md`, `TEMPLATE.md`,
`SYNTHESIS_TEMPLATE.md` and `INVENTORY.md`. Entries naming them are a record of
what was true during the walk, not of the tree today.

- **[2026-09-16] No compaction-recovery marker.** `/sop:execute-plan` writes
  `.claude/active-plan/<session>.txt` so a compacted session can find its plan.
  Not written here, for one reason: the design's Out of Scope forbids any change
  to `.claude/`. NFR-2 already makes `REGISTER.md` the resume marker — "an
  interrupted walk resumes from the register alone". So the harness file is
  redundant with a mechanism the design specifies anyway. A session recovering
  from compaction reads `REGISTER.md` and the plan at
  `docs/plans/2026-09-16-chr-150-domain-map-implementation.md`.

  Corrected 2026-09-16: this decision was first recorded with a second reason,
  that check 7 would catch the marker as an untracked file. That was wrong.
  `.claude/active-plan/` is gitignored at `.gitignore:38`. So a file written
  there is invisible to `git status --porcelain`, and check 7 never sees it. The
  Out of Scope prohibition is the whole of the case, not half of it.

- **[2026-09-16] The plan's baseline snippets were corrected and the baseline
  re-taken.** A code review found two defects in `check7_baseline.sh`, both
  verified. `find` does not descend into a symlinked directory, and `-type f`
  does not match the symlink itself. So a symlinked protected root contributes
  zero lines to the manifest, and a changed tree reports clean. This project's
  worktree recipe symlinks exactly those directories. Separately, `( find ...
  ) | sort > "$tmp" || exit 1` tests `sort`'s exit status, not `find`'s. So a
  `find` that errored partway would bake a short baseline that check 7 agrees
  with forever.

  The script was hardened first. The plan's Task 1 Step 2 and Task 2 Step 2
  snippets carried the same defects. So they were corrected too, and the
  baseline re-taken against the amended plan. Still 225 files. The plan's own
  hash is what changed.

  This is the one re-take this walk gets. It is not the escape hatch that was
  removed earlier. No sitting has run, so there is no walk state for a re-take
  to hide. The rule from here is unchanged: a protected-tree change is a BR-5
  failure to report, not to re-baseline away.

  Verified after the re-take: check 7 green at 225 lines with no symlinks. A
  probe file under `docs/plans/` flips it to 1 and back to 0. The plan's hash in
  the manifest matches the file on disk.

- **[2026-09-16] BR-5's enforcement is narrower than "check 7 catches it".** The
  baseline roots `docs` and `.claude/audit` only. All 22 files under `.claude/`
  are gitignored, and only `audit/` is scanned. So the design's Out of Scope
  item "any change to `.claude/`" has no automated enforcement outside
  `.claude/audit/`. This follows the design's own §3.2 baseline command, so it
  is not a defect. But the plan's phrase "the only enforcement of BR-5" reads
  broader than what runs.

- **[2026-09-16] Five defects belong to the plan, not the map, and are deferred
  to one amendment.** The plan lives in `docs/plans/`, inside the protected set.
  So editing it fails check 7 and needs a baseline re-take. Batching them:

  1. **Task 23 Step 1** says a blocked row becomes walkable when its target is
     `confirmed`. Design §1.4 says *decided*. `RECIPE.md` and `REGISTER.md` both
     follow the design. Under the plan's stricter rule, a row blocked on a
     candidate that ends up `dropped` is never eligible. Task 23 Step 3 declares
     a deadlock. The plan is the wrong document here.
  2. **Task 26** does not mention `## Exclusions pending`. So a path parked
     there with the reason `pending <candidate>` is never reassigned. And Task
     28 copies that non-answer into the finished synthesis, where check 8
     accepts it. Either Task 26 gains a reassignment step, or a drop's reason
     must be final.
  3. **Task 28 Step 2** derives the synthesis's exclusion table from
     `INVENTORY.txt` minus owned paths. It never reads `## Exclusions pending`.
     It can recover the paths but not the reasons, and it requires them
     non-empty.
  4. **Two sitting notes say "Task 25 revisits it"** — Task 9 at plan line 1017
     and Task 17 at line 1149. Task 25 writes the check scripts. The revision
     pass is Task 26. Found as one instance on 2026-09-16 and as a second while
     walking sitting 9. Grep the amended plan for the phrase rather than fixing
     the two now known.
  5. **Task 26 Step 3 has no split path.** It says a revision may conclude a
     confirmed domain should be merged or dropped. It gives the register steps
     for both. A revision may equally conclude a confirmed domain should split.
     And §1 says the register is authoritative for the candidate list, while its
     starting length of fourteen is not. The only way to append a row is a
     `split` outcome. So without this step, the revision pass cannot act on the
     one conclusion that grows the walk.

  A sixth item is a Deltas-table omission rather than a defect: the Blocked rows
  table carries five columns where design §1.4 gives four. The `Offshoots`
  column is necessary. A blocked sitting's IDs have nowhere else to live, and
  check 6 compares ID sets. So the design is behind, and the plan's Deltas table
  should record it.

- **[2026-09-16] `grep` is not silent on a missing root, and the recipe said it
  was.** A code review caught it and measurement confirmed. `grep -rn ...
  crates/capture/src/` from the repo root prints `No such file or directory` and
  exits 2. What actually loses the signal is that the error is one stderr line
  in a long transcript. And a `for` loop over several roots reports only its
  last iteration's status. The recipe asserted the wrong mechanism for the exact
  failure class it exists to prevent. Corrected.

- **[2026-09-17] `capture` keeps `encoder.rs` on the placement test, not on
  symmetry with `audio`.** Sitting 1 raised that no `capture-encoding` candidate
  exists while `audio-encoding` did, and left it open. The handoff then reasoned
  that the merge at sitting 5 shrank the asymmetry. That reasoning does not hold,
  because the two cases were never alike:

  | | audio encoder + accumulator | capture encoder |
  |---|---|---|
  | Visibility | `mod encoder;` — private | `pub mod encoder;` plus `pub use encoder::encode_heif` |
  | Caller inside the crate | `accumulator.rs` uses `OggOpusEncoder` | none |
  | Caller outside | — | `src/pipeline.rs:76`, and `crates/capture/tests/integration.rs` |
  | In-crate deps | `crate::{AudioError, Result}`; accumulator also `AudioSource`, `CompletedSegment`, `segment_path` | `crate::error`, `crate::pixel_buffer` |

  `audio-encoding` merged because carving it out takes a cycle-shaped piece from
  the middle of a crate. `crates/capture/src/encoder.rs` is the opposite shape:
  a leaf at the edge. Nothing inside `chronicle-capture` calls it, and its one
  production caller is in another crate. So the merge argument is not available
  here. The question has to be answered on its own terms.

  It stays with `capture` for two reasons. `pixel_buffer.rs` is shared.
  `handler.rs` reads width and height from it, and `encoder.rs` builds the
  CGImage through it. So a separate domain leaves one of the two reaching across
  the line, and buys a seam whichever way it is drawn. And `maps/capture.md`
  already decides this deliberately. Its placement test asks whether the file
  turns a captured buffer into a frame, and `encode_heif` does. The external
  consumer is already recorded under `Depended On By`. Nothing is hidden by the
  current boundary.

  The practical case points the same way. The walk exists so the Linear project
  structure decision has a map. And a 331-line leaf module with one production
  caller is not a Linear project.

  This closes the sitting 1 open question. Task 26 revisits it only if a later
  sitting gives `encoder.rs` a second consumer.

- **[2026-09-17] `src/settings.rs` added to row 11's Roots, because no row
  reached it.** Every path in `INVENTORY.txt` was checked against every register
  row's Roots during sitting 7. Of those, 63 are reachable and
  `chronicle-daemon/src/settings.rs` was not. Roots are what a sitting greps
  from. So no sitting would have pulled it up for a decision. It would have
  arrived at close as an inventoried file no domain claims. Task 28 then reports
  that as an exclusion with no reason, since nobody ever discussed it. That is
  plan defect 3 arriving early with a concrete instance.

  It went to row 11 rather than to row 7. It is 392 lines holding three keys for
  three domains — `mic_enabled`, `capture_paused`, `whisper_model`. So it is not
  transcription's, and sitting 7 recorded it as an outside bullet. Row 11
  already carries `src/main.rs`, which is what reads these settings at boot
  (`main.rs:361`, `:397`). So `pipeline` is the sitting that will already have
  the calling code in front of it. The Symbols cell gained `settings` at the
  same time. Roots without Symbols is a row the dependency greps cannot walk.

  The file is not dead code. Six non-test call sites: `main.rs:158`, `:361`,
  `:397`, `:755`, and `capture_supervisor.rs:258`, `:290`. The gap was in the
  register, not the codebase.

  This is a Roots edit to an undecided row, which the walk permits. No outcome,
  no domain file, and no decided row was rewritten.

- **[2026-09-17] `background-work` is the schedule, not the retention stack, and
  row 9's exclusion was retired to pay for it.** Row 8 arrived with three roots
  that share the word `retention` and little else:

  | Root | Lines | Widest visibility | Where its public surface lives |
  |---|---|---|---|
  | `src/retention_task.rs` | 1067 | `pub(crate)` | itself |
  | `crates/storage/src/retention.rs` | 1932 | `pub(crate)` | `storage/src/lib.rs`, row 9 |
  | `crates/ipc/src/retention.rs` | 240 | `pub` | `ipc/src/lib.rs`, rows 12 and 13 |

  `retention_task.rs` is scheduling and nothing else — deadline arithmetic, a
  `CleanupOps` trait, and a loop. The trait is the code stating the point: the
  scheduler does not want to know what it is cleaning. The other two are the
  deletion work and the policy type. Each is a private module reachable only
  through a `lib.rs` that belongs to another candidate. Taking them would give
  this domain two cycle-shaped pieces from the middle of two crates. That is the
  cut that merged `audio-encoding` at sitting 5.

  Every existing document already splits them this way.
  `docs/guides/storage-engine.md:71` names `retention` an inner module of
  storage beside `screenshots`, `audio` and `search`.
  `docs/use-cases/background-work.md:107` locates its surviving-restarts pattern
  at `retention_task.rs:run_cleanup_loop` and nowhere else. ADR-015 explains why
  the `Retention` type sits in `chronicle-ipc`.

  **The paired edit was mandatory, not tidying.** Row 9's Roots read
  `crates/storage/src/ except search.rs and retention.rs`. The exclusion existed
  only to hold the file back for this sitting. Checked before deciding:
  `crates/storage/src/retention.rs` was reachable from row 8 and row 9 alone,
  and row 9's clause cancelled it. So declining it at sitting 8 without amending
  row 9 would have left it reachable by nothing. That would recreate the
  `settings.rs` gap one file over. The clause is now `except search.rs`.
  `retention (scope hits to crates/storage/src)` was added to row 9's Symbols in
  the same edit, because Roots without a matching symbol is a row the dependency
  greps cannot walk. `crates/ipc/src/retention.rs` needed no such edit — row 13
  already names it.

  **A blocked row was available and was refused.** Task 16's notes offer one,
  since a merge into `storage` or `IPC` is unavailable while neither is
  confirmed. But blocking records that this domain wanted to merge and could not
  yet. That is not what happened: the two files were never this domain's, only
  its grep's. Blocking would also have parked row 8 unfinished into Task 23's
  walkability rule, which is defect 1 in the pending plan amendment.

- **[2026-09-17] The `pipeline.rs` disagreement was surfaced, not settled.**
  Design §3.1's manifest assigns `chronicle-daemon/src/pipeline.rs` to
  `pipeline`. `docs/use-cases/INDEX.md:8` assigns it to `background-work`. The
  recipe requires whichever of rows 8 and 11 runs first to surface it rather
  than silently take a side. Sitting 8 ran first and surfaced it in
  `maps/background-work.md`.

  No edit followed. The register already implements the manifest's answer.
  `src/pipeline.rs` is reachable from row 11's Roots and from no other row. So
  taking the index's side would be a register change, and sitting 11 is where
  that belongs. The index's Code Location column is superseded wholesale by
  CHR-151 in any case.

- **[2026-09-17] `storage` confirmed at twelve files, and the crate's own
  visibility drew the boundary.** Three modules are `pub` — `error`, `media`,
  `models` — and six are `pub(crate)`. Measured against every importer: seven
  daemon files use the crate, each through `Storage` or a `models` type. Not one
  names a `pub(crate)` module. No other crate depends on it at all.
  `chronicle-daemon/Cargo.toml:28` is the only manifest naming it.
  `docs/guides/storage-engine.md:71` had already written this rule down. So the
  Document comparison is `deliberately matches` — the second in the walk.

  The three `.sql` migrations are owned here. `schema.rs:5-8` embeds all three
  with `include_str!`. So they compile into the binary and are crate source, not
  runtime assets. `retention.rs` is owned here too, following sitting 8 rather
  than re-deciding it.

  Two seams. `SEAM-cleanup-schedule-key` gained its second endpoint, owner
  unchanged at `background-work`. `SEAM-storage-status-wire` is new. `models.rs`
  declares `StorageStatus` and `crates/ipc/src/lib.rs` declares `StorageStats`.
  Neither imports the other, and `src/ipc_handler.rs` converts. Owner `storage`,
  because this domain decides what a status can report. Its IPC endpoint lands
  at sitting 12.

- **[2026-09-17] Sitting 9 constrains sitting 10, and that was surfaced before
  deciding.** `search.rs` is `pub(crate) mod search;` at `lib.rs:24`. Its public
  entry point is `Storage::search`, defined in `lib.rs` — a file `storage` now
  owns. That is the same shape sitting 8 refused for `retention.rs`: a private
  module whose entire public surface lives in another domain's file. Applied
  consistently, row 10's Rust half is a `merged into storage`, which is
  available now that `storage` is confirmed.

  It was raised before this sitting closed rather than after. So the boundary
  could have been worded to leave `search` room. The maintainer confirmed twelve
  files knowing that. Sitting 10 still decides its own outcome, and two facts
  point the other way. Row 10's Roots carry three Swift files that `retention`
  had no equivalent of. And `docs/use-cases/INDEX.md` lists `search` as a
  use-case domain of its own. A merge would need those three Swift files to land
  somewhere.

- **[2026-09-17] `search` merged into `storage`, and one of my two reasons for
  hesitating was false.** At sitting 9 I told the maintainer that two facts cut
  against a merge. Row 10 carries three Swift files, and
  `docs/use-cases/INDEX.md` lists `search` as a use-case domain of its own.
  **The second is wrong.** The index has six domains — `pipeline`,
  `background-work`, `storage`, `audio-encoding`, `transcription`, `ipc-compat`
  — and no `search` row. Its only match for "search\|fts5\|snippet" is the
  literal `FTS5` inside the `storage` row's description. Checked properly at
  sitting 10, before deciding.

  Corrected, the documents all point one way.
  `docs/use-cases/storage.md:187-208` files FTS5 query sanitization as a
  `storage` solution located in `search.rs`.
  `docs/guides/storage-engine.md:79-111` puts "How search works" inside the
  storage guide. `:63` lists `search.rs` in the storage module table. `:71`
  names `search` among the `pub(crate)` modules reached only through `Storage`.
  No document treats it as a domain.

  The code agrees. `search.rs` is 443 lines of which 174 are production, and
  `#[cfg(test)]` begins at `:175`. It holds one `pub(crate) fn` and two private
  helpers, with its public entry point `Storage::search` in `lib.rs`. Exactly
  one caller outside the crate, `src/ipc_handler.rs:394`, and it passes
  `SearchFilter::ScreenOnly`. So the module's audio branch is unreachable in
  production, and exercised only by its own tests. `SearchHitSource` has one
  variant.

  The three Swift files go to `UI` at sitting 14, recorded as an outside bullet.
  `SearchPopoverView.swift` is not a search view. It is 429 lines holding
  `PausedBanner` (`:304`), `TranscriptionBanner` (`:329`), `provisionModel`
  (`:161`), `resumeCapture` (`:295`) and an
  `@Environment(TranscriptionAlertState)` (`:7`). Search is one method,
  `runSearch` (`:257`). Claiming it would give this domain a file that is mostly
  two other domains' UI. Sittings 2 and 7 both pushed the Swift side out for the
  same reason.

- **[2026-09-17] Row 9's Roots keep `except search.rs`, and I had proposed
  otherwise.** Going into the merge I said the clause should be dropped so the
  register would not claim `storage` excludes a file it owns. That was wrong,
  and row 8 is the precedent. Its Roots name three files while its Owned Files
  names one. Roots are *paths the sitting starts from* — an input, and a record
  of what that sitting actually walked. Sitting 9 did start from
  `crates/storage/src/ except search.rs`. Rewriting it now would falsify that
  record to tidy an apparent inconsistency. The register already explains that
  one row lower, where row 10's Detail reads `into storage`.

  No check reads Roots, and the recipe states no command consumes the clauses.
  Check 8 compares `## Owned Files` against `INVENTORY.txt`, which now includes
  `search.rs` under `storage`. Nothing is left inconsistent except a reader who
  stops at row 9.

- **[2026-09-17] `pipeline` confirmed at six files; the plan's expected drop was
  not supported.** Task 19's notes say "the plausible outcome is a drop rather
  than a domain". Measured, `pipeline` has three consumers beyond `main.rs`:
  `capture_runtime.rs` and `capture_supervisor.rs`, both owned by `capture`, and
  `ipc_handler.rs`, row 12's. `pipeline/sinks.rs` publishes the stage contracts
  as traits with typed enqueue-result enums. A domain that other domains program
  against is not a leftover. So `dropped` would have been the wrong outcome.

  `main.rs` and `settings.rs` were not really an open choice. Checked before
  deciding: **only row 11 reaches either file**. Rows 12, 13 and 14 are IPC,
  ipc-compat and UI, and none of their Roots touch `src/main.rs` or
  `src/settings.rs`. Check 8 set-compares owned files against `INVENTORY.txt`,
  so every inventory file must be owned by close. This was the last sitting that
  could reach them. The maintainer was told that before choosing.

  A split was offered and declined. It would have put `pipeline.rs` and the
  three submodules in one row and `main.rs` plus `settings.rs` in an appended
  row 15. Declined because the line would run *through* `main.rs` rather than
  around it. The helpers belonging to other domains stay in that file either
  way. So the new row inherits the same mixture, and buys a name rather than a
  boundary. It would also have been the first live use of Task 26 Step 3's
  missing split path, recorded this morning as plan defect 5.

  One new seam, `SEAM-stage-enqueue`, owner `pipeline`, with its second endpoint
  written into `maps/capture.md`. That edit also added two `Depends On` bullets
  capture had been missing. Sitting 1 could not see them because `pipeline` had
  not been walked. And leaving them out once measured would have been a known
  omission rather than an unknown one.

- **[2026-09-17] `main.rs`'s cross-domain helpers went to CHR-58, not a new
  ticket.** `main.rs` declares eleven modules, seven owned by decided domains.
  It also holds two helpers that are other domains' logic rather than
  composition. The first is `note_reconcile_outcome` (`:231`), which
  `docs/use-cases/background-work.md:20` names as half of a `background-work`
  solution split across this file and `capture_supervisor.rs`. The second is
  `handle_provision_event` (`:155`) with `begin_model_switch` (`:188`), the
  `transcription` model-switch path.

  CHR-58 already says the file is "accumulating responsibilities that could be
  factored", which is the same defect in different words. So the recipe's rule
  applied: label the existing issue `needs-domain` and record its ID rather than
  filing a second. The measurement went on as a comment so the finding is
  attached to something. The map still records who owns the file today,
  `pipeline`. That does not depend on the refactor happening.

- **[2026-09-21] `SEAM-permission-wire` re-described: the two sides are not
  corresponding types.** Sitting 2 wrote the seam as "the daemon's
  `MicrophoneStatus` and the UI's `MicState` are corresponding types defined
  independently", with permissions deciding "which grant states exist". Reading
  `map_outcome` (`chronicle-daemon/src/ipc_handler.rs:583`) after IPC was
  decided on 2026-09-18 shows that is not the shape. `MicState`
  (`chronicle-daemon/crates/ipc/src/lib.rs:75`) is a toggle-outcome type —
  `Off`, `On`, `PermissionDenied`, `Error`. And `MicrophoneStatus` is read only
  on the `Failed` branch, to choose between the last two. No grant state crosses
  the wire as itself.

  The seam now reads: permissions owns the grant vocabulary, IPC owns what the
  UI is told, and the contract is the mapping. Owner stays `permissions`. It
  publishes the vocabulary IPC must adapt to. And adding a `MicrophoneStatus`
  variant forces IPC to decide where it lands, not the reverse.

- **[2026-09-21] No UI-owned file references `MicState`.** `setMicEnabled`
  (`DaemonConnection.swift:170`) has no caller, and `micState`
  (`DaemonConnection.swift:556`) is read nowhere outside that file. So the
  permission vocabulary stops at IPC. This is why `permissions.md`'s third
  `outside` bullet could not be re-pointed at a UI-owned file: there is none to
  cite. The bullet now attributes the wire states to IPC rather than to "the
  UI".

  Folded into CHR-154 as a comment rather than filed as a new offshoot, on the
  maintainer's call. Same file, same transport/wire-vocabulary split, measured
  from the consumer side. The comment widens it past `setMicEnabled`. Of the
  nine request methods, that one alone has no caller. And of the five
  `StatusData` blocks, `ocr` and `audio` are decoded and never read by a view.
  The register's Offshoots cell for `permissions` is unchanged. CHR-154 already
  carries `needs-domain` from sitting 12.

- **[2026-09-21] OCR's two outward citations re-pointed at the owning domain.**
  Sitting 3 wrote "storing the extracted text" against
  `chronicle-daemon/src/pipeline.rs:163`, which is the call site.
  `update_ocr_text` is defined at `crates/storage/src/lib.rs:184` and
  `screenshots.rs:82`, and `storage.md` claims it as inside. It now cites
  storage.

  The second bullet said "reporting extraction counts to the UI" against the
  Swift `OcrStats`. Three things were wrong. `ocr_enqueued` and `ocr_dropped`
  are incremented in `pipeline.rs:136`, `:139` and `:145`. Those are queue
  outcomes in pipeline's `ocr_loop`, not extractions, and the OCR crate
  increments nothing. `DaemonConnection.swift` is IPC's file. And `OcrStats` is
  IPC's type (`crates/ipc/src/lib.rs:193`) built at `ipc_handler.rs:333`. And
  the `ocr` status block is decoded and never read by a view. So nothing reaches
  the UI. The bullet now names pipeline's bookkeeping, published by IPC.

  No seam with storage was added. OCR never calls storage. The text travels
  through pipeline, so the only direct edge is `SEAM-ocr-extraction`.

- **[2026-09-21] `audio.md`'s `transcription` paragraph got its own heading.**
  The 2026-09-18 code review called its placement under `## How The Existing
  Documents Saw This` a workaround, and it was one. Check 4 forbids a bare prose
  sentence in `Depends On` and `Depended On By`. So an explanation of why a
  domain is *absent* from a dependent list has nowhere legal to go in that
  section. It now sits under `## Why transcription Is Not A Dependent`, between
  `## Seams` and the documents table.

  Check 2 validates register-to-maps correspondence and check 4 parses only the
  four claim sections, so an extra heading is legal — the documents table and
  `## Offshoots Filed` already are ones.

  The paragraph's facts were re-measured and all hold. `chronicle-audio` is in
  transcription's `[dev-dependencies]`. And the four `use
  chronicle_audio::OggOpusEncoder` sites are `lib.rs:912`, `:946`, `:981` and
  `:1209`, all inside the `#[cfg(test)] mod tests` opening at `:535`. The line
  numbers went into the text since they had been measured.

  One pre-existing error surfaced while reading around the edit. The ADR-009 row
  said `capture` "owns the seam below", but `## Seams` is above the documents
  table. Corrected to "above".

- **[2026-09-21] `SEAM-power-event` added; `power` no longer has an empty Seams
  section.** Sitting 6 wrote `- None`, correctly for what it could see.
  `pipeline` was not walked until 2026-09-17, so the other endpoint did not
  exist yet.

  The edge has the same shape as `SEAM-permission-wire`. `power` publishes a
  two-variant `PowerEvent`, and `main.rs:822-826` translates each variant into a
  different supervisor call. And it carries a contract term the others do not.
  `try_send` at `power.rs:141` discards its error. So an event is dropped rather
  than queued when the bounded channel at `main.rs:376` fills. The seam is owned
  by `power`, with `pipeline` as the second endpoint.

  `Depended On By: pipeline` was left alone and is complete. `PowerEvent`
  appears outside `power.rs` only at `main.rs:376`, `:823` and `:825`.
  `capture_supervisor.rs` never names it. So `capture` does not depend on
  `power`, even though the `outside` bullet cites `set_system_asleep`.

- **[2026-09-21] Two corrections in `transcription.md`, no boundary change.**
  The `settings.rs` bullet said `whisper_model` sits "alongside two other
  domains' keys". `settings.rs` holds three key pairs — `mic_setting`,
  `capture_paused`, `whisper_model`. The first two are both read and written by
  `capture_supervisor.rs`. So they belong to one other domain, `capture`.
  `main.rs` also touches all three, but that is `pipeline`, which owns
  `settings.rs` itself and so is not an "other" domain. Two other keys, one
  other domain.

  The `background-work.md:49` verdict cell said which domain owns `pipeline.rs`
  "is contested between rows 8 and 11 and is not settled here". Row 11 was
  decided later the same day. And `maps/pipeline.md` is the only file listing
  `chronicle-daemon/src/pipeline.rs` under `## Owned Files`. The cell now
  records the outcome.

- **[2026-09-21] `Status` records revisits with three verbs, one per strength of
  evidence.** Maintainer's call, in two rounds. `revised <date>` means a pass
  read the file and changed a claim. `reviewed <date>` means a pass read it and
  changed nothing. No check parses the field, so the vocabulary is for readers
  asking whether a file was re-checked after a later domain landed.

  `edited <date>` was added after the merged review. It is the interesting one.
  The 2026-09-18 marks were back-filled from file mtimes and first written as
  `revised`. That claims more than an mtime can support: a timestamp proves the
  file differs, not that anyone judged its content.

  The 2026-09-18 session note splits the five. It names seven domains that "have
  not been read end-to-end this session" — `ocr`, `power`, `permissions`,
  `transcription`, `storage`, `audio`, `ui`. And it separately records "Step 4
  closed all nine reciprocity gaps... Steps 1/2 done for the domains those
  touched". Eleven minus those seven is `capture`, `background-work`, `ipc` and
  `pipeline`. So those four were read end to end, and had a claim added when
  their gap closed. That is `revised`, on that note's contemporaneous testimony
  and independent of any mtime.

  So `capture`, `background-work` and `pipeline` carry `revised 2026-09-18`,
  cited to the handoff. `ocr` and `audio` carry `edited 2026-09-18`. The handoff
  places both among the seven *not* read. And it records `audio.md`'s change as
  relocating a paragraph to satisfy check 4, which changed no claim. For those
  two, an mtime really is all there is.

  The first pass at this downgraded all five to `edited`, quoting only the half of
  the handoff that exonerates `ocr` and `audio`. Carrying half a source is how a
  correct-looking conclusion gets built on a true quotation.

  The mtimes themselves were read before this session's edits and are now
  overwritten. So they cannot be re-derived. That is the second reason not to
  spend a strong verb on them.

  `ipc` and `ui` were decided and edited on 2026-09-18. So a same-day mark would
  say nothing, and was not added.

  `storage.md` keeps `reviewed` even though this pass edited it. The only edits
  were pinning two floating date referents. No claim changed. **Superseded
  below:** a later entry the same day moves `storage.md` to `revised`, after the
  CHR-90 citation turned out to be wrong and CHR-156 was filed.

- **[2026-09-21] `storage` needed no boundary change — the first clean
  sitting.** Every measurable claim was re-verified. Seven importers, all
  reaching the crate through `Storage` or a `models` type with none naming a
  `pub(crate)` module. The control: `chronicle_storage::` matches 36 real hits,
  and brace-form `use chronicle_storage::{...}` imports were checked separately
  because the first pattern would have missed them. Zero `design §` citations
  under `crates/storage/src/` against nine in `src/provisioning.rs`. And 123
  lines in `003_null_marker_transcripts.sql`.

  No gap between `ui.md` and `storage.md`. `ui.md` cites
  `crates/storage/src/models.rs` as *outside* without claiming a dependency,
  which is right. Storage's data reaches the UI as IPC's `StorageStats`, not
  through an import.

- **[2026-09-21] Floating date referents pinned; one count was wrong.** Three
  measurement claims in `maps/` said "today", which stops meaning anything once
  the file outlives the session that wrote it. `storage.md`'s two are now dated.
  `transcription.md`'s said `lib.rs` "is 1287 lines today and seven files
  outside the crate reference `chronicle_transcription`". The line count is
  exact. The file count is six, not seven. The seventh hit is
  `crates/transcription/examples/transcribe_wav.rs`, which is inside the crate.
  It is the Cargo example Design §3.2 excludes from the inventory.

  Three other uses of "today" were left alone. They are idiomatic — "the map
  records who owns the file today, not who should" — not measurements.

- **[2026-09-21] Three claims corrected in `ui.md`; no boundary change.** UI was
  the last sitting, so nothing downstream could invalidate it. This pass was a
  claim audit. Most held exactly: 12 files and 1,631 lines,
  `SearchPopoverView.swift` at 429 and `SettingsView.swift` at 458, the
  `MenuBarExtra` at `ChronicleApp.swift:9-17`, ten keys in `Info.plist`, and the
  `INDEX.md` control matching exactly one line. "No Rust symbol is named
  anywhere in it" holds. Zero hits across the eleven owned Swift files, against
  a control firing twice in `DaemonConnection.swift`.

  What did not hold:

  1. "`docs/guides/` holds four files and `docs/use-cases/` six" counted one
     directory by files and the other by entries. `docs/use-cases/` holds eight
     files, six of them domain entries. `FORMAT.md` and `INDEX.md` are the
     others. And `docs/guides/`'s four already includes its `index.md`.

  2. "Nine of the twelve corpus files that mention the UI at all are under
     `.claude/audit/`" does not reproduce against the design's fixed corpus of
     39 files. Every pattern tried gives 19 to 21 files with 8 or 13 under
     `.claude/audit/`. None gives 12 and 9. The pattern was never recorded,
     which is why it could not be checked. Replaced with a measured count that
     states its pattern: thirteen of the twenty files naming `chronicle-ui` or
     `ChronicleUI`. The point it was making survives and is stronger.

  3. "Nine of its ten keys are bundle identity; `LSUIElement` is not" undercut
     its own argument. Seven are `CFBundle*` identity. `LSMinimumSystemVersion`
     is a deployment floor and `NSHighResolutionCapable` a rendering capability.
     And that last one is arguably as much a UI decision as `LSUIElement`, which
     is what the sentence was trying to single out.

  A count that cannot be re-run is not evidence. Every replacement here states the
  pattern or the file set it was measured over.

- **[2026-09-21] `storage`'s clean sitting reopened by a stale ticket citation;
  CHR-156 filed.** The claim check flagged seven CHR IDs that had not been
  resolved against Linear. Six were exact — CHR-121 Done 2026-05-21, CHR-47 and
  CHR-71 Done, CHR-77 Backlog, CHR-93 Todo, CHR-38 Done. One was not.

  `storage.md` recorded `.claude/audit/security.md:18-19` as "accurate — still
  true, and tracked as CHR-90". CHR-90 is **Done**, completed 2026-04-06, the
  same day the audit was written. It was an umbrella for that sweep, and this
  item was not addressed before it closed. The map was telling a reader the gap
  is tracked when nothing open covered it, which is worse than saying nothing.

  The concern itself was re-measured and holds. No `sqlcipher`, `encrypt` or
  `cipher` anywhere in the storage crate's or the daemon's `Cargo.toml` —
  `rusqlite 0.39` with `bundled-full`. And two FTS5 external-content tables,
  `screenshots_fts` at `migrations/001_initial_schema.sql:39` and `audio_fts` at
  `:44`, duplicating the text.

  Filed as CHR-156 rather than reopening CHR-90, whose scope was the 2026-04-06
  audit sweep and not this gap. `storage`'s Offshoots cell moved from `none
  found` to CHR-156, written after the domain file per Standing Constraint 4.
  And its `Status` moved from `reviewed` to `revised` — the sitting no longer
  changed nothing.

- **[2026-09-21] Two claim-check unverifiables settled in place.** `ui.md`'s "no
  Rust symbol is named anywhere in it" carried no pattern. So it could not be
  re-run. It now names one:
  `chronicle_|chronicle-daemon|MicrophoneStatus|StorageStatus|CompletedSegment`.
  Zero hits across the eleven owned Swift files, against two in
  `DaemonConnection.swift`.

  The mtime evidence behind the back-filled 2026-09-18 marks was consumed by
  this session's own edits. It cannot be recovered. The entry recording it now
  says so, so a later reader does not mistake an unrepeatable measurement for a
  wrong one.

- **[2026-09-21] What each sitting re-checked and left unchanged.** The merged
  review pointed out that the seven sitting entries above record the fields that
  *changed* and say nothing about the fields that were checked and held. That
  makes a silent pass indistinguishable from a skipped one, in a task whose only
  deliverable is a record. Closing that gap here rather than editing seven
  entries, so the check has one date rather than seven.

  **`Owned Files` was not edited in any of the eleven domain files this
  session.** Re-derived after the last edit: 64 inventory paths, 64 owned, every
  path owned exactly once, none unowned, none owned that is not in the
  inventory. Because no `Owned Files` section changed, Step 2's "when `Owned
  Files` changes, update `Owns` and `Code` to match" never fired. And `Owns` and
  `Code` are unchanged everywhere except where a sitting entry above says
  otherwise.

  **`Depends On` and `Depended On By`** were read in all seven sittings. Two are
  recorded above because they moved or were argued about. `power`'s was
  confirmed complete against every `PowerEvent` reference, and `audio`'s absence
  of `transcription` got its own section. The other five were read and left
  alone.

  Step 4's reciprocity test is not evidence for this. It catches an edge
  recorded on one side and not the other. It cannot see an edge recorded on
  neither. The three thinnest domains were checked against their actual imports
  rather than against each other. `permissions.rs` imports only
  `objc2_foundation`, `crates/ocr/src/lib.rs` only `std::path`, and
  `transcription` only `chronicle_ipc` outside its own crate. All three match
  their `Depends On`.

- **[2026-09-21] Two format extensions written into `FORMAT.md`.** Both were
  introduced by this pass and recorded only in this log. One is a domain file
  carrying a `## ` section outside `TEMPLATE.md`'s set. The other is the
  `Status` grammar with `revised` and `reviewed` clauses.

  The original justification was "check 2 validates register-to-maps
  correspondence and check 4 parses only the four claim sections, so an extra
  heading is legal". It is true and scoped to the checks that exist. Checks 5, 6
  and 8 do not exist yet. And Task 27 writes them against `FORMAT.md`, not
  against this file. A rationale that holds for a smaller set than the one it
  will be applied to reads as sound and is not. `FORMAT.md` now carries both
  rules. That includes the requirement that a check locate a section by name
  rather than by position or by enumerating the heading vocabulary.

- **[2026-09-21] `FORMAT.md` said two mandatory sections were optional
  additions.** The `## Sections beyond the template` text added an hour earlier
  called `## How The Existing Documents Saw This` and `## Offshoots Filed`
  additions "beyond the template". Both are *in* `TEMPLATE.md`, which carries seven
  headings, not five.

  The sentence came from a `DECISIONS.md` justification that was true in its own
  frame, namely sections beyond the four that check 4 parses. It became false
  when it was moved under a heading about sections beyond `TEMPLATE.md`. The
  same defect the whole revision pass has been chasing: a rationale that holds
  where it was written and not where it was carried.

  It matters because Task 27 writes check 6 against this file. And check 6 is
  the BR-4 accounting. An author told `## Offshoots Filed` is an optional
  addition would reasonably let a check tolerate its absence. And a domain file
  with no offshoots record at all would pass. `FORMAT.md` now states the seven
  are mandatory and names `audio.md`'s section as the only current addition.

- **[2026-09-21] The seam occurrence-versus-file trap written into
  `FORMAT.md`.** `## Seams` says an ID "appears in exactly two domain files".
  Three files name an ID twice as a prose cross-reference outside `## Seams` —
  `audio.md`, `background-work.md`, `transcription.md`. So a whole-file
  occurrence count reports three for those seams and fails them. And the
  tempting repair is to loosen the two-file rule check 5 exists to enforce.

  This had been carried as a review-conversation advisory for Task 27. That is
  the vehicle the finding above just proved unreliable. So it went into
  `FORMAT.md` instead, where check 5's author will be reading.

- **[2026-09-21] check 6 reads the leading ID of an Offshoots bullet, not every
  ID in the line.** The first draft collected every `CHR-\d+` in a bullet and
  compared that set against the register cell. It failed `storage.md` on its
  first run. That bullet reads "CHR-156 — capture history is unencrypted at rest
  and the CHR-90 that was said to track it is closed", and the description names
  a second ticket.

  `FORMAT.md`'s grammar is `- CHR-NNN — <one line>`. So the ID a bullet files
  under is the leading one, and the rest of the line is prose. The check now
  matches `^- (CHR-\d+)\s+—\s+\S` and rejects a bullet that does not have that
  shape. That is stricter than the old version as well as correct.

  Worth recording because the check caught this itself, on the live tree, the
  first time it ran. A parser written against a grammar and never pointed at real
  content is a parser nobody has tested.

- **[2026-09-21] Checks 5, 6 and 8 falsify against a synthetic fixture, not a
  copy of the live map.** They cannot use the live tree. Checks 5 and 8 need a
  `SYNTHESIS.md` Task 28 has not written. And 6 needs the
  `OFFSHOOTS_RESOLVED.txt` Task 29 writes at close. `falsify.sh`'s clean gate
  would abort on all three for exactly the stretch in which they most need
  falsifying.

  So `falsify.sh` grew a `synth()` builder: two domains, one seam, three
  register rows, two source files. It also builds its own repo tree, so check
  8's globs have something to regenerate from. Small enough to read in one
  screen, which is the point. A case that breaks a two-file inventory is
  legible, where the same case against 64 files is not. Seventeen cases run
  against it, each on a fresh copy.

  Its own clean gate runs first and aborts the section if any of the three is
  not green on it. That mirrors the live gate's reasoning: once the baseline is
  red, "the fixture broke it" and "it was already broken" are the same exit
  code.

- **[2026-09-21] The live clean gate tolerates the pre-close state, conditioned
  on the artifact being absent.** `check5` and `check8` exit 1 and `check6`
  exits 2 against the live tree today. All three are the documented pre-close
  state rather than defects. The gate now accepts exactly those three codes —
  but only while `SYNTHESIS.md` and `OFFSHOOTS_RESOLVED.txt` really are missing.
  The moment Task 28 or Task 29 writes one, the tolerance stops applying and the
  same failure is treated as real.

  A blanket exemption would have gone stale silently on the day the artifact
  landed. That is the failure this whole revision pass has been finding in
  prose.

- **[2026-09-21] `falsify.sh` was modified although Task 27's Files list names
  only the three checks.** Same precedent as Task 25, which added the harness on
  the maintainer's approval as a file outside the plan's list. The 2026-09-18
  session note recorded it at the time, calling `falsify.sh` "a harness that is
  not in the plan's file list and was added on the maintainer's approval". So
  the precedent rests on a contemporaneous record rather than on this entry's
  say-so. Step 4 requires each new check to be broken on purpose and seen to
  fail. Putting that evidence anywhere other than the runnable harness would
  leave it in whoever ran it last.

- **[2026-09-21] shellcheck's `SC2034 REPO_ROOT appears unused` left standing on
  check 6.** Checks 1, 2, 3 and 6 accept `REPO_ROOT` and never read it. Checks 5
  and 8 do read it. The uniform two-argument signature is Task 2 Step 1's
  interface. It is what lets the falsification harness invoke any check the same
  way. Silencing it on check 6 alone would make the newest file the only one
  carrying a disable directive. The plan calls shellcheck advisory. Its other
  output is `SC2016` on single-quoted `perl` breakers, where `$1` is perl's
  capture group and single quotes are correct.

- **[2026-09-21] check 5 accepts three dressings of the synthesis File cell.**
  The spec review found the first draft required that cell to equal the
  register's Detail cell verbatim. That would have blocked the close if Task 28
  wrote `` `maps/x.md` `` or `[x](maps/x.md)` instead of a bare path. The rule
  worth enforcing is that the cell points at the register's file. Which dress it
  wears is a formatting choice. And failing a close over one is a rule nobody
  agreed to.

  `file_cell()` now strips backticks and unwraps a markdown link before
  comparing. Verified against all three forms: each exits 0.

  Caught before Task 28 rather than by Task 28, which is the whole reason this
  task's review is two-stage.

- **[2026-09-21] Three deliberate extras in check 5, kept and recorded.** The spec
  review flagged them as scope creep. They are, and they stay, so the record says
  why rather than leaving someone to find them and wonder:

  - Non-empty Contract and Evidence cells in the Seams table, and a non-empty
    Owns cell in Domains. Step 1 says "against `SYNTHESIS_TEMPLATE.md`'s
    structure". And a blank cell is a column the template declares and the
    synthesis does not fill.
  - The reverse Candidate Outcomes join — a synthesis naming a candidate the
    register does not have. Step 1 asks only for the forward direction. But the
    reverse catches a synthesis that invented a domain, which is the failure
    BR-1 cares about.
  - Duplicate detection on synthesis rows, inventory paths, exclusion rows and
    resolved IDs. A table naming the same key twice passes every set comparison
    while telling a reader two different things.

  None of the three can fire on a synthesis that is correct. So the cost of
  keeping them is nil, and the cost of the close being blocked by one is a
  conversation, not a rewrite.

- **[2026-09-21] Five copies of `section()` and two register-row parsers
  reconciled.** The code review found the suite had drifted into two
  incompatible readings of its own register. Checks 1, 2 and 6 matched
  `^\|\s*\d+\s*\|` and silently dropped anything else. Checks 5 and 8 took every
  pipe-led line and discarded the first as a header. A row whose `#` cell is not
  a number was invisible to the first three and a live candidate to the other
  two. `section()` had drifted too — five copies, two signatures, and only three
  of them `re.escape`d the name.

  The sharpest version of the finding is that `check5_synthesis.sh` shells out
  to check 1 rather than re-deriving completeness. Its comment says "Two
  implementations of the same rule is how they come to disagree". And that same
  file introduced a second implementation of the register-row parser.

  A shared Python module was considered again and rejected again, for the reason
  Task 25 rejected it. These scripts are run standalone, often one at a time, by
  someone who did not write them. Instead `section()` and a new
  `register_rows()` are now **byte-identical copies** across checks 1, 2, 5, 6
  and 8. That was verified by hashing the extracted function from each file.
  `table_rows()` stays in checks 5 and 8 for the synthesis tables, whose first
  cell is a name rather than a number.

  `register_rows()` reports a non-numeric `#` cell rather than skipping it. So
  the ambiguity that made the two parsers disagree is now a failure in all five.
  A falsification case covers it. The docstring is raw because the `\|` in its
  own explanation raised a `SyntaxWarning` on every run.

- **[2026-09-21] The `||` guard on check 8's find pipeline did not do what its
  message said.** The re-review demonstrated it: `( a; b; c; d )` exits with
  **d's** status. So a failure in any of the first three globs was invisible to
  a guard on the group, `pipefail` or not. The `chronicle-daemon` sweep — the
  biggest one and the likeliest to meet an unreadable subdirectory — was the
  unguarded one.

  Probed end to end with an unreadable directory holding an inventoried `.rs`
  file. Before: `INVENTORY.txt lists chronicle-daemon/locked/hidden.rs, which
  the globs no longer find`. That sends the reader to the inventory when the
  problem was `find`. After, with each glob carrying its own `|| exit 1`:
  `check8: inventory regeneration failed; the globs did not complete`.

  This is the second time in this task a fix was written, looked right, and did
  not work. The first was the malformed-bullet falsification cases, whose `want`
  strings named a downstream message the fix had made unreachable. The harness
  reported WRONG FAILURE, and that is how it was found. Both were caught by
  running the thing rather than by reading it.

- **[2026-09-21] Harness housekeeping from the re-review.** Two check 6 cases
  had been appended after the check 8 block. So the heading said check 8 while
  the message said check6 — in a harness whose value is legibility to someone
  who did not write it. They are filed under check 6 now. And the shared-parser
  case has its own heading, since it is about `register_rows` rather than about
  check 6.

  The vacuum case was labelled "an empty inventory" while asserting "the globs
  found zero source files". Emptying both sides only ever proves the first
  guard, because it fires before the second is reached. Split into two cases,
  one per guard.

  Added a case for check 6's `BULLET_ID` no-match branch, which was reachable
  and uncovered. It is now the only thing between a wrongly-shaped ID and a
  silent pass, since the bare-prose guard took the other half.

  `check5_synthesis.sh` was the only one of the five that dropped a short register
  row silently where the other four reported it. Pre-existing, but conspicuous one
  line after the reconcile made the four-way agreement explicit. It reports now.

- **[2026-09-21] The synthesis says "the closure checks pass" while check 6
  exits 2.** Task 28 Step 2 is explicit: take the status line from check 1's
  `complete` or `incomplete`, in the exact `FORMAT.md` wording. check 1 reports
  `complete`, so the line is `> **Authoritative.** The walk is complete and the
  closure checks pass.` And check 5 validates it against check 1 and nothing
  else.

  Its second clause is not literally true yet. check 6 exits 2 because
  `OFFSHOOTS_RESOLVED.txt` does not exist until Task 29 Step 3 writes it. And
  MVF-2's exit criterion is Task 29 Step 7's green run, not this one. Writing
  the line now is what the format requires. Task 29 is what makes it true.
  Recorded rather than quietly resolved either way, because a reader who ran the
  suite between Task 28 and Task 29 would see a 2 and wonder. **Superseded
  2026-09-21:** Task 29 wrote `OFFSHOOTS_RESOLVED.txt`, check 6 exits 0, and
  both clauses of the status line are now true.

- **[2026-09-21] The Exclusions table is empty, and that is the result.** All 64
  inventory paths are owned by exactly one confirmed domain, so nothing needs an
  exclusion reason. The plan's Task 28 flagged a known gap — that the exclusion
  table's reasons could not be recovered from `INVENTORY.txt` minus owned paths.
  At 64/64 it is moot. The 2026-09-18 session note anticipated that in those
  words: "With 64/64 owned there are no exclusions, so this may now be moot —
  confirm before relying on it." This is that confirmation.

  The two paths the design singled out as needing a decision rather than a
  default both landed. Both were re-verified while writing the table:
  `chronicle-daemon/src/settings.rs` in `pipeline`, and
  `chronicle-ui/Sources/ChronicleUI/Info.plist` in `UI`.

- **[2026-09-21] The pre-close tolerance expired by itself, as designed.**
  Before the synthesis existed, `falsify.sh`'s live gate accepted check5 exit 1
  and check8 exit 1 on the condition that `SYNTHESIS.md` was genuinely absent.
  The moment Task 28 wrote it, both checks went green. The gate stopped printing
  their exemption line without anyone editing the gate.

  Only check 6's tolerance remained live, and it expired the same way at Task 29
  Step 3. **As of the close all three are dead branches**, kept deliberately —
  `falsify.sh` explains why at its clean gate.

- **[2026-09-21] The synthesis's `search` row conflated the register's Roots
  column with its Symbols column.** It said the Swift files row 10's Roots named
  "went to UI and IPC instead". All three — `SearchPopoverView.swift`,
  `ResultRow.swift`, `SnippetAttributedString.swift` — went to UI, and `maps/ui.md`
  says so outright.

  The IPC half came from the *Symbols* cell, a different column. `SearchHit`,
  `SearchHitSource` and `SearchResponse` are declared in
  `DaemonConnection.swift`, which IPC owns. Both facts are true and they are
  about different things. The sentence welded them into one wrong claim.

  The row now states them separately. No check could have caught this. Check 5
  joins on candidate *names* and never reads the "where it ended up" prose,
  which is the half a reader actually reads.

- **[2026-09-21] The status line's second clause has no legal fix inside this
  plan.** The merged review called it Important and was right about the shape.
  The "Authoritative" string welds two independent facts into one sentence: the
  walk is complete, and the closure checks pass. Only the first is gated. check
  5 selects between the two strings on check 1's exit code alone. So if Task 29
  were abandoned, the false clause would ship with no check able to fire on it.

  It cannot be fixed here. The string is fixed by the **design** at
  `docs/plans/2026-09-14-chr-150-reconcile-domain-maps-design.md:469`, not by
  `FORMAT.md`. And `FORMAT.md` says outright that where the two disagree, the
  design wins and `FORMAT.md` is the defect. Splitting the sentence in
  `FORMAT.md` would manufacture that disagreement. Editing the design is not
  available either: `docs/plans/` is inside the set BR-5 protects and closure
  check 7 hashes it.

  So it is recorded as design feedback for the follow-on spike rather than
  actioned: **BR-6's status line should gate one claim, not two.** The reviewer's
  own judgement was that Task 28 made the correct call and the synthesis should not
  change. Today's exposure is one task wide and the only reader inside it is the
  walker.

- **[2026-09-21] The synthesis gained the framing a cold reader needs.** The
  merged review read it as someone who had never seen the project and found two
  real gaps, both one sentence.

  It promised "given a source file, this says which domain owns it" and then
  pointed at the placement test. That test answers where a *new* file should go,
  not where an existing one already lives. The answer to the question actually
  asked is `## Owned Files`, an exact enumeration covering all 64 paths. The
  synthesis never named it. It does now, with both routes stated separately.

  It also let ten seam rows read as the whole interaction graph. `pipeline`
  alone names nine `Depends On` entries, of which three are seams. `pipeline →
  storage` is a live dependency with no seam row anywhere. A seam is only where
  a contract had to be agreed. The synthesis now says so, and points at the
  domain files for the rest.

- **[2026-09-21] `capture.md` and `background-work.md` carry a 2026-09-21 mtime
  and no 2026-09-21 Status clause, deliberately.** Their only edit today was
  rewriting the Status line itself, when the 2026-09-18 marks moved from
  `edited` back to `revised`. Stamping `edited 2026-09-21` for an edit that
  changed nothing but the stamp would be noise. And `reviewed` would be false —
  nobody read either file today. The vocabulary describes what happened to a
  domain file's *claims*. Neither file's claims were touched.

- **[2026-09-21] CHR-155 was filed by the walk and recorded by nothing.** The
  closure run's Step 3 turned it up: nine Linear issues carry `needs-domain` and
  the register named eight. CHR-155 — "chronicle-ipc re-exports CancellationToken,
  creating a false domain edge" — was never written into any register row.

  It was not a matter of the ticket arriving too late to be recorded. CHR-155
  was created at 19:39 EDT on 2026-09-18. The session note for that day was last
  written at 20:31 EDT, fifty-two minutes *later*. And that note does record the
  finding: "The `CancellationToken` edge is recorded, with the re-export named
  in both `background-work.md` and `ipc.md`." It names the edge and not the
  ticket.

  So the *finding* was written down three times: `background-work.md:28`,
  `ipc.md:52`, and the session note's decision list. The *ticket ID* existed
  only in Linear. Nothing carried the ID across, and `CHR-155` appeared nowhere
  under `docs/domains/`. A finding and the issue filed for it are two different
  artifacts. Recording one is not recording the other.

  Recorded on row 12, IPC, on the maintainer's call. The offending line is
  `crates/ipc/src/lib.rs:352`, a file IPC owns. And the fix CHR-155 itself
  proposes lands there. `background-work` is where the symptom shows: the map
  implies the retention scheduler talks to the daemon-UI protocol. But it is not
  where the change goes.

  **The check could have caught this. The procedure could not.** An earlier
  version of this entry said no check could, and that was wrong.
  `check6_offshoots.sh:245` already carries the guard:

  ```python
  for i in sorted(resolved_ids - register_ids):
      fail.append(f"{i} is in OFFSHOOTS_RESOLVED.txt but no register row names it")
  ```

  Verified by planting a `CHR-999` line in a copy of the tree: check 6 exits 1 with
  exactly that message. The branch is live.

  What made it unreachable is Task 29 Step 3's input rule — "Read the IDs from
  the register. For each, call Linear." Build the file that way and it can only
  ever contain IDs the register already names. So `resolved_ids - register_ids`
  is empty by construction. The guard is dead not because it is wrong but
  because nothing can feed it a surprise.

  This session's own sequence proves the branch works: `maps/ipc.md` was written
  at 16:29:22, `OFFSHOOTS_RESOLVED.txt` at 16:29:28, `REGISTER.md` at 16:30:20.
  For fifty-two seconds the resolved file named CHR-155 and no register row did.
  A check 6 run in that window would have failed with the line above.

  So the fix is the rule, not the code: **write `OFFSHOOTS_RESOLVED.txt` from a
  `label:needs-domain` query, not from the register.** That is what the executor
  actually did, which is the only reason the gap surfaced at all. The header
  comment on check 6 now says so. Two of its branches — this one and the
  duplicate-ID one — still have no falsification case, which is carried forward
  below.

- **[2026-09-21] The resume drill passed, and its gaps were worth more than its
  verdict.** A fresh reader was given `REGISTER.md` and forbidden every other
  file. It derived the termination condition from the stated rule, and concluded
  correctly that there is no next candidate. It listed all fourteen sittings
  with the right paths and merge targets. And it cross-checked the close-out's
  own arithmetic against the table instead of trusting it. NFR-2 holds.

  Three gaps it named were real and are now closed in the register. It cited
  nine terms — `NFR-2`, `MVF-1`, `Design §n`, numbered closure checks — into two
  documents whose paths the file never gave, and which are gitignored. The
  header now names both paths and says they are gitignored. So a reader who
  cannot find them knows why. The file never named CHR-150, recording nine
  offshoot IDs and zero parent IDs. And "confirmed in Linear under the
  `needs-domain` label", in a file called `OFFSHOOTS_RESOLVED.txt`, reads as
  though nine open questions were settled. The close-out now says outright that
  resolved means looked-up-and-labelled, and that all nine are open work.

  It also flagged the hand-applied `except` clauses in rows 4, 9 and 14 as
  unverified set arithmetic. And it flagged `DaemonConnection.swift` appearing
  in three rows' Roots as a contradiction of single ownership. Both are answered
  by check 8, which the register never mentioned. It does now. That includes
  stating that Roots scope a sitting's searches and do not assign ownership,
  which is the distinction that dissolves the apparent contradiction.

- **[2026-09-21] The suite is green: eight PASS, aggregate 0.** This is the run
  that satisfies MVF-2's exit criterion, not the Step 2 run. Two things changed
  between them. `OFFSHOOTS_RESOLVED.txt` was written, and `evidence/` — fourteen
  packs, 148K — was deleted per Design §3.1.

  The synthesis's status line is now true in both clauses. It has said "the walk
  is complete and the closure checks pass" since Task 28, when only the first
  half held. The entry above about it having no legal fix inside this plan
  describes a window that is now shut. The design-level observation stands for
  the follow-on spike: BR-6's status line gates one claim and asserts two.

- **[2026-09-21] A UTC timestamp compared against a local-time mtime.** The
  entry above first said CHR-155 was created "three hours after that day's
  handoff was written, which is how it missed the register". Both halves were
  wrong, and from one mistake. Linear returns `2026-09-18T23:39:11Z` and `stat`
  prints local time, which is EDT here. Converted, the ticket predates the
  handoff's last write by fifty-two minutes rather than following it by three
  hours. So the causal story ran backwards as well.

  Worth keeping because the sentence read as precise. "Created at 23:39, three
  hours after" carries two numbers and an explanation. None of it survived
  converting one of them. A timestamp from an API and a timestamp from the
  filesystem are not in the same frame unless something puts them there.

- **[2026-09-21] The close-out review found the walk closed and the record
  one-directional.** Zero Critical. Its sharpest point was that the tree records
  what happened impeccably and records what is still owed nowhere. A reader
  inheriting this had no open list, and `DECISIONS.md` is chronological with no
  index. `REGISTER.md`'s close-out now carries a `Still open` block naming all
  three items. And this file opens with a way in.

  Its other structural point: **the map exists in one place and nothing backs it
  up.** 34 untracked files in a working checkout, and `check7_baseline.sh`
  actively enforces that state. So a green suite guarantees the map exists
  nowhere else. The close-out now says so and names the long-term-home decision
  as due. That was the honest answer to "is the walk closed or does it only
  report as closed".

  **Superseded 2026-09-22:** the map is committed and gated in CI, and the
  baseline check is deleted. `REGISTER.md`'s close-out records what was settled.

- **[2026-09-21] Two comments in `falsify.sh` were falsified by this very
  task.** Both said checks 5, 6 and 8 need a synthetic fixture because
  `SYNTHESIS.md` and `OFFSHOOTS_RESOLVED.txt` "have not been written yet". Tasks
  28 and 29 wrote them, so the stated reason expired while the files sat
  untouched. That is the exact class the revision pass spent itself on: a change
  falsifying comments in a file it never edited.

  The fixture stays. Only its justification changed. The reason now is size. A
  case that breaks a two-file inventory shows what it broke, where the same case
  against 64 files buries it. The pre-close tolerance block is now three dead
  branches, kept deliberately and labelled as such, because the tree they guard
  is untracked. A lost or half-restored `docs/domains/` puts the suite back in
  the pre-close state. And a harness that aborts with a reason beats one that
  reports a bare failure.

- **[2026-09-21] Two dead branches in check 6 now have falsification cases.**
  The reverse guard — an ID in `OFFSHOOTS_RESOLVED.txt` that no register row
  names — is the one CHR-155 needed and never reached. It had no case. Nor did
  the duplicate-ID branch. Both do now, and both bite. The harness is at 40
  cases, 39 bit, one no-op control.

  Writing them is what proved the guard live rather than arguing it from the
  source. Planting `CHR-999` in a copy of the tree fails check 6 with the exact
  message. A branch with no case is a branch nobody has watched work.
