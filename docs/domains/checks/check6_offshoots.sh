#!/usr/bin/env bash
# Closure check 6: every offshoot the walk recorded is accounted for, locally
# and in Linear. Per Design section 5 check 6, and BR-4.
#
# A shell script cannot reach Linear, so the check is in two halves. The local
# half reads the register and the domain files. The Linear half reads
# OFFSHOOTS_RESOLVED.txt, which the executor writes at close from what Linear
# actually returned.
#
# Write that file from a `label:needs-domain` query -- one line per issue Linear
# returns -- and NOT by looking up the IDs the register already names. The
# difference decides whether the comparison below can see anything. Both
# directions are checked, but deriving the file from the register makes the
# resolved-minus-register branch unreachable by construction: an offshoot the
# walk filed and no row recorded is then invisible. That happened to CHR-155,
# which sat in Linear under the label with no register row until the close.
#
# Exit 0 = both halves pass.
# Exit 2 = the local half passed and OFFSHOOTS_RESOLVED.txt is absent.
# Exit 1 = anything else.
#
# Exit 2 is not a pass. Design section 5 check 6 requires every recorded ID to
# resolve in Linear, and MVF-2's exit criterion is that all eight checks pass.
# It is a distinct code because it is the state the suite sees on its first run,
# before Linear has been touched, and reporting that as a hard failure would
# hide a real one sitting next to it.
#
# Invoke as: bash check6_offshoots.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"
REG="$MAP_ROOT/REGISTER.md"

[ -f "$REG" ] || { echo "check6: no register at $REG"; exit 1; }
[ -d "$MAP_ROOT/maps" ] || { echo "check6: no maps/ directory under $MAP_ROOT"; exit 1; }

python3 - "$MAP_ROOT" <<'PY'
import re, sys, os

map_root = sys.argv[1]
reg_path = os.path.join(map_root, 'REGISTER.md')
maps_dir = os.path.join(map_root, 'maps')
resolved_path = os.path.join(map_root, 'OFFSHOOTS_RESOLVED.txt')

text = open(reg_path, encoding='utf-8').read()
fail = []

def section(text, name):
    m = re.search(r'^## ' + re.escape(name) + r'\n(.*?)(?=^## |\Z)', text, re.S | re.M)
    return m.group(1) if m else None

reg_block = section(text, 'Register')
if reg_block is None:
    print("check6: no ## Register section in REGISTER.md")
    sys.exit(1)

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

rows = register_rows(reg_block, fail)

if not rows:
    print("check6: examined zero register rows")
    sys.exit(1)

# The Task 4 grammar, restated in FORMAT.md: either `none found` or a
# ", "-separated run of CHR- IDs. check1 enforces the same shape on decided
# rows; check6 enforces it on every row, because an undecided row with a blank
# Offshoots cell is a row whose offshoots nobody has accounted for yet.
ID_RUN = re.compile(r'CHR-\d+(?:, CHR-\d+)*$')
ID = re.compile(r'CHR-\d+')
BULLET_ID = re.compile(r'^-\s+(CHR-\d+)\s+—\s+\S')

register_ids = set()
confirmed = {}   # domain name -> (row number, set of IDs, raw cell)

for cells in rows:
    if len(cells) < 8:
        fail.append(f"row {cells[0]}: has {len(cells)} cells, expected 8")
        continue
    num, name, outcome, _decided, detail, offshoots = cells[:6]

    if not offshoots:
        fail.append(f"row {num} ({name}): Offshoots cell is empty; expected 'none found' or CHR- IDs")
        continue
    if offshoots != 'none found' and not ID_RUN.fullmatch(offshoots):
        fail.append(f"row {num} ({name}): Offshoots '{offshoots}' is not 'none found' or ', '-separated CHR- IDs")
        continue

    ids = set(ID.findall(offshoots)) if offshoots != 'none found' else set()
    register_ids |= ids

    if outcome == 'confirmed':
        confirmed[name] = (num, ids, offshoots, detail)

if not confirmed:
    fail.append("zero confirmed rows examined")

# ---------------------------------------------------------------------------
# Local half, domain files.
#
# Every confirmed row's file carries an Offshoots Filed section naming exactly
# the IDs its register row names, and non-empty Status, Owns and Code.
#
# Design 2.1 requires all three header fields. A present-but-blank field passes
# a check that only greps for the label, which is why each is matched with a
# non-empty tail rather than by presence.
# ---------------------------------------------------------------------------
# check4's guard, for the two sections check4 does not read. A leading-dash
# filter drops a malformed line before the grammar sees it, so a smuggled
# "-path" with no space validated nothing -- and in the sibling section it hid a
# real double-ownership violation. FORMAT.md:53-54: a section never carries a
# bare prose sentence.
BULLET = re.compile(r'^\s*-\s')

def bullets_of(body, label, fail):
    out = []
    for line in body.split('\n'):
        s = line.strip()
        if not s:
            continue
        if BULLET.match(line):
            out.append(s)
        else:
            fail.append(f"{label}: bare prose, not a bullet: {s[:64]!r}")
    return out

HEADER = {
    'Status': re.compile(r'^\*\*Status:\*\*[ \t]*(\S.*)$', re.M),
    'Owns':   re.compile(r'^\*\*Owns:\*\*[ \t]*(\S.*)$', re.M),
    'Code':   re.compile(r'^\*\*Code:\*\*[ \t]*(\S.*)$', re.M),
}

for name, (num, ids, raw, detail) in sorted(confirmed.items()):
    # Read through the register's Detail cell rather than guessing <name>.md.
    # check2 owns the correspondence between the two; check6 follows it so the
    # two checks cannot disagree about which file belongs to which row.
    if not re.fullmatch(r'maps/[A-Za-z0-9_.-]+\.md', detail):
        fail.append(f"row {num} ({name}): confirmed Detail '{detail}' is not maps/<name>.md")
        continue
    path = os.path.join(map_root, detail)
    base = os.path.basename(detail)
    if not (os.path.isfile(path) and base in os.listdir(maps_dir)):
        fail.append(f"row {num} ({name}): no domain file at {detail}")
        continue

    dtext = open(path, encoding='utf-8').read()

    for field, pat in HEADER.items():
        if not pat.search(dtext):
            fail.append(f"{base}: **{field}:** is missing or empty")

    body = section(dtext, 'Offshoots Filed')
    if body is None:
        fail.append(f"{base}: no ## Offshoots Filed section")
        continue

    bullets = bullets_of(body, f"{base} [Offshoots Filed]", fail)
    if not bullets:
        fail.append(f"{base} [Offshoots Filed]: section present but carries no bullet")
        continue

    sentinel = bullets == ['- none found']
    file_ids = set()
    for b in bullets:
        if b == '- none found':
            continue
        # FORMAT.md's grammar is `- CHR-NNN — <one line>`, so the ID this
        # bullet files under is the leading one. Collecting every ID in the
        # line instead reads IDs named in the description as filings: storage's
        # bullet explains that CHR-90 was closed, and a findall counted CHR-90
        # as a second offshoot and failed a correct file.
        m = BULLET_ID.match(b)
        if not m:
            fail.append(f"{base} [Offshoots Filed]: bullet is not '- CHR-NNN — <text>': {b[:60]!r}")
            continue
        file_ids.add(m.group(1))

    if sentinel and ids:
        fail.append(f"{base}: says '- none found' but row {num} names {', '.join(sorted(ids))}")
    elif not sentinel and not ids:
        fail.append(f"{base}: names {', '.join(sorted(file_ids))} but row {num} says 'none found'")
    elif not sentinel and file_ids != ids:
        missing = sorted(ids - file_ids)
        extra = sorted(file_ids - ids)
        detail_msg = []
        if missing:
            detail_msg.append(f"missing {', '.join(missing)}")
        if extra:
            detail_msg.append(f"extra {', '.join(extra)}")
        fail.append(f"{base}: Offshoots Filed disagrees with row {num} -- " + "; ".join(detail_msg))

n_ids = len(register_ids)
print(f"check6: examined {len(rows)} rows, {len(confirmed)} confirmed, {n_ids} offshoot IDs")

if fail:
    for f in fail:
        print("check6: " + f)
    sys.exit(1)

# ---------------------------------------------------------------------------
# Linear half.
#
# An absent file and a zero-offshoot walk are different states. A walk that
# filed nothing still has to say so, which is why the empty file is required
# rather than inferred from an empty register.
# ---------------------------------------------------------------------------
if not os.path.isfile(resolved_path):
    print("check6: local half passed; OFFSHOOTS_RESOLVED.txt is absent, so Linear is unverified")
    sys.exit(2)

LINE = re.compile(r'^(CHR-\d+) resolved label=needs-domain$')
resolved_ids = set()
for n, line in enumerate(open(resolved_path, encoding='utf-8'), 1):
    line = line.rstrip('\n')
    if not line.strip():
        continue
    m = LINE.fullmatch(line)
    if not m:
        fail.append(f"OFFSHOOTS_RESOLVED.txt:{n}: {line[:60]!r} is not 'CHR-NNN resolved label=needs-domain'")
        continue
    if m.group(1) in resolved_ids:
        fail.append(f"OFFSHOOTS_RESOLVED.txt:{n}: {m.group(1)} is listed twice")
    resolved_ids.add(m.group(1))

for i in sorted(register_ids - resolved_ids):
    fail.append(f"{i} is in the register but not in OFFSHOOTS_RESOLVED.txt")
for i in sorted(resolved_ids - register_ids):
    fail.append(f"{i} is in OFFSHOOTS_RESOLVED.txt but no register row names it")

print(f"check6: {len(resolved_ids)} IDs resolved in Linear")

if fail:
    for f in fail:
        print("check6: " + f)
    sys.exit(1)
sys.exit(0)
PY
