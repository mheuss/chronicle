# Domain: power

**Status:** Decided 2026-09-16, revised 2026-09-21
**Owns:** Hearing the operating system say the machine is going to sleep or has woken, and saying so in one word.
**Code:** `chronicle-daemon/src/power.rs`

## Boundary

- inside: registering with IOKit and holding the kernel port that acks a sleep message — `chronicle-daemon/src/power.rs` — `spawn_power_observer`
- inside: the two-word vocabulary the rest of the daemon reacts to — `chronicle-daemon/src/power.rs` — `PowerEvent`
- outside: deciding what a sleep event means for recording — `chronicle-daemon/src/capture_supervisor.rs` — `set_system_asleep`
- outside: routing an event to whoever acts on it — `chronicle-daemon/src/main.rs` — `power_rx`
- outside: keeping the timeline correct across a gap the event describes — `chronicle-daemon/crates/audio/src/accumulator.rs` — `SegmentAccumulator`

**Placement test:** Does the file listen to the operating system for a power transition? Deciding what to do about one belongs to whoever is running at the time.
**Document comparison:** differs — no document describes this module. `docs/use-cases/pipeline.md` treats system sleep as an input to its reconcile pattern, which is the reaction rather than the observation, and no guide or ADR mentions sleep or wake at all.

## Owned Files

- chronicle-daemon/src/power.rs

## Depends On

- None

## Depended On By

- pipeline — `chronicle-daemon/src/main.rs` — `spawn_power_observer`

## Seams

- SEAM-power-event — pipeline — power names the transition and pipeline decides what it means: a two-variant `PowerEvent` delivered fire-and-forget, since `try_send` at `power.rs:141` logs at warn and discards its error, so an event is dropped rather than queued when the bounded channel at `main.rs:376` is full — owner: power — `chronicle-daemon/src/power.rs` — `PowerEvent`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/pipeline.md` | Treats "pause, system sleep, etc." as changes to a desired state that a runtime state must reconcile against, and lists "race between pause and sleep events" among its indicators | accurate about the reaction; it describes what consumes `PowerEvent`, not what produces it |
| `docs/use-cases/INDEX.md` | Does not name `power`. Its only sleep reference is `audio-encoding`, for "sleep-gap detection" | stale by omission |
| `docs/guides/` | Nothing — no guide mentions sleep or wake | stale by omission |
| `docs/decisions/` | Nothing — no ADR mentions power, sleep or wake | accurate, in that no decision was needed here |
| `chronicle-daemon/src/power.rs:92` | A code comment justifies fire-and-forget delivery by citing "`SegmentAccumulator` gap-detection (design §5.2)" | wrong — `design §5.2` does not exist in the corpus, and `SegmentAccumulator` has since moved to `audio`. Counted in CHR-36 |

## Offshoots Filed

- CHR-36 — a dangling `design §5.2` citation at `power.rs:92`; the existing issue already counts this file, so it was labelled rather than duplicated
