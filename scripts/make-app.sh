#!/usr/bin/env bash
#
# Build Chronicle.app from the daemon and UI builds.
#
# Usage:
#   scripts/make-app.sh           # debug build (default)
#   scripts/make-app.sh release   # release build
#
# Produces: dist/Chronicle.app

set -euo pipefail

cd "$(dirname "$0")/.."

CONFIG="${1:-debug}"
case "$CONFIG" in
    debug|release) ;;
    *)
        echo "Unknown config: $CONFIG (expected: debug or release)" >&2
        exit 2
        ;;
esac

MANIFEST="chronicle-daemon/Cargo.toml"
METADATA="$(cargo metadata --no-deps --format-version 1 --manifest-path "$MANIFEST")"
VERSION="$(/usr/bin/python3 -c 'import json, sys
m = json.load(sys.stdin)
print(next(p["version"] for p in m["packages"] if p["name"] == "chronicle-daemon"))' <<<"$METADATA")"
TARGET_DIR="$(/usr/bin/python3 -c 'import json, sys
print(json.load(sys.stdin)["target_directory"])' <<<"$METADATA")"
BUILD="$(git rev-list --count HEAD)"

echo "Building chronicle-daemon ($CONFIG)..."
if [ "$CONFIG" = "release" ]; then
    CHRONICLE_BUILD_NUMBER="$BUILD" cargo build --release --manifest-path "$MANIFEST"
else
    CHRONICLE_BUILD_NUMBER="$BUILD" cargo build --manifest-path "$MANIFEST"
fi
DAEMON_BIN="$TARGET_DIR/$CONFIG/chronicle-daemon"

echo "Building ChronicleUI ($CONFIG)..."
UI_BIN="$(cd chronicle-ui && swift build -c "$CONFIG" --show-bin-path)/ChronicleUI"
# SwiftPM does not relink when only the -sectcreate Info.plist changes.
rm -f "$UI_BIN"
(cd chronicle-ui && swift build -c "$CONFIG")

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

smmla="$(otool -tv "$DAEMON_BIN" | grep -c smmla || true)"
if [ "$smmla" != "0" ]; then
    echo "$DAEMON_BIN has $smmla i8mm instructions; run cargo clean -p whisper-rs-sys" >&2
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
