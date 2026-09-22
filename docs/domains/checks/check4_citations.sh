#!/usr/bin/env bash
# Closure check 4: every claim bullet carries a citation, and every citation
# resolves to a symbol that is really in the file it names.
# Per Design section 5 check 4, NFR-1, and the grammar in Design 2.2.
#
# The four claim sections must all be present, each with at least one bullet.
# Every line in them is either a bullet -- the `- None` sentinel, or ending in
# `— `path` — `Symbol`` -- or one of Boundary's two `**` fields, which check 3
# owns. Anything else is a failure, including a bare prose sentence and an
# indented bullet. That is the difference between this check and one that only
# validates what already looks like a citation.
#
# Exit 0 = every section present, every bullet grammatical, every citation
#          resolved.
# Exit 1 = any of those failed, or zero bullets or zero citations were seen.
#
# Invoke as: bash check4_citations.sh [MAP_ROOT] [REPO_ROOT]
set -u
set -o pipefail

MAP_ROOT="${1:-docs/domains}"
REPO_ROOT="${2:-.}"

[ -d "$MAP_ROOT/maps" ] || { echo "check4: no maps/ directory under $MAP_ROOT"; exit 1; }

python3 - "$MAP_ROOT" "$REPO_ROOT" <<'PY'
import re, sys, os, glob

map_root, repo_root = sys.argv[1], sys.argv[2]
fail = []

SECTIONS = ('Boundary', 'Depends On', 'Depended On By', 'Seams')

files = sorted(glob.glob(os.path.join(glob.escape(map_root), 'maps', '*.md')))
if not files:
    print("check4: examined zero domain files")
    sys.exit(1)

# The citation tail. Design 2.2 fixes it as `— `path` — `Symbol`` at the end of
# the bullet. Anchored at the end so a bullet that merely mentions a backticked
# path mid-sentence does not read as a citation.
TAIL = re.compile(r'—\s+`([^`]+)`\s+—\s+`([^`]+)`\s*$')

# Indentation is not a defence. `  - claim with no citation` is still a claim;
# matching only a column-zero `- ` let one escape the grammar check entirely and
# land in the prose counter instead, which is the silent pass this check exists
# to prevent.
BULLET = re.compile(r'^\s*-\s')

n_bullets = 0
n_citations = 0
n_prose = 0
boundary_cited = {}

for path in files:
    name = os.path.basename(path)
    text = open(path, encoding='utf-8').read()

    for sec in SECTIONS:
        m = re.search(r'^## ' + re.escape(sec) + r'\n(.*?)(?=^## |\Z)', text, re.S | re.M)
        if m is None:
            # A missing section is a failure, not an empty set. An absent
            # `## Seams` cannot be told from a domain with no seams unless the
            # section is required to exist and say `- None`.
            fail.append(f"{name}: no ## {sec} section")
            continue

        body = m.group(1)
        bullets = [l.rstrip() for l in body.split('\n') if BULLET.match(l)]

        if not bullets:
            fail.append(f"{name} [{sec}]: section present but carries no bullet")
            continue

        # FORMAT.md: "A section is never left blank and never carries a bare
        # prose sentence." A paragraph here is a claim with no citation, which
        # is what this check exists to catch; scoping the gate to bullets would
        # let anyone make one by removing a leading dash.
        #
        # Exactly two lines are exempt, both in Boundary and both owned by
        # check 3. Exempting any `**` run instead let an uncited claim through
        # in any section by opening it with a bold word.
        for line in body.split('\n'):
            s = line.strip()
            if not s or BULLET.match(line):
                continue
            if sec == 'Boundary' and (
                s.startswith('**Placement test:**') or s.startswith('**Document comparison:**')
            ):
                continue
            n_prose += 1
            fail.append(f"{name} [{sec}]: bare prose, not a bullet: {s[:72]!r}")

        for b in bullets:
            n_bullets += 1

            if b.strip() == '- None':
                continue
            # `- none found` is the Offshoots Filed sentinel, not a claim-section
            # one. Name it explicitly so the failure says which sentinel was
            # wanted rather than reading as a grammar error.
            if b.strip() == '- none found':
                fail.append(
                    f"{name} [{sec}]: `- none found` is the Offshoots Filed sentinel; "
                    f"this section's empty sentinel is `- None`"
                )
                continue

            t = TAIL.search(b)
            if t is None:
                fail.append(f"{name} [{sec}]: bullet has no citation: {b[:72]!r}")
                continue

            n_citations += 1
            cited_path, symbol = t.group(1), t.group(2)

            full = os.path.join(repo_root, cited_path)
            if not os.path.isfile(full):
                fail.append(f"{name} [{sec}]: cited path does not exist: {cited_path}")
                continue
            # isfile() resolves src/POWER.rs to src/power.rs on a
            # case-insensitive volume, so a citation that fails on a
            # case-sensitive checkout passes here. check2 guards the same
            # hazard on Detail cells.
            d, b_ = os.path.split(full)
            try:
                if b_ not in os.listdir(d):
                    fail.append(
                        f"{name} [{sec}]: cited path differs in case from disk: {cited_path}"
                    )
                    continue
            except OSError as e:
                fail.append(f"{name} [{sec}]: cannot list {d}: {e}")
                continue

            # A path that exists is not enough -- Design 2.2. Open it and
            # confirm the symbol is really in there.
            try:
                content = open(full, encoding='utf-8', errors='replace').read()
            except OSError as e:
                fail.append(f"{name} [{sec}]: cannot read {cited_path}: {e}")
                continue

            # Containment alone matches `foo` inside `foo_with_timeout`, so a
            # rename that extends the old name reads as resolved. Every cited
            # symbol is a plain identifier, so require identifier boundaries.
            if re.search(r'(?<!\w)' + re.escape(symbol) + r'(?!\w)', content) is None:
                fail.append(
                    f"{name} [{sec}]: symbol `{symbol}` not found in {cited_path}"
                )
            elif sec == 'Boundary':
                boundary_cited[name] = boundary_cited.get(name, 0) + 1

# The zero-citation guard below is global, so it cannot see one file gutted
# while the rest stay intact. A confirmed domain whose Boundary is `- None` has
# made no claim at all, which is the per-file form of the same vacuity.
for path in files:
    nm = os.path.basename(path)
    if boundary_cited.get(nm, 0) == 0:
        fail.append(f"{nm} [Boundary]: no resolved citation; the domain claims nothing")

print(f"check4: examined {len(files)} domain files, {n_bullets} bullets, {n_citations} citations")
if n_prose:
    print(f"check4: {n_prose} bare prose lines inside claim sections")

for f in fail:
    print("check4: " + f)

if n_bullets == 0:
    print("check4: zero bullets examined")
    sys.exit(1)
if n_citations == 0:
    print("check4: zero citations examined")
    sys.exit(1)
if fail:
    sys.exit(1)
sys.exit(0)
PY
