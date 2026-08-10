#!/usr/bin/env bash
# Fetch a whisper.cpp ggml model into Chronicle's data dir.
# Usage: scripts/fetch-whisper-model.sh [base|small|medium]
# Default: base
#
# NOTE: developer/CI tool. The daemon downloads models itself (HEU-475);
# the app never calls this script. Pinned URL/SHA1 values are duplicated
# in chronicle-daemon/crates/transcription/src/lib.rs (MANIFEST) — update
# BOTH when upstream rotates. File mode: daemon provisions 0600; this
# script's 0644 is legacy dev-tool behavior.
set -euo pipefail

VARIANT="${1:-base}"
BASE_DIR="${HOME}/Library/Application Support/Chronicle"
MODELS_DIR="${BASE_DIR}/models"

# Pinned per-variant URL + SHA1. SHA1 matches the upstream
# `whisper.cpp/models/README.md` published values. Update when upstream
# rotates.
case "$VARIANT" in
  base)
    URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin"
    SHA1="465707469ff3a37a2b9b8d8f89f2f99de7299dac"
    ;;
  small)
    URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"
    SHA1="55356645c2b361a969dfd0ef2c5a50d530afd8d5"
    ;;
  medium)
    URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin"
    SHA1="fd9727b6e1217c2f614f9b698455c4ffd82463b4"
    ;;
  *)
    echo "Unknown variant: $VARIANT (allowed: base, small, medium)" >&2
    exit 2
    ;;
esac

# The variant is now validated against the allow-list. Build the destination
# path and install the cleanup trap — a partial download is removed if the
# script aborts before the final mv (e.g. curl fails); harmless on success.
DEST="${MODELS_DIR}/ggml-${VARIANT}.bin"
trap 'rm -f "$DEST.tmp"' EXIT

mkdir -p "$MODELS_DIR"

if [[ -f "$DEST" ]]; then
  ACTUAL="$(shasum -a 1 "$DEST" | awk '{print $1}')"
  if [[ "$ACTUAL" == "$SHA1" ]]; then
    echo "Already present and checksum matches: $DEST"
    exit 0
  fi
  echo "Existing file checksum mismatch, re-downloading: $DEST" >&2
  rm "$DEST"
fi

echo "Downloading $VARIANT → $DEST"
curl -fL --progress-bar -o "$DEST.tmp" "$URL"

ACTUAL="$(shasum -a 1 "$DEST.tmp" | awk '{print $1}')"
if [[ "$ACTUAL" != "$SHA1" ]]; then
  rm -f "$DEST.tmp"
  echo "Checksum mismatch: expected $SHA1, got $ACTUAL" >&2
  exit 3
fi

mv "$DEST.tmp" "$DEST"
chmod 0644 "$DEST"
echo "Done."
