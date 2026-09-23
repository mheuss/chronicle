#!/usr/bin/env bash
# Falsification harness for the closure checks.
#
# Task 25 Step 4 requires each check to be broken on purpose and seen to fail.
# That evidence is what makes a green suite mean anything, and without a harness
# it lives only in whoever ran it last. This script is that evidence, runnable.
#
# Each case copies a clean tree into a fresh mktemp -d, breaks exactly one
# thing, runs one check against the copy, and tears the copy down in the same
# invocation. Nothing is ever written inside the repo. Per Standing Constraint 2.
#
# Two kinds of clean tree. Checks 2, 3 and 4 copy the live docs/domains. Checks
# 5, 6 and 8 build a small synthetic map with its own repo -- two domains, one
# seam, two source files -- small enough to read in one screen, which is the
# point: a case that breaks a two-file inventory is legible where the same case
# against 64 files is not.
#
# The synthetic fixture was originally built because SYNTHESIS.md and
# OFFSHOOTS_RESOLVED.txt did not exist yet. Both exist now, so those three could
# copy the live tree as the others do; the legibility argument is why they still
# do not.
#
# Exit 0 = every check passes clean AND every case made its check fail.
# Exit 1 = a check failed on the clean tree, or a case did not bite.
#
# Invoke as: bash falsify.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"

MAP_ABS=$(cd "$MAP_ROOT" 2>/dev/null && pwd) || { echo "falsify: MAP_ROOT unreadable: $MAP_ROOT"; exit 1; }
REPO_ABS=$(cd "$REPO_ROOT" 2>/dev/null && pwd) || { echo "falsify: REPO_ROOT unreadable: $REPO_ROOT"; exit 1; }
CHECKS="$MAP_ABS/checks"

fails=0
cases=0
noops=0
noop_labels=""

# Without this trap, a Ctrl-C between mktemp and rm -rf leaves a full copy of
# docs/domains in /tmp.
FIX=""
trap '[ -n "$FIX" ] && rm -rf "$FIX"' EXIT

# ---------------------------------------------------------------------------
# Clean case first, and it gates everything below.
#
# A check that fails on everything looks exactly like a working check until it
# blocks the close. Worse, once the live tree is red a case cannot be told from
# the baseline: "the fixture broke it" and "it was already broken" produce the
# same exit code. So a red baseline aborts rather than running the cases.
# ---------------------------------------------------------------------------
echo "== clean case =="
for f in "$CHECKS"/check[0-9]*.sh; do
    # Output is captured so the abort path can print why, not just that.
    out=$(bash "$f" "$MAP_ABS" "$REPO_ABS" 2>&1)
    rc=$?
    printf '  %-32s exit=%s\n' "$(basename "$f")" "$rc"
    # The clean case must be all-zero. Every tolerance that used to live here
    # was for a pre-close state that cannot recur now the map is committed, and
    # each one turned a deleted input into a green run.
    case "$(basename "$f"):$rc" in
        *:0) ;;
        *)
            echo "falsify: $(basename "$f") is not green on the live tree; fix that before falsifying anything"
            echo "$out"
            exit 1
            ;;
    esac
done
echo

# ---------------------------------------------------------------------------
# run <label> <check script> <shell to break the fixture>
#
# The break runs with the fixture as cwd, so it edits "domains/..." paths.
# ---------------------------------------------------------------------------
run() {
    local label="$1" script="$2" breaker="$3" want="$4"
    local rc brc
    cases=$((cases + 1))

    [ -f "$CHECKS/$script" ] || {
        printf '  %-48s %s\n' "$label" "NO SUCH CHECK: $script"
        fails=$((fails + 1))
        return
    }

    FIX=$(mktemp -d) || { echo "falsify: mktemp failed"; exit 1; }
    cp -R "$MAP_ABS" "$FIX/domains" || { echo "falsify: copy failed"; exit 1; }

    ( cd "$FIX" && eval "$breaker" ) >/dev/null 2>&1
    brc=$?
    # A breaker that itself failed leaves a fixture broken in some other way, or
    # in no way at all. Either makes the case's verdict meaningless.
    if [ "$brc" -ne 0 ]; then
        printf '  %-48s %s\n' "$label" "BREAKER FAILED (exit $brc)"
        fails=$((fails + 1))
        rm -rf "$FIX"; FIX=""
        return
    fi

    # A breaker whose pattern no longer matches edits nothing, the check
    # correctly finds nothing wrong, and the case reads as a pass. That is a
    # broken test reporting success -- it happened once while these were being
    # written. Compare before trusting the exit code.
    if diff -rq "$MAP_ABS" "$FIX/domains" >/dev/null 2>&1; then
        printf '  %-48s %s\n' "$label" "NO-OP — tested nothing"
        noops=$((noops + 1))
        noop_labels="$noop_labels|$label"
        rm -rf "$FIX"; FIX=""
        return
    fi

    bash "$CHECKS/$script" "$FIX/domains" "$REPO_ABS" >"$FIX/out" 2>&1
    rc=$?   # must be captured here; any command in between overwrites it

    if [ "$rc" -eq 0 ]; then
        printf '  %-48s %s\n' "$label" "DID NOT BITE (exit 0)"
        fails=$((fails + 1))
    elif ! grep -qF -- "$want" "$FIX/out"; then
        # Non-zero alone is not evidence. A renamed script exits 127, and a
        # fixture can trip a different rule than the one the case names -- both
        # counted as a successful falsification before this line existed.
        printf '  %-48s %s\n' "$label" "WRONG FAILURE (exit $rc, wanted: $want)"
        fails=$((fails + 1))
    else
        printf '  %-48s exit=%s  %s\n' "$label" "$rc" \
            "$(grep -m1 -F -- "$want" "$FIX/out" | cut -c1-72)"
    fi
    rm -rf "$FIX"; FIX=""
}

echo "== check 1: register grammar, merge targets and split children =="
run "Decided is not YYYY-MM-DD" check1_outcomes.sh \
    'perl -0pi -e "s/\| 1 \| capture \| confirmed \| 2026-09-16 \| maps\/capture.md \|/| 1 | capture | confirmed | 16-09-2026 | maps\/capture.md |/" domains/REGISTER.md' \
    "Decided '16-09-2026' is not YYYY-MM-DD"
run "confirmed Detail is not maps/<name>.md" check1_outcomes.sh \
    'perl -0pi -e "s/\| 1 \| capture \| confirmed \| 2026-09-16 \| maps\/capture.md \|/| 1 | capture | confirmed | 2026-09-16 | capture.md |/" domains/REGISTER.md' \
    "confirmed Detail 'capture.md' is not maps/<name>.md"
run "a row with no Roots" check1_outcomes.sh \
    'perl -0pi -e "s/\| crates\/capture\/src\/; src\/capture_runtime.rs; src\/capture_supervisor.rs \|/|  |/" domains/REGISTER.md' \
    "row 1 (capture): has no Roots"
run "an outcome outside the vocabulary" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \|/| 5 | audio-encoding | absorbed |/" domains/REGISTER.md' \
    "outcome 'absorbed' not in"
run "merged Detail is not 'into <candidate>'" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | merged | 2026-09-16 | audio |/" domains/REGISTER.md' \
    "merged Detail 'audio' is not 'into <candidate>'"
run "a merge into a candidate with no row" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | merged | 2026-09-16 | into ghost |/" domains/REGISTER.md' \
    "merges into 'ghost', which is not a register row"
run "a merge into itself" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | merged | 2026-09-16 | into audio-encoding |/" domains/REGISTER.md' \
    "merges into 'audio-encoding', whose outcome is 'merged', not confirmed"
run "a merge into a merged candidate" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | merged | 2026-09-16 | into search |/" domains/REGISTER.md' \
    "merges into 'search', whose outcome is 'merged', not confirmed"
run "dropped Detail is not 'cross-cutting: ...'" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | dropped | 2026-09-16 | x |/" domains/REGISTER.md' \
    "dropped Detail 'x' is not 'cross-cutting: <sentence>'"
run "split Detail names only one child" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | split | 2026-09-16 | into audio |/" domains/REGISTER.md' \
    "split Detail 'into audio' is not 'into <name>, <name>"
run "a split naming a child with no row" check1_outcomes.sh \
    'perl -0pi -e "s/\| 5 \| audio-encoding \| merged \| 2026-09-16 \| into audio \|/| 5 | audio-encoding | split | 2026-09-16 | into ghost1, ghost2 |/" domains/REGISTER.md' \
    "split names child 'ghost1', which is not a register row"
run "a decided row with empty Offshoots" check1_outcomes.sh \
    'perl -0pi -e "s/\| into audio \| none found \|/| into audio |  |/" domains/REGISTER.md' \
    "decided but Offshoots is empty"
run "Offshoots that are not CHR- IDs" check1_outcomes.sh \
    'perl -0pi -e "s/\| into audio \| none found \|/| into audio | CHR-x |/" domains/REGISTER.md' \
    "is not 'none found' or ', '-separated CHR- IDs"
run "a second Register heading with a trailing space" check1_outcomes.sh \
    'printf "\n## Register \n\n| 99 | smuggled | confirmed | 2026-09-23 | maps/x.md | none found | src/ | x |\n" >> domains/REGISTER.md' \
    "'## Register' appears 2 times in REGISTER.md"
run "a candidate name on two rows" check1_outcomes.sh \
    'perl -0pi -e "s/(\n\| 14 \|[^\n]*\n)/\$1| 15 | search | confirmed | 2026-09-20 | maps\/storage.md | none found | src\/ | x |\n/" domains/REGISTER.md' \
    "candidate 'search' is named on rows 10 and 15"
run "a row number used twice" check1_outcomes.sh \
    'perl -0pi -e "s/\n\| 14 \|/\n| 13 |/" domains/REGISTER.md' \
    "row number 13 is used twice"
run "a second Register heading" check1_outcomes.sh \
    'printf "\n## Register\n\n| 99 | smuggled | confirmed | 2026-09-23 | maps/x.md | none found | src/ | x |\n" >> domains/REGISTER.md' \
    "'## Register' appears 2 times in REGISTER.md"
run "an appended row with no Symbols" check1_outcomes.sh \
    'perl -0pi -e "s/(\n\| 14 \|[^\n]*\n)/\$1| 15 | ghost | confirmed | 2026-09-20 | maps\/ghost.md | none found | src\/ |  |\n/" domains/REGISTER.md' \
    "row 15 (ghost): appended row has no Symbols"
run "a blocked row naming no register row" check1_outcomes.sh \
    'perl -0pi -e "s/(\| # \| Candidate \| Waiting on \| Since \| Offshoots \|\n\|---\|---\|---\|---\|---\|\n)/\$1| 9 | ghost | audio | 2026-09-20 | none found |\n/" domains/REGISTER.md' \
    "blocked row 'ghost' is not a register row"

echo "== check 2: domain files and confirmed rows correspond =="
run "delete a confirmed domain's file" check2_domain_files.sh \
    'rm domains/maps/power.md' \
    'row 6 (power): confirmed, but no domain file at maps/power.md'
run "orphan .md in maps/" check2_domain_files.sh \
    'echo x > domains/maps/ghost.md' \
    'maps/ghost.md: no confirmed row names it'
run "a dropped row given a domain file" check2_domain_files.sh \
    'perl -0pi -e "s/\| 13 \| ipc-compat \| merged \| 2026-09-18 \| into IPC \|/| 13 | ipc-compat | dropped | 2026-09-18 | cross-cutting: x |/" domains/REGISTER.md; echo x > domains/maps/ipc-compat.md' \
    "row 13 (ipc-compat): outcome 'dropped'"
run "non-.md file in maps/" check2_domain_files.sh \
    'echo x > domains/maps/notes.txt' \
    'maps/notes.txt: only domain files live in maps/'

echo
echo "== check 3: placement test and document comparison, inside Boundary =="
run "empty Placement test" check3_placement_tests.sh \
    'perl -0pi -e "s/\*\*Placement test:\*\*[^\n]*/**Placement test:**/" domains/maps/power.md' \
    'power.md: **Placement test:** is present but empty'
run "Document comparison with neither keyword" check3_placement_tests.sh \
    'perl -0pi -e "s/\*\*Document comparison:\*\* (differs|deliberately matches)/**Document comparison:** roughly agrees/" domains/maps/ocr.md' \
    'ocr.md: **Document comparison:** starts with neither'
run "Placement test moved outside Boundary" check3_placement_tests.sh \
    'perl -0pi -e "s/(\*\*Placement test:\*\*[^\n]*\n)//; s/(^## Owned Files)/\$1\n\n**Placement test:** moved\n/m" domains/maps/storage.md' \
    'storage.md: **Placement test:** is present but outside the Boundary section'

echo
echo "== check 4: sections, bullet grammar, and citations that resolve =="
run "symbol absent from the cited file" check4_citations.sh \
    'perl -0pi -e "s/— \`PowerEvent\`/— \`NoSuchSymbol\`/" domains/maps/power.md' \
    'symbol `NoSuchSymbol` not found'
run "cited path does not exist" check4_citations.sh \
    'perl -0pi -e "s|src/power.rs\` — \`spawn_power_observer|src/nosuch.rs\` — \`spawn_power_observer|" domains/maps/power.md' \
    'cited path does not exist: chronicle-daemon/src/nosuch.rs'
run "bullet with no citation" check4_citations.sh \
    'perl -0pi -e "s/^(## Boundary\n\n)/\$1- inside: uncited claim\n/m" domains/maps/ocr.md' \
    "bullet has no citation: '- inside: uncited claim'"
run "indented bullet with no citation" check4_citations.sh \
    'perl -0pi -e "s/^(## Boundary\n\n)/\$1  - inside: uncited indented claim\n/m" domains/maps/ocr.md' \
    "bullet has no citation: '  - inside: uncited indented claim'"
run "bare prose in a claim section" check4_citations.sh \
    'perl -0pi -e "s/^(## Boundary\n\n)/\$1This domain also owns scheduling, honest.\n/m" domains/maps/ocr.md' \
    "bare prose, not a bullet: 'This domain also owns scheduling, honest.'"
run "missing ## Seams section" check4_citations.sh \
    'perl -0pi -e "s/^## Seams\n.*?(?=^## )//sm" domains/maps/power.md' \
    'power.md: no ## Seams section'
run "Depends On present but empty" check4_citations.sh \
    'perl -0pi -e "s/^(## Depends On\n).*?(?=^## )/\$1\n/sm" domains/maps/ocr.md' \
    'ocr.md [Depends On]: section present but carries no bullet'

SYNTH_REF=""
trap '[ -n "$FIX" ] && rm -rf "$FIX"; [ -n "$SYNTH_REF" ] && rm -rf "$SYNTH_REF"' EXIT

synth() {
    local d="$1"
    mkdir -p "$d/domains/maps" "$d/repo/chronicle-daemon/src" \
             "$d/repo/chronicle-ui/Sources/ChronicleUI" \
             "$d/repo/chronicle-daemon/crates/storage/src/migrations" || return 1
    echo 'pub struct Alpha;' > "$d/repo/chronicle-daemon/src/alpha.rs"
    echo 'struct Beta {}'    > "$d/repo/chronicle-ui/Sources/ChronicleUI/Beta.swift"

    cat > "$d/domains/INVENTORY.txt" <<'EOF'
chronicle-daemon/src/alpha.rs
chronicle-ui/Sources/ChronicleUI/Beta.swift
EOF

    cat > "$d/domains/REGISTER.md" <<'EOF'
# Candidate Register

## Register

| # | Candidate | Outcome | Decided | Detail | Offshoots | Roots | Symbols |
|---|---|---|---|---|---|---|---|
| 1 | alpha | confirmed | 2026-09-21 | maps/alpha.md | CHR-901 | src/alpha.rs | rust: Alpha |
| 2 | beta | confirmed | 2026-09-21 | maps/beta.md | none found | Sources/ChronicleUI/Beta.swift | swift: Beta |
| 3 | gamma | merged | 2026-09-21 | into alpha | none found | src/alpha.rs | rust: Alpha |

## Blocked rows

| # | Candidate | Waiting on | Since | Offshoots |
|---|---|---|---|---|
EOF

    cat > "$d/domains/maps/alpha.md" <<'EOF'
# Domain: alpha

**Status:** Decided 2026-09-21
**Owns:** The alpha side.
**Code:** `chronicle-daemon/src/`

## Boundary

- inside: the alpha type — `chronicle-daemon/src/alpha.rs` — `Alpha`

**Placement test:** Is it alpha?
**Document comparison:** differs — nothing describes it.

## Owned Files

- chronicle-daemon/src/alpha.rs

## Depends On

- None

## Depended On By

- beta — `chronicle-daemon/src/alpha.rs` — `Alpha`

## Seams

- SEAM-ab — beta — alpha decides the shape and beta follows — owner: alpha — `chronicle-daemon/src/alpha.rs` — `Alpha`

## Offshoots Filed

- CHR-901 — the alpha offshoot
EOF

    cat > "$d/domains/maps/beta.md" <<'EOF'
# Domain: beta

**Status:** Decided 2026-09-21
**Owns:** The beta side.
**Code:** `chronicle-ui/Sources/ChronicleUI/`

## Boundary

- inside: the beta type — `chronicle-ui/Sources/ChronicleUI/Beta.swift` — `Beta`

**Placement test:** Is it beta?
**Document comparison:** differs — nothing describes it.

## Owned Files

- chronicle-ui/Sources/ChronicleUI/Beta.swift

## Depends On

- alpha — `chronicle-daemon/src/alpha.rs` — `Alpha`

## Depended On By

- None

## Seams

- SEAM-ab — alpha — alpha decides the shape and this domain follows — owner: alpha — `chronicle-ui/Sources/ChronicleUI/Beta.swift` — `Beta`

## Offshoots Filed

- none found
EOF

    cat > "$d/domains/SYNTHESIS.md" <<'EOF'
# Chronicle Domain Map

> **Authoritative.** The walk is complete and the closure checks pass.

## Domains

| Domain | Owns | File |
|---|---|---|
| alpha | The alpha side. | maps/alpha.md |
| beta | The beta side. | maps/beta.md |

## Seams

| ID | Endpoint A | Endpoint B | Contract | Owner | Evidence |
|---|---|---|---|---|---|
| SEAM-ab | alpha | beta | alpha decides the shape | alpha | `Alpha` |

## Candidate Outcomes

| Candidate | Outcome | Where it ended up |
|---|---|---|
| alpha | confirmed | maps/alpha.md |
| beta | confirmed | maps/beta.md |
| gamma | merged | into alpha |

## Exclusions

| Path | Reason |
|---|---|
EOF

    printf 'CHR-901 resolved label=needs-domain\n' > "$d/domains/OFFSHOOTS_RESOLVED.txt"

    # check5 shells out to check1 next to itself, so the fixture needs a checks/
    # directory rather than borrowing the live one -- otherwise check5 would
    # read the live register while judging the fixture's synthesis.
    mkdir -p "$d/domains/checks"
    cp "$CHECKS"/check*.sh "$d/domains/checks/" || return 1
    return 0
}

# run_synth <label> <check script> <breaker> <wanted text> [exact exit code]
#
# The breaker runs with the fixture root as cwd, so it edits "domains/..." and
# "repo/..." paths. Passing an exact code is how the check 6 exit-2 case asserts
# 2 rather than merely non-zero.
run_synth() {
    local label="$1" script="$2" breaker="$3" want="$4" exact="${5:-}"
    local rc brc
    cases=$((cases + 1))

    [ -f "$SYNTH_REF/domains/checks/$script" ] || {
        printf '  %-48s %s\n' "$label" "NO SUCH CHECK: $script"
        fails=$((fails + 1))
        return
    }

    FIX=$(mktemp -d) || { echo "falsify: mktemp failed"; exit 1; }
    cp -R "$SYNTH_REF/." "$FIX/" || { echo "falsify: synth copy failed"; exit 1; }

    ( cd "$FIX" && eval "$breaker" ) >/dev/null 2>&1
    brc=$?
    if [ "$brc" -ne 0 ]; then
        printf '  %-48s %s\n' "$label" "BREAKER FAILED (exit $brc)"
        fails=$((fails + 1))
        rm -rf "$FIX"; FIX=""
        return
    fi

    if diff -rq "$SYNTH_REF" "$FIX" >/dev/null 2>&1; then
        printf '  %-48s %s\n' "$label" "NO-OP — tested nothing"
        noops=$((noops + 1))
        noop_labels="$noop_labels|$label"
        rm -rf "$FIX"; FIX=""
        return
    fi

    bash "$FIX/domains/checks/$script" "$FIX/domains" "$FIX/repo" >"$FIX/out" 2>&1
    rc=$?

    if [ -n "$exact" ] && [ "$rc" -ne "$exact" ]; then
        printf '  %-48s %s\n' "$label" "WRONG EXIT (got $rc, wanted exactly $exact)"
        fails=$((fails + 1))
    elif [ -z "$exact" ] && [ "$rc" -eq 0 ]; then
        printf '  %-48s %s\n' "$label" "DID NOT BITE (exit 0)"
        fails=$((fails + 1))
    elif ! grep -qF -- "$want" "$FIX/out"; then
        printf '  %-48s %s\n' "$label" "WRONG FAILURE (exit $rc, wanted: $want)"
        fails=$((fails + 1))
    else
        printf '  %-48s exit=%s  %s\n' "$label" "$rc" \
            "$(grep -m1 -F -- "$want" "$FIX/out" | cut -c1-72)"
    fi
    rm -rf "$FIX"; FIX=""
}

echo
echo "== synthetic fixture: clean case, and it gates checks 5, 6 and 8 =="
SYNTH_REF=$(mktemp -d) || { echo "falsify: mktemp failed"; exit 1; }
synth "$SYNTH_REF" || { echo "falsify: could not build the synthetic fixture"; exit 1; }
for s in check5_synthesis check6_offshoots check8_coverage; do
    bash "$SYNTH_REF/domains/checks/$s.sh" "$SYNTH_REF/domains" "$SYNTH_REF/repo" >/dev/null 2>&1
    rc=$?
    printf '  %-32s exit=%s\n' "$s.sh" "$rc"
    if [ "$rc" -ne 0 ]; then
        echo "falsify: $s.sh is not green on the synthetic fixture; the cases below would be meaningless"
        exit 1
    fi
done

echo
echo "== check 5: the synthesis agrees with the files and the register =="
run_synth "a seam with two owners" check5_synthesis.sh \
    'perl -0pi -e "s/owner: alpha —/owner: beta —/" domains/maps/beta.md' \
    'SEAM-ab names 2 owners across its files: alpha, beta'
run_synth "seam in a domain file, absent from the synthesis" check5_synthesis.sh \
    'perl -0pi -e "s/^\| SEAM-ab \|.*\n//m" domains/SYNTHESIS.md' \
    'SEAM-ab is in the domain files but missing from the synthesis Seams table'
run_synth "seam named in three domain files" check5_synthesis.sh \
    'perl -0pi -e "s/\| 3 \| gamma \| merged \| 2026-09-21 \| into alpha \|/| 3 | gamma | confirmed | 2026-09-21 | maps\/gamma.md |/" domains/REGISTER.md;
     sed -e "s/^# Domain: alpha/# Domain: gamma/" -e "s/^\*\*Owns:\*\* The alpha side./**Owns:** The gamma side./" domains/maps/alpha.md > domains/maps/gamma.md;
     perl -0pi -e "s/^- CHR-901 — the alpha offshoot$/- none found/m" domains/maps/gamma.md' \
    "SEAM-ab is named in 3 domain files' ## Seams sections"
run_synth "owner that is not one of the endpoints" check5_synthesis.sh \
    'perl -0pi -e "s/owner: alpha —/owner: delta —/g" domains/maps/alpha.md domains/maps/beta.md;
     perl -0pi -e "s/\| alpha \| \`Alpha\` \|/| delta | \`Alpha\` |/" domains/SYNTHESIS.md' \
    "SEAM-ab: owner 'delta' is not one of its endpoints (alpha, beta)"
run_synth "authoritative while check1 reports incomplete" check5_synthesis.sh \
    'perl -0pi -e "s/\| 2 \| beta \| confirmed \|/| 2 | beta |  |/" domains/REGISTER.md' \
    'status line says the walk is complete, but check1 reports incomplete'
run_synth "candidate outcome disagrees with the register" check5_synthesis.sh \
    'perl -0pi -e "s/\| gamma \| merged \| into alpha \|/| gamma | confirmed | into alpha |/" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes row 'gamma' says 'confirmed', register says 'merged'"
run_synth "merge destination disagrees with the register" check5_synthesis.sh \
    'perl -0pi -e "s/\| gamma \| merged \| into alpha \|/| gamma | merged | into beta |/" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes row 'gamma' says 'into beta', register says 'into alpha'"
run_synth "confirmed destination names the wrong file" check5_synthesis.sh \
    'perl -0pi -e "s/\| alpha \| confirmed \| maps\/alpha.md \|/| alpha | confirmed | maps\/beta.md |/" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes row 'alpha' names file 'maps/beta.md', register says 'maps/alpha.md'"
run_synth "duplicate candidate row hides a wrong one" check5_synthesis.sh \
    'perl -0pi -e "s/^\| gamma \| merged \| into alpha \|$/| gamma | confirmed | maps\/alpha.md |\n| gamma | merged | into alpha |/m" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes names 'gamma' twice"
run_synth "dropped destination disagrees with the register" check5_synthesis.sh \
    'perl -0pi -e "s/\| 3 \| gamma \| merged \| 2026-09-21 \| into alpha \|/| 3 | gamma | dropped | 2026-09-21 | cross-cutting: alpha handles it |/" domains/REGISTER.md;
     perl -0pi -e "s/\| gamma \| merged \| into alpha \|/| gamma | dropped | into beta |/" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes row 'gamma' says 'into beta', register says 'cross-cutting: alpha handles it'"
run_synth "a second Seams heading in a domain file" check5_synthesis.sh \
    'printf "\n## Seams\n\n- SEAM-zz — alpha — smuggled — owner: alpha — \`x\` — \`X\`\n" >> domains/maps/beta.md' \
    "'## Seams' appears 2 times in beta.md"
run_synth "undecided row given an outcome in the synthesis" check5_synthesis.sh \
    'perl -0pi -e "s/\| 3 \| gamma \| merged \|/| 3 | gamma |  |/" domains/REGISTER.md;
     perl -0pi -e "s/^> \*\*Authoritative\.\*\* The walk is complete and the closure checks pass\.$/> **Not authoritative.** The walk is incomplete. This synthesis covers only the register rows that have an outcome, and the closure checks have not passed./m" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes row 'gamma' says 'merged', register has no outcome"
run_synth "merge destination with a second target appended" check5_synthesis.sh \
    'perl -0pi -e "s/\| gamma \| merged \| into alpha \|/| gamma | merged | into alpha and beta |/" domains/SYNTHESIS.md' \
    "synthesis Candidate Outcomes row 'gamma' says 'into alpha and beta', register says 'into alpha'"

echo
echo "== check 6: offshoots accounted for locally and in Linear =="
run_synth "blank Offshoots cell" check6_offshoots.sh \
    'perl -0pi -e "s/\| maps\/alpha.md \| CHR-901 \|/| maps\/alpha.md |  |/" domains/REGISTER.md' \
    'row 1 (alpha): Offshoots cell is empty'
run_synth "malformed offshoot ID in the register" check6_offshoots.sh \
    'perl -0pi -e "s/\| CHR-901 \|/| CHR901 |/" domains/REGISTER.md' \
    "row 1 (alpha): Offshoots 'CHR901' is not 'none found'"
run_synth "register ID absent from the resolved file" check6_offshoots.sh \
    ': > domains/OFFSHOOTS_RESOLVED.txt' \
    'CHR-901 is in the register but not in OFFSHOOTS_RESOLVED.txt'
run_synth "resolved line missing the label" check6_offshoots.sh \
    'printf "CHR-901 resolved\n" > domains/OFFSHOOTS_RESOLVED.txt' \
    "is not 'CHR-NNN resolved label=needs-domain'"
run_synth "domain file disagrees with its register row" check6_offshoots.sh \
    'perl -0pi -e "s/- CHR-901 — the alpha offshoot/- CHR-902 — the wrong offshoot/" domains/maps/alpha.md' \
    'alpha.md: Offshoots Filed disagrees with row 1'
run_synth "empty Owns field" check6_offshoots.sh \
    'perl -0pi -e "s/^\*\*Owns:\*\* The alpha side\.$/**Owns:**/m" domains/maps/alpha.md' \
    'alpha.md: **Owns:** is missing or empty'
run_synth "resolved file removed entirely must exit 2" check6_offshoots.sh \
    'rm domains/OFFSHOOTS_RESOLVED.txt' \
    'OFFSHOOTS_RESOLVED.txt is absent, so Linear is unverified' 2
run_synth "malformed Offshoots bullet is not silently skipped" check6_offshoots.sh \
    'perl -0pi -e "s/^- CHR-901 — the alpha offshoot$/- CHR-901 — the alpha offshoot\n-CHR-999 — smuggled in with no space/m" domains/maps/alpha.md' \
    "alpha.md [Offshoots Filed]: bare prose, not a bullet: '-CHR-999 — smuggled in with no space'"
run_synth "sentinel alongside a real offshoot bullet" check6_offshoots.sh \
    'perl -0pi -e "s/^- CHR-901 — the alpha offshoot$/- CHR-901 — the alpha offshoot\n- none found/m" domains/maps/alpha.md' \
    "alpha.md [Offshoots Filed]: '- none found' must be the only bullet"
run_synth "a second Offshoots Filed heading" check6_offshoots.sh \
    'printf "\n## Offshoots Filed\n\n- CHR-999 — smuggled in a second section\n" >> domains/maps/beta.md' \
    "'## Offshoots Filed' appears 2 times in beta.md"
# The bare-prose guard catches a line that is not a bullet. This is the other
# half: a well-formed bullet whose ID is the wrong shape.
# The branch CHR-155 needed and never reached, because the close procedure built
# the resolved file from the register. It is live; only the input rule was dead.
run_synth "resolved ID that no register row names" check6_offshoots.sh \
    'printf "CHR-999 resolved label=needs-domain\n" >> domains/OFFSHOOTS_RESOLVED.txt' \
    'CHR-999 is in OFFSHOOTS_RESOLVED.txt but no register row names it'
run_synth "the same ID listed twice in the resolved file" check6_offshoots.sh \
    'printf "CHR-901 resolved label=needs-domain\n" >> domains/OFFSHOOTS_RESOLVED.txt' \
    'CHR-901 is listed twice'
run_synth "wrongly shaped offshoot ID in a domain file" check6_offshoots.sh \
    'perl -0pi -e "s/^- CHR-901 — the alpha offshoot$/- CHR902 — wrong ID shape/m" domains/maps/alpha.md' \
    "alpha.md [Offshoots Filed]: bullet is not '- CHR-NNN — <text>'"

echo
echo "== check 8: every source file owned once, or excluded with a reason =="
run_synth "a path owned by two domains" check8_coverage.sh \
    'perl -0pi -e "s|^- chronicle-ui/Sources/ChronicleUI/Beta.swift$|- chronicle-ui/Sources/ChronicleUI/Beta.swift\n- chronicle-daemon/src/alpha.rs|m" domains/maps/beta.md' \
    'chronicle-daemon/src/alpha.rs is owned by 2 domains: alpha, beta'
run_synth "a second Owned Files heading" check8_coverage.sh \
    'printf "\n## Owned Files\n\n- chronicle-daemon/src/alpha.rs\n" >> domains/maps/beta.md' \
    "'## Owned Files' appears 2 times in beta.md"
run_synth "a second Owned Files heading with closing hashes" check8_coverage.sh \
    'printf "\n##  Owned Files ##\n\n- chronicle-daemon/src/alpha.rs\n" >> domains/maps/beta.md' \
    "'## Owned Files' appears 2 times in beta.md"
run_synth "a path both owned and excluded" check8_coverage.sh \
    'printf "| chronicle-daemon/src/alpha.rs | not really |\n" >> domains/SYNTHESIS.md' \
    'chronicle-daemon/src/alpha.rs is both owned and excluded'
run_synth "an Exclusions row with a blank reason" check8_coverage.sh \
    'perl -0pi -e "s/^- chronicle-daemon\/src\/alpha.rs$//m" domains/maps/alpha.md;
     printf "| chronicle-daemon/src/alpha.rs |  |\n" >> domains/SYNTHESIS.md' \
    "synthesis Exclusions row 'chronicle-daemon/src/alpha.rs' has an empty Reason cell"
run_synth "an Owned Files bullet naming a directory" check8_coverage.sh \
    'perl -0pi -e "s|^- chronicle-daemon/src/alpha.rs$|- chronicle-daemon/src/|m" domains/maps/alpha.md' \
    "is a directory or glob; one exact inventory path per bullet"
run_synth "an extra .rs file appears in the repo" check8_coverage.sh \
    'echo "pub struct Late;" > repo/chronicle-daemon/src/late.rs' \
    'chronicle-daemon/src/late.rs is a production source file and is not in INVENTORY.txt'
# The vacuum case. Emptied on both sides, every assertion in check 8 is a loop
# over an empty set and "every source file resolves to one domain" is vacuously
# true. It exited 0 before the guard landed.
# Two guards, two cases. Emptying both sides only ever proves the first, because
# it fires before the second is reached.
run_synth "no source files under the repo root" check8_coverage.sh \
    ': > domains/INVENTORY.txt;
     rm repo/chronicle-daemon/src/alpha.rs repo/chronicle-ui/Sources/ChronicleUI/Beta.swift;
     perl -0pi -e "s/^- chronicle-daemon\/src\/alpha.rs$/- None/m" domains/maps/alpha.md;
     perl -0pi -e "s/^- chronicle-ui\/Sources\/ChronicleUI\/Beta.swift$/- None/m" domains/maps/beta.md' \
    'check8: the globs found zero source files under the repo root'
run_synth "an empty INVENTORY.txt must not pass vacuously" check8_coverage.sh \
    ': > domains/INVENTORY.txt' \
    'check8: INVENTORY.txt is empty'
# A malformed Owned Files bullet used to be dropped by the leading-dash filter
# before the grammar saw it, hiding the double-ownership violation underneath.
run_synth "malformed Owned Files bullet is not silently skipped" check8_coverage.sh \
    'perl -0pi -e "s|^- chronicle-daemon/src/alpha.rs$|- chronicle-daemon/src/alpha.rs\n-chronicle-ui/Sources/ChronicleUI/Beta.swift|m" domains/maps/alpha.md' \
    "alpha.md [Owned Files]: bare prose, not a bullet: '-chronicle-ui/Sources/ChronicleUI/Beta.swift'"
# The four globs name three roots. These two cases are the reason the sweep
# after them exists: both files ship, and both were invisible before it.
run_synth "a source file outside every known root" check8_coverage.sh \
    'mkdir -p repo/extra && echo "fn main(){}" > repo/extra/stray.rs' \
    'check8: source files outside every known root'
run_synth "a .swift beside Sources rather than inside it" check8_coverage.sh \
    'mkdir -p repo/chronicle-ui/Widgets && echo "struct X {}" > repo/chronicle-ui/Widgets/Stray.swift' \
    'check8: source files outside every known root'
# Four siblings report a short register row by name. check8 did not, and read the
# missing cells as a domain that owns nothing.
run_synth "a truncated register row is named, not inferred" check8_coverage.sh \
    'perl -0pi -e "s/^(\| 1 \| alpha \| confirmed \|)[^\n]*$/\$1/m" domains/REGISTER.md' \
    'cells, expected 8'


echo
echo "== the shared register parser: checks 1, 2, 5, 6 and 8 agree =="
# The reconcile's own case. Before the five checks agreed on one register-row
# parser, three silently dropped this row and two counted it as a candidate.
run_synth "a non-numeric # cell is reported, not skipped" check6_offshoots.sh \
    'perl -0pi -e "s/^\\| 1 \\| alpha \\|/| one | alpha |/m" domains/REGISTER.md' \
    "register row has a non-numeric # cell"

echo
echo "== control: a deliberate no-op must report NO-OP, not pass =="
# Proves the guard above is live. Its `want` is deliberately unreachable: if this
# case ever runs a check instead of reporting NO-OP, it must fail loudly.
run "no-op breaker" check4_citations.sh 'true' \
    'unreachable — this case must report NO-OP'

echo
bit=$((cases - fails - noops))
echo "falsify: $cases cases, $bit bit, $fails failed, $noops no-op"

# Exactly one no-op is expected, and it must be the control. Counting no-ops in
# their own variable rather than folding them into `fails` and subtracting a
# constant means a case that quietly stops matching is reported as itself,
# instead of inviting someone to bump the constant.
if [ "$noops" -ne 1 ] || [ "$noop_labels" != "|no-op breaker" ]; then
    echo "falsify: expected exactly one no-op, the control; got $noops:$noop_labels"
    exit 1
fi
if [ "$fails" -ne 0 ]; then
    echo "falsify: a case did not bite, or bit for the wrong reason — a check is not testing what it claims to"
    exit 1
fi
exit 0
