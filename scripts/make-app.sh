#!/usr/bin/env bash
#
# Build and sign Chronicle.app from the daemon and UI builds.
#
# Usage:
#   scripts/make-app.sh           # debug build (default)
#   scripts/make-app.sh release   # release build
#   scripts/make-app.sh dist      # release build from a clean tree
#
# Produces: dist/Chronicle.app
# One-time setup: docs/guides/signing-and-notarization.md

set -euo pipefail

cd "$(dirname "$0")/.."

CONFIG="${1:-debug}"
case "$CONFIG" in
    debug|release|dist) ;;
    *)
        echo "Unknown config: $CONFIG (expected: debug, release or dist)" >&2
        exit 2
        ;;
esac
BUILD_CONFIG="$CONFIG"
if [ "$CONFIG" = "dist" ]; then
    BUILD_CONFIG="release"
fi

GUIDE="docs/guides/signing-and-notarization.md"

resolve_identity() {
    local matches
    matches="$(security find-identity -v -p codesigning | grep '"Developer ID Application: ' || true)"
    if [ -n "${CHRONICLE_SIGN_IDENTITY:-}" ]; then
        matches="$(grep -F -- "$CHRONICLE_SIGN_IDENTITY" <<<"$matches" || true)"
    fi
    local count
    count="$(grep -c . <<<"$matches" || true)"
    if [ "$count" -ne 1 ]; then
        echo "Found $count matching Developer ID Application identities; expected 1." >&2
        [ -n "$matches" ] && echo "$matches" >&2
        echo "Set CHRONICLE_SIGN_IDENTITY, or see $GUIDE#certificate" >&2
        exit 1
    fi
    awk '{print $2}' <<<"$matches"
}

IDENTITY="$(resolve_identity)"

if [ "$CONFIG" = "dist" ]; then
    if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
        echo "dist needs a clean working tree. Commit or stash your changes first." >&2
        exit 1
    fi
    PROFILE="${CHRONICLE_NOTARY_PROFILE:-chronicle-notary}"
    if ! notary_check="$(xcrun notarytool history --keychain-profile "$PROFILE" 2>&1)"; then
        echo "$notary_check" >&2
        echo "Notary profile '$PROFILE' is not usable. See $GUIDE#notary-profile" >&2
        exit 1
    fi
fi

MANIFEST="chronicle-daemon/Cargo.toml"
METADATA="$(cargo metadata --no-deps --format-version 1 --manifest-path "$MANIFEST")"
VERSION="$(/usr/bin/python3 -c 'import json, sys
m = json.load(sys.stdin)
print(next(p["version"] for p in m["packages"] if p["name"] == "chronicle-daemon"))' <<<"$METADATA")"
BUILD="$(git rev-list --count HEAD)"

echo "Building chronicle-daemon ($BUILD_CONFIG)..."
CARGO_ARGS=(--manifest-path "$MANIFEST" --message-format=json-render-diagnostics)
if [ "$BUILD_CONFIG" = "release" ]; then
    CARGO_ARGS+=(--release)
fi
CARGO_JSON="$(CHRONICLE_BUILD_NUMBER="$BUILD" cargo build "${CARGO_ARGS[@]}")"
DAEMON_BIN="$(/usr/bin/python3 -c 'import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m["target"]["name"] == "chronicle-daemon" and m.get("executable"):
        print(m["executable"])' <<<"$CARGO_JSON")"
NATIVE_RLIBS="$(/usr/bin/python3 -c 'import json, re, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m["target"]["name"] in ("whisper_rs_sys", "audiopus_sys"):
        print(*(f for f in m["filenames"] if f.endswith(".rlib")), sep="\n")
    if m.get("reason") != "build-script-executed":
        continue
    crate = re.search(r"[#/](whisper-rs-sys|audiopus_sys)(?:[@#]|$)", m["package_id"])
    if not crate:
        continue
    out = m["out_dir"]
    dirs = [re.sub(r"^(native|crate|dependency|framework|all)=", "", p) for p in m["linked_paths"]]
    if not dirs or not all(d == out or d.startswith(out + "/") for d in dirs):
        sys.exit(crate.group(1) + " linked a library from outside its own build. For audiopus_sys, set OPUS_NO_PKG_CONFIG=1, unset LIBOPUS_LIB_DIR and OPUS_LIB_DIR, then run cargo clean -p audiopus_sys")' <<<"$CARGO_JSON")"

# The linker sets the binary's minos itself, so check the rlibs, which hold the native objects rustc linked.
if [ "$(wc -l <<<"$NATIVE_RLIBS")" -ne 2 ]; then
    echo "Expected the whisper-rs-sys and audiopus_sys rlibs, got: $NATIVE_RLIBS" >&2
    exit 1
fi
while read -r rlib; do
    minos="$(otool -l "$rlib" | grep -A3 LC_BUILD_VERSION | grep minos || true)"
    if [ -z "$minos" ]; then
        echo "No build version found in $rlib" >&2
        exit 1
    fi
    if grep -qv 'minos 14.0$' <<<"$minos"; then
        echo "$rlib has objects built for another macOS. Clear the stale native builds as .cargo/config.toml describes, and check RUSTFLAGS for an outside -L" >&2
        exit 1
    fi
done <<<"$NATIVE_RLIBS"

echo "Building ChronicleUI ($BUILD_CONFIG)..."
UI_BIN="$(cd chronicle-ui && swift build -c "$BUILD_CONFIG" --show-bin-path)/ChronicleUI"
# SwiftPM doesn't always relink when only the -sectcreate Info.plist changes.
rm -f "$UI_BIN"
(cd chronicle-ui && swift build -c "$BUILD_CONFIG")

for bin in "$DAEMON_BIN" "$UI_BIN"; do
    if [ ! -f "$bin" ]; then
        echo "Binary not found at $bin" >&2
        exit 1
    fi
    archs="$(lipo -archs "$bin")"
    if [ "$archs" != "arm64" ]; then
        echo "$bin is built for '$archs'; expected arm64 only" >&2
        exit 1
    fi
    minos="$(otool -l "$bin" | grep -A3 LC_BUILD_VERSION | grep -m1 minos || true)"
    if [[ "$minos" != *"minos 14.0" ]]; then
        echo "$bin declares '$minos'; expected minos 14.0" >&2
        exit 1
    fi
done

disasm="$(otool -tv "$DAEMON_BIN")"
smmla="$(grep -c smmla <<<"$disasm" || true)"
if [ "$smmla" != "0" ]; then
    echo "$DAEMON_BIN has $smmla i8mm instructions; clear the stale native builds as .cargo/config.toml describes" >&2
    exit 1
fi

daemon_mic="$(launchctl plist __TEXT,__info_plist "$DAEMON_BIN" | /usr/bin/python3 -c 'import re, sys
print(re.search(r"\"NSMicrophoneUsageDescription\" = \"(.*)\";", sys.stdin.read()).group(1))')"
ui_mic="$(/usr/libexec/PlistBuddy -c 'Print :NSMicrophoneUsageDescription' chronicle-ui/Sources/ChronicleUI/Info.plist)"
if [ "$daemon_mic" != "$ui_mic" ]; then
    echo "NSMicrophoneUsageDescription differs between chronicle-daemon/build.rs and the UI Info.plist" >&2
    exit 1
fi

APP="dist/Chronicle.app"
CONTENTS="$APP/Contents"
AGENT_PLIST="dev.heuss.chronicle.daemon.plist"

echo "Assembling $APP..."
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Library/LaunchAgents"
cp chronicle-ui/Sources/ChronicleUI/Info.plist "$CONTENTS/Info.plist"
cp "$UI_BIN" "$CONTENTS/MacOS/ChronicleUI"
cp "$DAEMON_BIN" "$CONTENTS/MacOS/chronicle-daemon"
cp "packaging/$AGENT_PLIST" "$CONTENTS/Library/LaunchAgents/$AGENT_PLIST"
printf 'APPL????' > "$CONTENTS/PkgInfo"

/usr/libexec/PlistBuddy \
    -c "Add :CFBundleShortVersionString string $VERSION" \
    -c "Add :CFBundleVersion string $BUILD" \
    "$CONTENTS/Info.plist"
plutil -lint "$CONTENTS/Info.plist" "$CONTENTS/Library/LaunchAgents/$AGENT_PLIST"

# CHR-70: replace with Developer ID signing, hardened runtime and entitlements.
codesign --force --sign - "$CONTENTS/MacOS/chronicle-daemon"
codesign --force --sign - "$APP"

echo ""
echo "Built: $APP (version $VERSION, build $BUILD)"
echo "Launch with: open $APP"
