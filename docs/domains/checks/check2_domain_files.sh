#!/usr/bin/env bash
# Closure check 2: domain files and confirmed rows are in exact correspondence.
# Per Design section 5 check 2, and BR-3.
#
# Every `confirmed` row has a domain file at its Detail path, under maps/.
# No row with any other outcome has one. No file in maps/ is unreferenced by a
# confirmed row.
#
# Exit 0 = the correspondence holds.
# Exit 1 = a rule failed, or zero rows were examined.
#
# Invoke as: bash check2_domain_files.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"
REG="$MAP_ROOT/REGISTER.md"

[ -f "$REG" ] || { echo "check2: no register at $REG"; exit 1; }
[ -d "$MAP_ROOT/maps" ] || { echo "check2: no maps/ directory under $MAP_ROOT"; exit 1; }

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
    reported, never skipped. Five checks parse this table and two of them used
    to disagree about such a row.
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
    print("check2: no ## Register section in REGISTER.md")
    sys.exit(1)

rows = register_rows(reg_block, fail)

if not rows:
    print("check2: examined zero register rows")
    sys.exit(1)

maps_dir = os.path.join(map_root, 'maps')

# Referenced paths are collected as basenames under maps/. The Detail cell is
# written relative to MAP_ROOT ("maps/<name>.md"), which is how Design 1.2
# spells it and what check 1 already enforces the shape of.
referenced = {}

for cells in rows:
    if len(cells) < 8:
        fail.append(f"row {cells[0]}: has {len(cells)} cells, expected 8")
        continue
    num, name, outcome, decided, detail = cells[:5]

    if outcome == 'confirmed':
        if not detail.startswith('maps/'):
            fail.append(f"row {num} ({name}): confirmed Detail '{detail}' is not under maps/")
            continue
        target = os.path.join(map_root, detail)
        if not os.path.isfile(target):
            fail.append(f"row {num} ({name}): confirmed, but no domain file at {detail}")
            continue
        # A case-insensitive volume resolves maps/OCR.md and maps/ocr.md to the
        # same file, so isfile() alone passes a spelling that fails elsewhere.
        # Compare against the real directory listing instead.
        base = os.path.basename(detail)
        if base not in os.listdir(maps_dir):
            fail.append(f"row {num} ({name}): Detail '{detail}' does not match the on-disk spelling")
            continue
        # The correspondence is one-to-one, not merely onto. Two confirmed rows
        # naming the same file both satisfy the isfile test, and the file is not
        # an orphan, so without this the count can read 11 confirmed / 10 files
        # and still pass -- one domain with no file at all. A copy-pasted Detail
        # cell is the likeliest way to get there.
        if base in referenced:
            owner = referenced[base]
            fail.append(
                f"row {num} ({name}): Detail '{detail}' is already claimed by row {owner}"
            )
            continue
        referenced[base] = num
    else:
        # Any other outcome -- merged, dropped, split, or an empty cell -- must
        # not have a domain file named after it. A merged candidate's
        # responsibility lives in its target's file; it gets none of its own.
        stray = f"{name.lower()}.md"
        if os.path.isfile(os.path.join(maps_dir, stray)) and stray in os.listdir(maps_dir):
            fail.append(
                f"row {num} ({name}): outcome '{outcome or 'empty'}' but a domain file exists at maps/{stray}"
            )

# Opening maps/ in Finder drops a .DS_Store, which is not a domain file.
entries = {f for f in os.listdir(maps_dir) if f != '.DS_Store'}

# "and nothing else lives in maps/" -- FORMAT.md, restating Design 2.1. Filtering
# the orphan diff to *.md would let any other file sit there unreported, since
# no confirmed row can ever name it.
for other in sorted(f for f in entries if not f.endswith('.md')):
    fail.append(f"maps/{other}: only domain files live in maps/")

on_disk = {f for f in entries if f.endswith('.md')}
orphans = sorted(on_disk - set(referenced))
for o in orphans:
    fail.append(f"maps/{o}: no confirmed row names it in its Detail cell")

n_conf = sum(1 for c in rows if len(c) >= 3 and c[2] == 'confirmed')

# A register with no confirmed rows, or an empty maps/, has verified nothing.
# checks 3 and 4 both guard this; without it check2 alone reports success on a
# gutted tree.
if n_conf == 0:
    fail.append("zero confirmed rows examined")
if not on_disk:
    fail.append("maps/ holds no domain files")

print(f"check2: examined {len(rows)} rows, {n_conf} confirmed, {len(on_disk)} files in maps/")

if fail:
    for f in fail:
        print("check2: " + f)
    sys.exit(1)
sys.exit(0)
PY
