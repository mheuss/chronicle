#!/usr/bin/env bash
#
# Build the chronicle-ui SwiftUI binary and assemble a minimal Chronicle.app
# bundle so macOS Control Center registers the NSStatusItem.
#
# Usage:
#   ./scripts/make-app.sh           # debug build (default)
#   ./scripts/make-app.sh release   # release build
#
# Produces: .build/Chronicle.app — launch via `open .build/Chronicle.app`.
#
# Full code-signed packaging + SMAppService comes in HEU-420; this is the
# minimum bundle structure needed for the menu bar icon to render under
# modern macOS Control Center.

set -euo pipefail

cd "$(dirname "$0")/.."

CONFIG="${1:-debug}"
case "$CONFIG" in
    debug)   ARCH_DIR="arm64-apple-macosx/debug" ;;
    release) ARCH_DIR="arm64-apple-macosx/release" ;;
    *)
        echo "Unknown config: $CONFIG (expected: debug or release)" >&2
        exit 2
        ;;
esac

echo "Building ChronicleUI ($CONFIG)..."
if [ "$CONFIG" = "release" ]; then
    swift build -c release
else
    swift build
fi

BIN=".build/$ARCH_DIR/ChronicleUI"
if [ ! -f "$BIN" ]; then
    echo "Binary not found at $BIN" >&2
    exit 1
fi

APP=".build/Chronicle.app"
echo "Assembling $APP..."
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"

cp Sources/ChronicleUI/Info.plist "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/ChronicleUI"
chmod +x "$APP/Contents/MacOS/ChronicleUI"

# PkgInfo is optional but harmless; some old tools look for it.
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo ""
echo "Built: $APP"
echo "Launch with: open $APP"
echo "Or in foreground for logs:  $APP/Contents/MacOS/ChronicleUI"
