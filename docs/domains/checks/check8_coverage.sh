#!/usr/bin/env bash
# Closure check 8: every production source file resolves to exactly one
# confirmed domain or to an explicit exclusion with a reason.
# Per Design section 5 check 8, and BR-7.
#
# The inventory is regenerated from the repo rather than trusted, because a
# source file added during the walk would otherwise be missing from both the
# inventory and the map and the set arithmetic would balance anyway.
#
# Exit 0 = the inventory is current and partitions exactly into owned and
#          excluded.
# Exit 1 = the synthesis is absent, the globs did not complete, either side of
#          the inventory comparison is empty, the inventory has drifted, or the
#          partition fails; or zero confirmed rows were examined.
#
# Invoke as: bash check8_coverage.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"
REG="$MAP_ROOT/REGISTER.md"
SYN="$MAP_ROOT/SYNTHESIS.md"
INV="$MAP_ROOT/INVENTORY.txt"

[ -f "$REG" ] || { echo "check8: no register at $REG"; exit 1; }
[ -f "$INV" ] || { echo "check8: no inventory at $INV"; exit 1; }
[ -d "$MAP_ROOT/maps" ] || { echo "check8: no maps/ directory under $MAP_ROOT"; exit 1; }

# Task 28 Step 1 runs this check before the synthesis exists and requires a real
# failure, not a vacuous pass. Decided first, before anything is parsed.
[ -f "$SYN" ] || { echo "check8: no synthesis at $SYN"; exit 1; }

REPO_ABS=$(cd "$REPO_ROOT" 2>/dev/null && pwd) || { echo "check8: REPO_ROOT unreadable: $REPO_ROOT"; exit 1; }

# The four globs are Task 3 Step 1 verbatim, which is Design 3.2's table one
# line each. They are duplicated here rather than shared with Task 3 because
# this check exists to disagree with the file Task 3 produced; a shared
# generator would make the diff compare a list against itself.
# Three roots, not four, because chronicle-ui/Sources serves two of the globs --
# the .swift sweep and the Info.plist one.
for root in chronicle-daemon chronicle-ui/Sources chronicle-daemon/crates/storage/src/migrations; do
    [ -d "$REPO_ABS/$root" ] || { echo "check8: cannot regenerate the inventory; $root is not a directory under $REPO_ROOT"; exit 1; }
done

FRESH=$(mktemp) || { echo "check8: mktemp failed"; exit 1; }
trap 'rm -f "$FRESH"' EXIT

# Each find guards itself. A ( a; b; c; d ) group exits with d's status, so a
# guard on the group alone sees only the last glob -- the first three could fail
# and the short list would surface as inventory drift, sending whoever reads it
# to the inventory when the problem was find.
( cd "$REPO_ABS" && \
  ( /usr/bin/find chronicle-daemon \
        -path '*/target' -prune -o -path '*/tests' -prune -o -path '*/examples' -prune -o \
        -name '*.rs' -print || exit 1
    /usr/bin/find chronicle-ui/Sources -name '*.swift' || exit 1
    /usr/bin/find chronicle-daemon/crates/storage/src/migrations -name '*.sql' || exit 1
    /usr/bin/find chronicle-ui/Sources -name 'Info.plist' || exit 1 ) | sort ) > "$FRESH" \
  || { echo "check8: inventory regeneration failed; the globs did not complete"; exit 1; }

# The globs above name three roots, so a source file outside all three is
# invisible to them and a new top-level crate would be unowned with this check
# still passing. This sweep covers the rest of the tree and fails on what it
# finds, which turns a silent under-enumeration into a named source root.
#
# chronicle-daemon is pruned because its glob is recursive over the whole root.
# chronicle-ui is not: its glob reads only Sources, so pruning the root here
# would leave a .swift in a sibling directory invisible to both. The three
# entries pruned under it are the ones whose content does not ship -- tests, the
# build directory, and the package manifest.
STRAY=$(cd "$REPO_ABS" && /usr/bin/find . \
    -path './.git' -prune -o \
    -path './.sop-tmp' -prune -o \
    -path './chronicle-daemon' -prune -o \
    -path './chronicle-ui/Sources' -prune -o \
    -path './chronicle-ui/Tests' -prune -o \
    -path './chronicle-ui/.build' -prune -o \
    -path './chronicle-ui/Package.swift' -prune -o \
    -path '*/target' -prune -o -path '*/.build' -prune -o \
    \( -name '*.rs' -o -name '*.swift' \) -print) \
  || { echo "check8: stray-source sweep failed"; exit 1; }
if [ -n "$STRAY" ]; then
    echo "check8: source files outside every known root; the map does not cover them:"
    echo "$STRAY" | sed 's|^\./|  |'
    exit 1
fi

python3 - "$MAP_ROOT" "$FRESH" <<'PY'
import re, sys, os

map_root, fresh_path = sys.argv[1], sys.argv[2]
reg_path = os.path.join(map_root, 'REGISTER.md')
inv_path = os.path.join(map_root, 'INVENTORY.txt')
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

# check4's guard, for the two sections check4 does not read. A leading-dash
# filter drops a malformed line before the grammar sees it, so a smuggled
# "-path" with no space validated nothing -- and in Owned Files it hid a real
# double-ownership violation. FORMAT.md:53-54: a section never carries a bare
# prose sentence.
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

def lines(path):
    return [l.strip() for l in open(path, encoding='utf-8') if l.strip()]

# ---------------------------------------------------------------------------
# The inventory must still describe the repo.
# ---------------------------------------------------------------------------
recorded = lines(inv_path)
fresh = lines(fresh_path)

# Every assertion in this check is a loop over one of these sets. All empty and
# "every production source file resolves to exactly one domain" is vacuously
# true, which is the silent pass this check exists to be. Every sibling carries
# the equivalent guard -- check1's "examined zero register rows", check2's "zero
# confirmed rows" and check4's "zero bullets examined". This is the one whose
# whole job is set arithmetic, so it is the one where an empty set does the
# most damage.
if not fresh:
    print("check8: the globs found zero source files under the repo root")
    sys.exit(1)
if not recorded:
    print("check8: INVENTORY.txt is empty")
    sys.exit(1)

dup = sorted({p for p in recorded if recorded.count(p) > 1})
for p in dup:
    fail.append(f"INVENTORY.txt lists {p} more than once")

rec_set, fresh_set = set(recorded), set(fresh)
for p in sorted(fresh_set - rec_set):
    fail.append(f"{p} is a production source file and is not in INVENTORY.txt")
for p in sorted(rec_set - fresh_set):
    fail.append(f"INVENTORY.txt lists {p}, which the globs no longer find")

# ---------------------------------------------------------------------------
# Owned: every confirmed domain's ## Owned Files.
# ---------------------------------------------------------------------------
reg_text = open(reg_path, encoding='utf-8').read()
reg_block = section(reg_text, 'Register', 'REGISTER.md')
if reg_block is None:
    print("check8: no ## Register section in REGISTER.md")
    sys.exit(1)

confirmed = {}
for cells in register_rows(reg_block, fail):
    if len(cells) < 8:
        fail.append(f"row {cells[0]}: has {len(cells)} cells, expected 8")
        continue
    if cells[2] == 'confirmed':
        confirmed[cells[1]] = cells[4]

if not confirmed:
    print("check8: register holds no confirmed rows")
    sys.exit(1)

owned = {}   # path -> [domain, ...]
for name, detail in sorted(confirmed.items()):
    path = os.path.join(map_root, detail)
    if not os.path.isfile(path):
        fail.append(f"{name}: no domain file at {detail} (check2 owns this; check8 cannot count without it)")
        continue
    base = os.path.basename(detail)
    body = section(open(path, encoding='utf-8').read(), 'Owned Files', os.path.basename(path))
    if body is None:
        fail.append(f"{base}: no ## Owned Files section")
        continue
    bullets = bullets_of(body, f"{base} [Owned Files]", fail)
    if not bullets:
        fail.append(f"{base} [Owned Files]: section present but carries no bullet")
        continue
    if bullets == ['- None']:
        continue
    for b in bullets:
        p = re.sub(r'^-\s*', '', b).strip().strip('`')
        # FORMAT.md: one exact path per bullet, copied verbatim from
        # INVENTORY.txt. A directory or a glob is a failure rather than a
        # shorthand, because the arithmetic below is set comparison and a
        # directory matches nothing in the inventory while looking like it
        # covers everything under it.
        if p.endswith('/') or '*' in p or '?' in p:
            fail.append(f"{base} [Owned Files]: {p!r} is a directory or glob; one exact inventory path per bullet")
            continue
        if p not in rec_set:
            fail.append(f"{base} [Owned Files]: {p!r} is not a path in INVENTORY.txt")
            continue
        owned.setdefault(p, []).append(name)

for p in sorted(owned):
    if len(owned[p]) > 1:
        fail.append(f"{p} is owned by {len(owned[p])} domains: {', '.join(sorted(owned[p]))}")

# ---------------------------------------------------------------------------
# Excluded: the synthesis's Exclusions table.
# ---------------------------------------------------------------------------
syn_text = open(syn_path, encoding='utf-8').read()
excl_block = section(syn_text, 'Exclusions', 'SYNTHESIS.md')
if excl_block is None:
    fail.append("synthesis has no ## Exclusions section")
    excl_block = ''

excluded = set()
for cells in table_rows(excl_block):
    if len(cells) < 2:
        fail.append(f"synthesis Exclusions row has {len(cells)} cells, expected 2: {cells}")
        continue
    p, reason = cells[0].strip().strip('`'), cells[1]
    if not reason:
        fail.append(f"synthesis Exclusions row {p!r} has an empty Reason cell")
    if p in excluded:
        fail.append(f"synthesis Exclusions lists {p!r} twice")
    excluded.add(p)

# ---------------------------------------------------------------------------
# The partition.
# ---------------------------------------------------------------------------
owned_set = set(owned)

for p in sorted(owned_set & excluded):
    fail.append(f"{p} is both owned and excluded")

for p in sorted(rec_set - owned_set - excluded):
    fail.append(f"{p} is in INVENTORY.txt and is neither owned nor excluded")

for p in sorted(excluded - rec_set):
    fail.append(f"synthesis excludes {p!r}, which is not a path in INVENTORY.txt")

print(f"check8: {len(recorded)} inventory paths, {len(owned_set)} owned across {len(confirmed)} domains, {len(excluded)} excluded")

if fail:
    for f in fail:
        print("check8: " + f)
    sys.exit(1)
sys.exit(0)
PY
