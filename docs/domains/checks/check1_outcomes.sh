#!/usr/bin/env bash
# Closure check 1: every register entry has a permitted outcome and a date,
# and the outcome grammar, merge targets and split children hold.
# Per Design section 5 check 1, and the grammar in Design 1.2 / 1.4.
#
# Exit 0 = grammar passes and the register is complete.
# Exit 3 = grammar passes, register incomplete. Rows remain to walk.
# Exit 1 = a grammar rule failed, or zero rows were examined.
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"
REG="$MAP_ROOT/REGISTER.md"

[ -f "$REG" ] || { echo "check1: no register at $REG"; exit 1; }

python3 - "$REG" "$MAP_ROOT" <<'PY'
import re, sys, os

reg_path, map_root = sys.argv[1], sys.argv[2]
text = open(reg_path, encoding='utf-8').read()

fail = []

def section(text, name):
    m = re.search(r'^## ' + re.escape(name) + r'\n(.*?)(?=^## |\Z)', text, re.S | re.M)
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

reg_block = section(text, 'Register')
if reg_block is None:
    print("check1: no ## Register section in REGISTER.md")
    sys.exit(1)

rows = register_rows(reg_block, fail)
blocked = register_rows(section(text, 'Blocked rows'), fail)

if not rows:
    print("check1: examined zero register rows")
    sys.exit(1)

WALK_ROWS = 14
OUTCOMES = {'confirmed', 'merged', 'dropped', 'split'}
by_name, by_num = {}, {}

for cells in rows:
    if len(cells) < 8:
        fail.append(f"row {cells[0]}: has {len(cells)} cells, expected 8")
        continue
    num, name, outcome, decided, detail, offshoots, roots, symbols = cells[:8]
    by_name[name] = (outcome, detail, num)
    by_num[num] = name

    # A row must be walkable whether or not it is decided. An appended row is
    # the case that matters: a split writes it, and Task 23 has nothing to run
    # a sitting from unless Roots and Symbols came with it.
    if not roots:
        fail.append(f"row {num} ({name}): has no Roots")
    # The walk opened with fourteen candidates. A row numbered above that was
    # appended mid-walk, and an appended row has to carry the Symbols a sitting
    # greps from, because no design manifest supplied them.
    if int(num) > WALK_ROWS and not symbols:
        fail.append(f"row {num} ({name}): appended row has no Symbols")

    if outcome and outcome not in OUTCOMES:
        fail.append(f"row {num} ({name}): outcome '{outcome}' not in {sorted(OUTCOMES)}")
        continue
    if not outcome:
        continue

    if not re.fullmatch(r'\d{4}-\d{2}-\d{2}', decided):
        fail.append(f"row {num} ({name}): Decided '{decided}' is not YYYY-MM-DD")

    if outcome == 'confirmed':
        if not re.fullmatch(r'maps/[A-Za-z0-9_.-]+\.md', detail):
            fail.append(f"row {num} ({name}): confirmed Detail '{detail}' is not maps/<name>.md")
    elif outcome == 'merged':
        if not re.fullmatch(r'into \S+', detail):
            fail.append(f"row {num} ({name}): merged Detail '{detail}' is not 'into <candidate>'")
    elif outcome == 'dropped':
        if not re.match(r'cross-cutting: \S', detail):
            fail.append(f"row {num} ({name}): dropped Detail '{detail}' is not 'cross-cutting: <sentence>'")
    elif outcome == 'split':
        if not re.fullmatch(r'into \S+(, \S+)+', detail):
            fail.append(f"row {num} ({name}): split Detail '{detail}' is not 'into <name>, <name>[, ...]'")

    if not offshoots:
        fail.append(f"row {num} ({name}): decided but Offshoots is empty")
    elif offshoots != 'none found' and not re.fullmatch(r'CHR-\d+(, CHR-\d+)*', offshoots):
        fail.append(f"row {num} ({name}): Offshoots '{offshoots}' is not 'none found' or ', '-separated CHR- IDs")

# A merge target must already be confirmed, so no chain is longer than one.
for name, (outcome, detail, num) in by_name.items():
    if outcome != 'merged':
        continue
    target = detail.split(' ', 1)[1] if ' ' in detail else ''
    if target not in by_name:
        fail.append(f"row {num} ({name}): merges into '{target}', which is not a register row")
    elif by_name[target][0] != 'confirmed':
        fail.append(f"row {num} ({name}): merges into '{target}', whose outcome is '{by_name[target][0] or 'undecided'}', not confirmed")

# Split children must exist as rows.
for name, (outcome, detail, num) in by_name.items():
    if outcome != 'split':
        continue
    for child in [c.strip() for c in detail.replace('into ', '', 1).split(',')]:
        if child not in by_name:
            fail.append(f"row {num} ({name}): split names child '{child}', which is not a register row")

undecided = [n for n, (o, _, _) in by_name.items() if not o]
blocked_names = [c[1] for c in blocked if len(c) > 1]

for b in blocked_names:
    if b not in by_name:
        fail.append(f"blocked row '{b}' is not a register row")
    elif by_name[b][0]:
        fail.append(f"blocked row '{b}' already has outcome '{by_name[b][0]}'")

print(f"check1: examined {len(rows)} rows, {len(blocked)} blocked")

if fail:
    for f in fail:
        print("  " + f)
    sys.exit(1)

if undecided or blocked:
    print("incomplete")
    sys.exit(3)

print("complete")
sys.exit(0)
PY
