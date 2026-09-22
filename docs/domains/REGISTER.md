# Candidate Register

**Walk state lives here.** A session with no prior context reads this file to
find the next candidate and what every decided sitting produced. Per NFR-2.

**Where the terms come from.** This file cites `NFR-2`, `MVF-1`, `BR-n`, `Design
§n` and numbered closure checks without defining them. They are defined in
`docs/plans/2026-09-14-chr-150-reconcile-domain-maps-design.md` and the
implementation plan beside it,
`docs/plans/2026-09-16-chr-150-domain-map-implementation.md`. **Both are
gitignored**, so they exist only in a working checkout. If they are gone, the
definitions are gone with them, and this file's rules are the only surviving
statement of the method.

The work is Linear issue **CHR-150**, "Spike: reconcile the competing domain maps
into one". The `CHR-` IDs in the Offshoots column are separate issues. Some were
filed by a sitting and some already existed and were labelled by one. That is
why their numbers are not contiguous.

MVF-1 walks the lowest-numbered row with an empty Outcome that is either not
blocked, or blocked on a candidate that is now decided. Decided means any of
`confirmed`, `merged`, `dropped`, `split`. A blocked row becomes walkable the
moment its target has an outcome. It is not excluded for good. The walk repeats
until no row has an empty Outcome and Blocked rows is empty.

A split appends rows with the next free integers, filling in their Roots and
Symbols at the same time. Existing numbers are never rewritten.

## Outcome grammar

Every cell below is parsed by closure check 1. The forms are exact. Per
Design §1.2.

| Outcome | Detail must be | Domain file |
|---|---|---|
| `confirmed` | `maps/<name>.md` | Required, at that path |
| `merged` | `into <candidate>`, where that candidate is already `confirmed` | None |
| `dropped` | `cross-cutting: <one sentence naming what handles it>` | None |
| `split` | `into <name>, <name>[, ...]`, each appended as a new row | None |

`Decided` is `YYYY-MM-DD`. `Offshoots` is the literal `none found`, or `CHR-`
IDs separated by `, ` and nothing else.

`Roots` and `Symbols` are separated by `; ` — semicolon then space. Roots are
written relative to `chronicle-daemon/` or `chronicle-ui/`. A command run from
the repo root needs that prefix. `grep` fails silently without it. A Symbols
cell reads `rust: a; b; swift: X; Y`. The `rust:` and `swift:` tags are what
split it between the two dependency searches. Some Roots cells carry an
exclusion clause in prose (`except`, `less the files claimed by`). No command
consumes those, so apply them by hand.

A merge may only name a target that is already confirmed. Design §1.3 requires
the target's Boundary section to absorb the responsibility. A file that does
not exist cannot absorb anything.

## Blocked rows

When the maintainer wants a merge into an undecided target, the row's Outcome
stays empty. The row is recorded here naming what it waits on. The walk
returns to it once the target is decided. A blocked row is incomplete, not
decided. Check 1 counts it as incomplete. A walk still holding one cannot
close. Per Design §1.4.

| # | Candidate | Waiting on | Since | Offshoots |
|---|---|---|---|---|

A blocked sitting writes no Outcome. Check 1's Offshoots rule cannot see it.
Its offshoot IDs go in the row above, or they are lost and closure check 6 fails
at close.

## Exclusions pending

A dropped candidate's inventory files land here with a reason, one row each. A
dropped sitting writes no domain file. Its Detail cell holds one sentence.
Without this block the reasons have nowhere durable to live.

Task 28 does not read this block. It derives the synthesis's exclusion paths
from `INVENTORY.txt` minus owned paths. It can recover the paths that way but not
the reasons. It requires them non-empty. Until the plan is amended, whoever
writes the synthesis copies the reasons from here by hand.

For the 2026-09-21 walk this never fired. Nothing was dropped, so this table
stayed empty. The synthesis's exclusion table is empty too. The gap is recorded
under `## Close-out` → Still open.

| Path | Reason | Dropped candidate |
|---|---|---|

## Register

| # | Candidate | Outcome | Decided | Detail | Offshoots | Roots | Symbols |
|---|---|---|---|---|---|---|---|
| 1 | capture | confirmed | 2026-09-16 | maps/capture.md | CHR-151 | crates/capture/src/; src/capture_runtime.rs; src/capture_supervisor.rs | rust: capture_runtime; capture_supervisor; chronicle_capture; engine; encoder; metadata; drops; deps; handler; pixel_buffer; swift: CaptureStats |
| 2 | permissions | confirmed | 2026-09-16 | maps/permissions.md | CHR-152 | src/permissions.rs; Sources/ChronicleUI/DaemonConnection.swift (MicState region); Sources/ChronicleUI/StartupAlertState.swift | rust: permissions; swift: MicState; StartupAlertState |
| 3 | OCR | confirmed | 2026-09-16 | maps/ocr.md | none found | crates/ocr/src/lib.rs | rust: chronicle_ocr; swift: OcrStats |
| 4 | audio | confirmed | 2026-09-16 | maps/audio.md | none found | crates/audio/src/ except encoder.rs and accumulator.rs; src/drop_reporter.rs; src/media_presence.rs | rust: chronicle_audio; device; drops; engine; handler; microphone; drop_reporter; media_presence; swift: AudioStats; MicState |
| 5 | audio-encoding | merged | 2026-09-16 | into audio | none found | crates/audio/src/encoder.rs; crates/audio/src/accumulator.rs | rust: encoder; accumulator |
| 6 | power | confirmed | 2026-09-16 | maps/power.md | CHR-36 | src/power.rs | rust: power |
| 7 | transcription | confirmed | 2026-09-17 | maps/transcription.md | CHR-153 | crates/transcription/src/lib.rs; src/provisioning.rs | rust: chronicle_transcription; provisioning; swift: TranscriptionState; TranscriptionStats; TranscriptionAlertState; TranscriptionBannerCopy; ModelEntry |
| 8 | background-work | confirmed | 2026-09-17 | maps/background-work.md | none found | src/retention_task.rs; crates/storage/src/retention.rs; crates/ipc/src/retention.rs | rust: retention_task; retention; swift: Retention; RetentionCopy |
| 9 | storage | confirmed | 2026-09-17 | maps/storage.md | CHR-156 | crates/storage/src/ except search.rs | rust: chronicle_storage; audio; files; media; models; schema; screenshots; error; retention (scope hits to crates/storage/src); swift: StorageStats |
| 10 | search | merged | 2026-09-17 | into storage | none found | crates/storage/src/search.rs; Sources/ChronicleUI/SearchPopoverView.swift; Sources/ChronicleUI/ResultRow.swift; Sources/ChronicleUI/SnippetAttributedString.swift | rust: search; swift: SearchHit; SearchHitSource; SearchResponse; SearchPopoverView; ResultRow |
| 11 | pipeline | confirmed | 2026-09-17 | maps/pipeline.md | CHR-58 | src/pipeline.rs; src/pipeline/; src/main.rs; src/settings.rs | rust: pipeline; settings |
| 12 | IPC | confirmed | 2026-09-18 | maps/ipc.md | CHR-56, CHR-154, CHR-155 | crates/ipc/src/lib.rs; crates/ipc/src/server.rs; src/ipc_handler.rs; Sources/ChronicleUI/DaemonConnection.swift; Sources/ChronicleUI/ConnectionSession.swift | rust: chronicle_ipc; server; ipc_handler; swift: DaemonConnection; ConnectionSession; IPCRequest; IPCError; ErrorResponse; DaemonErrorCode |
| 13 | ipc-compat | merged | 2026-09-18 | into IPC | none found | crates/ipc/src/lib.rs; crates/ipc/src/retention.rs; Sources/ChronicleUI/DaemonConnection.swift | rust: retention (scope hits to crates/ipc/src); swift: Retention; RetentionCopy; DaemonErrorCode |
| 14 | UI | confirmed | 2026-09-18 | maps/ui.md | none found | Sources/ChronicleUI/ less the files claimed by search, IPC, and ipc-compat | swift: ChronicleApp; MenuBarIcon; SettingsView; ScreenshotDetailView; StatusData; StatusResponse |

## Close-out

- **[2026-09-21] The walk is closed.** All fourteen rows are decided: eleven
  `confirmed`, three `merged`, none split, none dropped. No row is blocked.
  There is no next candidate.

  The map's entry point is `docs/domains/SYNTHESIS.md`. It names every confirmed
  domain with the file that describes it, every seam with its owner and both
  endpoints, and every candidate with where it ended up. Each confirmed domain's
  own file is at the `maps/` path in its Detail cell.

  All 64 paths in `INVENTORY.txt` are owned by exactly one confirmed domain, so
  the synthesis's Exclusions table is empty. That is machine-checked, not
  asserted. `checks/check8_coverage.sh` regenerates the inventory from the repo
  and fails if any path is owned twice, owned by nobody, or both owned and
  excluded. It is what settles the hand-applied `except` clauses in rows 4, 9
  and 14. It is also why a path appearing in several rows' Roots is not a
  contradiction. Roots scope a sitting's searches. They do not assign ownership.
  `## Owned Files` in each domain file does that.

  The nine offshoot IDs named in the Offshoots column are listed in
  `OFFSHOOTS_RESOLVED.txt`. "Resolved" there means the ID was looked up in Linear
  and carries the `needs-domain` label. **It does not mean the issue is closed**.
  All nine are open work. BR-4 requires offshoots to be filed and not fixed. The
  walk left every one of them for later.

  **Where the map lives, settled 2026-09-22.** For the length of the walk
  `docs/domains/` was untracked. The design intended that and a baseline check
  enforced it. That left the map in one place with nothing backing it up. A
  `git clean -xdf` or a lost checkout destroyed it. A green suite said nothing
  about that. The map, the checks and the falsification harness are now
  committed. `.github/workflows/domain-map.yml` runs checks 3, 4, 8 and
  `falsify.sh` on every pull request. The baseline check and its manifest were
  deleted with the walk they guarded.

  ### Still open

  The walk closed; these did not. None is tracked anywhere but here and in
  `DECISIONS.md`. That file is chronological and has no index.

  - **Five plan defects**, found during execution. They were deferred to an
    amendment that was never written. Recorded in `DECISIONS.md` under the
    2026-09-16 entries: the Task 23 walkability rule, `## Exclusions pending`
    reassignment, Task 28's exclusion-reason gap, two "Task 25 revisits it"
    mislabels, and Task 26 Step 3's missing split path.
  - **BR-6's status line gates one claim and asserts two.** The `Authoritative`
    string says the walk is complete *and* the closure checks pass. Check 5
    selects it on check 1's completeness alone. Both halves are true today. The
    string is fixed by the design, which is inside the protected set. This is
    feedback for the follow-on spike rather than something this plan could fix.
  - **The offshoot-accounting rule.** `OFFSHOOTS_RESOLVED.txt` must be written from
    a `label:needs-domain` query, not from the IDs the register already names.
    Otherwise check 6's reverse guard is unreachable and an unrecorded offshoot is
    invisible. CHR-155 was exactly that case. The check's header now says so.
    Task 29 Step 3's own wording still says to read the IDs from the register.

  The evidence packs that were under `evidence/` are deleted. They were working
  material for the sittings, per Design §3.1. Every claim they supported is in the
  domain files or `DECISIONS.md`. Each one cites a path and a symbol, so it is
  re-derivable from the repo. What did not survive is the per-sitting search
  provenance. That is which dependency search produced a given bullet, and what
  the negative searches returned. Nothing that remains depends on it.
