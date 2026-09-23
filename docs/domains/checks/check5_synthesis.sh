#!/usr/bin/env bash
# Closure check 5: the synthesis names every domain, every seam and every
# candidate, and agrees with the domain files about all of them.
# Per Design section 5 check 5, BR-1 and BR-6.
#
# The synthesis is the entry point to the map. A reader who opens it and stops
# there must not be told something the domain files deny, so every join below
# runs in both directions: nothing in the files is missing from the synthesis,
# and nothing in the synthesis is absent from the files.
#
# Exit 0 = the synthesis is present and agrees with the files and the register.
# Exit 1 = the synthesis is absent, malformed, or disagrees; or zero domain
#          files were examined.
#
# Invoke as: bash check5_synthesis.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"
REG="$MAP_ROOT/REGISTER.md"
SYN="$MAP_ROOT/SYNTHESIS.md"
CHECKS="$(cd "$(dirname "$0")" && pwd)"

[ -f "$REG" ] || { echo "check5: no register at $REG"; exit 1; }
[ -d "$MAP_ROOT/maps" ] || { echo "check5: no maps/ directory under $MAP_ROOT"; exit 1; }

# Task 28 Step 1 runs this check before writing the synthesis and requires a
# real failure there rather than a vacuous pass, so absence is decided here,
# before anything else is parsed.
[ -f "$SYN" ] || { echo "check5: no synthesis at $SYN"; exit 1; }

# The status line has to match what check 1 reports, so ask check 1 rather than
# re-deriving completeness from the register. Two implementations of the same
# rule is how they come to disagree.
#
# check1 documents 0 as complete and 3 as grammar-passes-but-incomplete. Any
# other code means check1 itself is unhappy, and its answer cannot be used.
bash "$CHECKS/check1_outcomes.sh" "$MAP_ROOT" "$REPO_ROOT" >/dev/null 2>&1
CHECK1_RC=$?
case "$CHECK1_RC" in
    0|3) ;;
    *) echo "check5: check1_outcomes.sh exited $CHECK1_RC, so the expected status line is unknown"; exit 1 ;;
esac

python3 - "$MAP_ROOT" "$CHECK1_RC" <<'PY'
import re, sys, os

map_root, check1_rc = sys.argv[1], int(sys.argv[2])
reg_path = os.path.join(map_root, 'REGISTER.md')
syn_path = os.path.join(map_root, 'SYNTHESIS.md')

fail = []

def section(text, name, where):
    pat = r'^## ' + re.escape(name) + r'\n(.*?)(?=^## |\Z)'
    n = len(re.findall(r'^##[ \t]+' + re.escape(name) + r'[ \t]*#*[ \t]*$', text, re.M))
    msg = f"'## {name}' appears {n} times in {where}; only the first would be read"
    if n > 1 and msg not in fail:
        fail.append(msg)
    m = re.search(pat, text, re.S | re.M)
    return m.group(1) if m else None

def register_rows(block, fail):
    """Body rows of a register table, as lists of cells.

    A row whose leading cell is neither the header nor a positive integer is
    reported, never skipped. Five checks parse this table. Three of them once
    read such a row differently from the other two.
    """
    rows = []
    for line in (block or '').split('\n'):
        s = line.strip()
        if not s.startswith('|'):
            continue
        cells = [c.strip() for c in s.strip('|').split('|')]
        if all(re.fullmatch(r':?-{2,}:?', c) for c in cells):
            continue
        if cells[0] == '#':
            continue
        if not re.fullmatch(r'\d+', cells[0]):
            fail.append(f"register row has a non-numeric # cell: {s[:64]!r}")
            continue
        rows.append(cells)
    return rows

def table_rows(block):
    """Body rows of a markdown table: pipe-led, minus header and separator."""
    out = []
    for line in (block or '').split('\n'):
        s = line.strip()
        if not s.startswith('|'):
            continue
        cells = [c.strip() for c in s.strip('|').split('|')]
        if all(re.fullmatch(r':?-{2,}:?', c) for c in cells):
            continue
        out.append(cells)
    return out[1:] if out else []

# ---------------------------------------------------------------------------
# Register: the authority on which candidates exist and which are confirmed.
# ---------------------------------------------------------------------------
reg_text = open(reg_path, encoding='utf-8').read()
reg_block = section(reg_text, 'Register', 'REGISTER.md')
if reg_block is None:
    print("check5: no ## Register section in REGISTER.md")
    sys.exit(1)

candidates = []          # every candidate name, in register order
confirmed = {}           # name -> detail path
reg_outcomes = {}        # name -> (outcome, detail)
for cells in register_rows(reg_block, fail):
    if len(cells) < 8:
        fail.append(f"row {cells[0]}: has {len(cells)} cells, expected 8")
        continue
    num, name, outcome, _decided, detail = cells[:5]
    candidates.append(name)
    reg_outcomes[name] = (outcome, detail)
    if outcome == 'confirmed':
        confirmed[name] = detail

if not candidates:
    print("check5: examined zero register rows")
    sys.exit(1)
if not confirmed:
    print("check5: register holds no confirmed rows")
    sys.exit(1)

# ---------------------------------------------------------------------------
# Domain files: seams as the files declare them.
#
# FORMAT.md: "Appears in two files" counts the files whose ## Seams section
# names the ID, never the number of occurrences. Three files name a seam ID a
# second time in prose outside ## Seams, so a whole-file scan reports three and
# fails a correct tree.
# ---------------------------------------------------------------------------
SEAM_BULLET = re.compile(
    r'^-\s+(SEAM-[a-z0-9-]+)\s+—\s+(.+?)\s+—\s+.*?owner:\s*([A-Za-z0-9_-]+)\s+—\s+`[^`]+`\s+—\s+`[^`]+`\s*$'
)

file_seams = {}   # seam id -> list of (domain name, partner named, owner)
for name, detail in sorted(confirmed.items()):
    path = os.path.join(map_root, detail)
    if not os.path.isfile(path):
        fail.append(f"{name}: no domain file at {detail} (check2 owns this; check5 cannot join without it)")
        continue
    body = section(open(path, encoding='utf-8').read(), 'Seams', os.path.basename(detail))
    if body is None:
        fail.append(f"{os.path.basename(detail)}: no ## Seams section")
        continue
    for line in body.split('\n'):
        s = line.strip()
        if not s.startswith('- ') or s == '- None':
            continue
        m = SEAM_BULLET.match(s)
        if not m:
            fail.append(f"{os.path.basename(detail)} [Seams]: bullet does not match the seam grammar: {s[:64]!r}")
            continue
        sid, partner, owner = m.group(1), m.group(2).strip(), m.group(3)
        file_seams.setdefault(sid, []).append((name, partner, owner))

# ---------------------------------------------------------------------------
# Synthesis.
# ---------------------------------------------------------------------------
syn_text = open(syn_path, encoding='utf-8').read()

# BR-6: an incomplete walk claims no authority. The two lines are fixed by
# FORMAT.md; matching a prefix rather than the whole line would let a file say
# "Authoritative." and then take it back in the same sentence.
AUTHORITATIVE = "> **Authoritative.** The walk is complete and the closure checks pass."
NOT_AUTHORITATIVE = (
    "> **Not authoritative.** The walk is incomplete. This synthesis covers only "
    "the register rows that have an outcome, and the closure checks have not passed."
)
want = AUTHORITATIVE if check1_rc == 0 else NOT_AUTHORITATIVE
other = NOT_AUTHORITATIVE if check1_rc == 0 else AUTHORITATIVE
state = "complete" if check1_rc == 0 else "incomplete"

syn_lines = [l.rstrip() for l in syn_text.split('\n')]
if want in syn_lines and other in syn_lines:
    # Carrying both lines is not a pass. The within-line protection above stops
    # a sentence taking itself back; this stops the document doing it two lines
    # apart.
    fail.append("synthesis carries both status lines; exactly one must be present")
elif want not in syn_lines:
    if other in syn_lines:
        fail.append(f"status line says the walk is {'incomplete' if check1_rc == 0 else 'complete'}, but check1 reports {state}")
    else:
        fail.append(f"no status line; check1 reports {state}, so it must be exactly: {want}")

for table in ('Domains', 'Seams', 'Candidate Outcomes', 'Exclusions'):
    if section(syn_text, table, 'SYNTHESIS.md') is None:
        fail.append(f"synthesis has no ## {table} section")

def file_cell(raw):
    """The path a File cell points at, whatever dress it is wearing.

    The rule worth enforcing is that the cell points at the register's file.
    Which of `maps/x.md`, maps/x.md or [x](maps/x.md) Task 28 happens to write
    is a formatting choice, and failing the close over one would be a rule
    nobody agreed to."""
    s = raw.strip()
    m = re.fullmatch(r'\[[^\]]*\]\(([^)]+)\)', s)
    if m:
        s = m.group(1)
    return s.strip().strip('`').strip()

syn_domains = {}     # domain -> file cell, normalized
for cells in table_rows(section(syn_text, 'Domains', 'SYNTHESIS.md')):
    if len(cells) < 3:
        fail.append(f"synthesis Domains row has {len(cells)} cells, expected 3: {cells}")
        continue
    domain, owns, dfile = cells[0], cells[1], file_cell(cells[2])
    if domain in syn_domains:
        fail.append(f"synthesis Domains names '{domain}' twice")
    syn_domains[domain] = dfile
    if not owns:
        fail.append(f"synthesis Domains row '{domain}' has an empty Owns cell")

syn_seams = {}       # id -> (a, b, owner)
for cells in table_rows(section(syn_text, 'Seams', 'SYNTHESIS.md')):
    if len(cells) < 6:
        fail.append(f"synthesis Seams row has {len(cells)} cells, expected 6: {cells}")
        continue
    sid, a, b, contract, owner, evidence = cells[:6]
    if sid in syn_seams:
        fail.append(f"synthesis Seams names '{sid}' twice")
    if not contract:
        fail.append(f"synthesis Seams row '{sid}' has an empty Contract cell")
    if not evidence:
        fail.append(f"synthesis Seams row '{sid}' has an empty Evidence cell")
    syn_seams[sid] = (a, b, owner)

syn_candidates = {}      # name -> (outcome cell, destination cell)
for cells in table_rows(section(syn_text, 'Candidate Outcomes', 'SYNTHESIS.md')):
    if len(cells) < 3:
        fail.append(f"synthesis Candidate Outcomes row has {len(cells)} cells, expected 3: {cells}")
        continue
    if cells[0] in syn_candidates:
        fail.append(f"synthesis Candidate Outcomes names '{cells[0]}' twice")
    syn_candidates[cells[0]] = (cells[1], cells[2])

# ---------------------------------------------------------------------------
# Joins, both directions.
# ---------------------------------------------------------------------------
for name in sorted(confirmed):
    if name not in syn_domains:
        fail.append(f"confirmed domain '{name}' is missing from the synthesis Domains table")
    elif syn_domains[name] != confirmed[name]:
        fail.append(f"synthesis Domains row '{name}' names file '{syn_domains[name]}', register says '{confirmed[name]}'")
for name in sorted(syn_domains):
    if name not in confirmed:
        fail.append(f"synthesis Domains names '{name}', which is not a confirmed register row")

for name in candidates:
    if name not in syn_candidates:
        fail.append(f"candidate '{name}' is missing from the synthesis Candidate Outcomes table")
for name in sorted(set(syn_candidates) - set(candidates)):
    fail.append(f"synthesis Candidate Outcomes names '{name}', which is not a register row")

# The destination cell may explain itself after the register's detail, set
# off by an em dash. A bare space would let a second destination through.
for name in candidates:
    if name not in syn_candidates:
        continue
    r_out, r_detail = reg_outcomes[name]
    s_out, s_dest = syn_candidates[name]
    if not r_out:
        if s_out:
            fail.append(f"synthesis Candidate Outcomes row '{name}' says '{s_out}', register has no outcome")
    elif s_out != r_out:
        fail.append(f"synthesis Candidate Outcomes row '{name}' says '{s_out}', register says '{r_out}'")
    elif r_out == 'confirmed':
        if file_cell(s_dest) != r_detail:
            fail.append(f"synthesis Candidate Outcomes row '{name}' names file '{file_cell(s_dest)}', register says '{r_detail}'")
    elif r_out in ('merged', 'split', 'dropped'):
        if not (s_dest == r_detail or s_dest.startswith(r_detail + ' — ')):
            fail.append(f"synthesis Candidate Outcomes row '{name}' says '{s_dest[:len(r_detail) + 20]}', register says '{r_detail}'")

for sid in sorted(file_seams):
    holders = file_seams[sid]
    names = [h[0] for h in holders]

    if len(holders) != 2:
        fail.append(f"{sid} is named in {len(holders)} domain files' ## Seams sections ({', '.join(sorted(names))}), expected 2")

    owners = {h[2] for h in holders}
    if len(owners) != 1:
        fail.append(f"{sid} names {len(owners)} owners across its files: {', '.join(sorted(owners))}")
    file_owner = sorted(owners)[0]

    if len(holders) == 2:
        # Each file names the other as its partner. A pair that agrees on the
        # ID but points at a third domain is a seam nobody owns the far side of.
        (n1, p1, _), (n2, p2, _) = holders
        if p1 != n2 or p2 != n1:
            fail.append(f"{sid}: {n1} names partner '{p1}' and {n2} names partner '{p2}'; expected each to name the other")
        if file_owner not in (n1, n2):
            fail.append(f"{sid}: owner '{file_owner}' is not one of its endpoints ({n1}, {n2})")

    if sid not in syn_seams:
        fail.append(f"{sid} is in the domain files but missing from the synthesis Seams table")
        continue

    a, b, syn_owner = syn_seams[sid]
    if syn_owner != file_owner:
        fail.append(f"{sid}: synthesis owner '{syn_owner}', domain files say '{file_owner}'")
    if len(holders) == 2 and {a, b} != set(names):
        fail.append(f"{sid}: synthesis endpoints {{{a}, {b}}}, domain files {{{', '.join(sorted(names))}}}")
    if syn_owner not in (a, b):
        fail.append(f"{sid}: synthesis owner '{syn_owner}' is not one of its synthesis endpoints ({a}, {b})")

for sid in sorted(set(syn_seams) - set(file_seams)):
    fail.append(f"{sid} is in the synthesis Seams table but no domain file's ## Seams section names it")

print(f"check5: {len(confirmed)} confirmed domains, {len(file_seams)} seams, {len(candidates)} candidates, status={state}")

if fail:
    for f in fail:
        print("check5: " + f)
    sys.exit(1)
sys.exit(0)
PY
