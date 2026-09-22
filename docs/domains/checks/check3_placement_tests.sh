#!/usr/bin/env bash
# Closure check 3: every domain file states a placement test and a document
# comparison, both inside its Boundary section.
# Per Design section 5 check 3, and the field rules in Design 2.1.
#
# Exit 0 = every domain file carries both fields, non-empty, in Boundary.
# Exit 1 = a field is missing, empty, outside Boundary, or malformed; or zero
#          domain files were examined.
#
# Invoke as: bash check3_placement_tests.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"

[ -d "$MAP_ROOT/maps" ] || { echo "check3: no maps/ directory under $MAP_ROOT"; exit 1; }

python3 - "$MAP_ROOT" <<'PY'
import re, sys, os, glob

map_root = sys.argv[1]
fail = []

files = sorted(glob.glob(os.path.join(glob.escape(map_root), 'maps', '*.md')))
if not files:
    print("check3: examined zero domain files")
    sys.exit(1)

# Design 5 check 3 is worded against the Boundary section, so the parse is
# scoped to it. A file that states a Placement test under some other heading
# has not satisfied the rule -- that is the case this scoping exists to fail,
# and a whole-file search would pass it.
def boundary(text):
    m = re.search(r'^## Boundary\n(.*?)(?=^## |\Z)', text, re.S | re.M)
    return m.group(1) if m else None

for path in files:
    name = os.path.basename(path)
    text = open(path, encoding='utf-8').read()

    b = boundary(text)
    if b is None:
        fail.append(f"{name}: no ## Boundary section")
        continue

    for label in ('Placement test', 'Document comparison'):
        anywhere = re.search(r'^\*\*' + re.escape(label) + r':\*\*(.*)$', text, re.M)
        inside = re.search(r'^\*\*' + re.escape(label) + r':\*\*(.*)$', b, re.M)
        if inside is None:
            if anywhere is not None:
                fail.append(f"{name}: **{label}:** is present but outside the Boundary section")
            else:
                fail.append(f"{name}: no **{label}:** field")
            continue
        value = inside.group(1).strip()
        if not value:
            fail.append(f"{name}: **{label}:** is present but empty")
            continue
        if label == 'Document comparison':
            if not (value.startswith('differs') or value.startswith('deliberately matches')):
                fail.append(
                    f"{name}: **Document comparison:** starts with neither "
                    f"'differs' nor 'deliberately matches': {value[:50]!r}"
                )

print(f"check3: examined {len(files)} domain files")

if fail:
    for f in fail:
        print("check3: " + f)
    sys.exit(1)
sys.exit(0)
PY
