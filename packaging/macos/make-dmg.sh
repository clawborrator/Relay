#!/usr/bin/env bash
# Wrap a built .app in a drag-to-Applications .dmg.
#
# Usage:
#   make-dmg.sh <app-path> <out-dmg-path>
#
# Example:
#   packaging/macos/make-dmg.sh \
#     dist/Relay.app \
#     dist/relay-macos-arm64.dmg
#
# The disk image lays out the .app next to an /Applications symlink, so
# opening the .dmg shows the familiar "drag the app onto Applications"
# window. Run AFTER any Developer ID signing of the .app — the dmg is
# built from the bundle exactly as it stands here.

set -euo pipefail

APP="${1:?usage: make-dmg.sh <app-path> <out-dmg>}"
OUT_DMG="${2:?missing output dmg path}"

APP_NAME="$(basename "$APP")"
VOL_NAME="Relay"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

STAGE="$WORK/stage"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/$APP_NAME"
ln -s /Applications "$STAGE/Applications"

mkdir -p "$(dirname "$OUT_DMG")"
rm -f "$OUT_DMG"
hdiutil create \
  -volname "$VOL_NAME" \
  -srcfolder "$STAGE" \
  -fs HFS+ \
  -format UDZO \
  -ov \
  "$OUT_DMG" >/dev/null

echo "Built $OUT_DMG"
