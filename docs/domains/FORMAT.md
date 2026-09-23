# File Formats

Restates Design §2.1, §2.2, and §2.3 in one place, for the check scripts to be
written against. Where this file and the design disagree, the design wins. This
file is the defect.

## Where domain files live

`docs/domains/maps/<name>.md`, and nothing else lives in `maps/`. "Every domain
file" means every path named by a confirmed register row's Detail cell.

## Header fields

`Status`, `Owns`, and `Code` are required and non-empty, per Design §2.1. A
present-but-blank field passes a check that only looks for the label.

`Status` is `Decided <date>`. It is optionally followed by one or more clauses,
each `, edited <date>`, `, revised <date>` or `, reviewed <date>`. The three say
how much is known about what happened:

- `revised` — a pass read the file and changed a claim in it.
- `reviewed` — a pass read the file and changed nothing.
- `edited` — the file changed on that date. No one recorded what the change
  meant. Use it when the only evidence is that the file differs, such as a
  timestamp, and not that someone judged its content.

No check parses past the label. This grammar is for readers, not for
scripts.

## Fields inside `## Boundary`

`Placement test` and `Document comparison` are required, non-empty, and live
inside the `## Boundary` section after its bullets. `Document comparison` starts
with `differs` or with `deliberately matches`.

Design §2.1 puts both inside Boundary. Closure check 3 is worded against the
Boundary section. They are labelled only so check 3 can parse them. They stay
where the design put them.

## Claim bullets and the sentinels

Every bullet in Boundary, Depends On, Depended On By, and Seams either matches
its section's grammar, which always ends in `` — `path` — `Symbol` ``, or is
exactly one of these two lines:

```
- None
- none found
```

`- None` is the sentinel for an empty Boundary, Depends On, Depended On By, or
Seams section, per Design §2.2. `- none found` is the sentinel for an empty
Offshoots Filed section. A section is never left blank and never carries a bare
prose sentence.

This is what lets check 4 fail. A check that validates only the bullets already
matching the citation form cannot see a claim written without one. Check 4
requires every bullet to match or be a sentinel. It fails on anything else.

All four sections must be present in every domain file, each with at least one
bullet.

## Sections beyond the template

Every domain file carries seven sections: the four claim
sections above, plus `## How The Existing Documents Saw This` and
`## Offshoots Filed`. Those two are mandatory, not optional additions. Check 6
requires an Offshoots record from every domain file. A check that tolerates a
missing `## Offshoots Filed` is not the BR-4 accounting it is meant to be.

A file may add a section beyond those seven. One does: `audio.md` carries
`## Why transcription Is Not A Dependent`. It is there because the rule above
forbids a bare prose sentence inside a claim section. That rule leaves an
explanation of an *absence* nowhere legal to go.

So a check must locate a section by name, never by position and never by
enumerating the full heading vocabulary. `^## <name>\n(.*?)(?=^## |\Z)` is the
form the existing checks use and the form new ones should use.

## `Code` and `Owned Files`

`**Code:**` is the design's §2.1 field. It stays as the human-readable summary.
`## Owned Files` is the machine-readable list check 8 parses. It holds one exact
path per bullet, copied verbatim from `INVENTORY.txt`. No directories, no globs,
no comma-separated lists. Check 8 does set comparison. A directory is a failure
rather than a shorthand.

`Code` may say `crates/audio/src/` while `Owned Files` enumerates every file in
it. Check 8 reads only `Owned Files`. Check 6 requires `Code` non-empty but does
not parse it.

A domain that owns no files carries `- None`.

## Seams

A seam ID is `SEAM-<lowercase-kebab>`. It appears in exactly two domain files,
both naming the same owner. The owner is one of the two endpoints. The
synthesis's seam table is keyed on the ID. Check 5 joins on it.

"Appears in two files" counts the files whose `## Seams` section names the ID,
never the number of occurrences. An ID may also be named in prose elsewhere in
the same file. `audio.md` names `SEAM-opus-segment-file` twice,
`background-work.md` names `SEAM-cleanup-schedule-key` twice, and
`transcription.md` names `SEAM-transcription-wire` twice. A whole-file occurrence
count reports three for each of those and fails them. The tempting repair is
to loosen the two-file rule. That is the rule check 5 exists to enforce.

## Offshoots

Either one or more `- CHR-NNN — <one line>` bullets, or the single literal line
`- none found`. The IDs must match the register row's Offshoots cell.

## Synthesis status line

The first line of the synthesis is exactly one of these. It must match what
closure check 1 reports:

```
> **Authoritative.** The walk is complete and the closure checks pass.
> **Not authoritative.** The walk is incomplete. This synthesis covers only the register rows that have an outcome, and the closure checks have not passed.
```

The synthesis's four tables are fixed by Design §2.3.
