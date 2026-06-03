#!/usr/bin/env bash
# Assemble a macOS .app bundle around the built shadows-desktop binary.
#
# Usage:
#   make-app.sh <binary-path> <version> <out-app-path>
#
# Example:
#   packaging/macos/make-app.sh \
#     target/aarch64-apple-darwin/release/shadows-desktop \
#     0.2.0 \
#     dist/Relay.app
#
# Emits the bundle at <out-app-path> and ad-hoc signs it (stable
# signature so it runs locally). A Developer ID re-sign + notarization
# can run on the emitted bundle BEFORE make-dmg.sh wraps it — that seam
# is why bundling and dmg-packing are two scripts. See README.md.
#
# The app icon is generated from clawborrator-supervisor/assets/app-icon.png
# (full-color, 1024x1024). The menu-bar status item uses a separate
# all-white icon, assets/tray.png, embedded into the binary.

set -euo pipefail

BIN="${1:?usage: make-app.sh <binary> <version> <out-app>}"
VERSION="${2:?missing version}"
OUT_APP="${3:?missing output .app path}"

# User-facing brand vs. the on-disk executable. The bundle/display name
# is "Relay"; the executable inside stays the cargo bin name so
# current_exe()-based autostart and the CLI subcommands are unaffected.
DISPLAY_NAME="Relay"
BINARY_NAME="relay"
BUNDLE_ID="com.clawborrator.supervisor"   # matches the LaunchAgent label

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# Full-color 1024px app icon (Dock / Finder / Applications). The
# menu-bar status item uses a separate all-white icon (assets/tray.png).
ICON_SRC="$REPO_ROOT/clawborrator-supervisor/assets/app-icon.png"

rm -rf "$OUT_APP"
mkdir -p "$OUT_APP/Contents/MacOS" "$OUT_APP/Contents/Resources"

# ─── binary ─────────────────────────────────────────────────────────
install -m 0755 "$BIN" "$OUT_APP/Contents/MacOS/$BINARY_NAME"

# ─── icon: tray.png -> AppIcon.icns via an .iconset ─────────────────
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
# Apple's canonical iconset sizes (point size + @2x retina variant).
for spec in \
  "16:icon_16x16.png"      "32:icon_16x16@2x.png" \
  "32:icon_32x32.png"      "64:icon_32x32@2x.png" \
  "128:icon_128x128.png"   "256:icon_128x128@2x.png" \
  "256:icon_256x256.png"   "512:icon_256x256@2x.png" \
  "512:icon_512x512.png"   "1024:icon_512x512@2x.png"; do
  px="${spec%%:*}"; name="${spec##*:}"
  sips -z "$px" "$px" "$ICON_SRC" --out "$ICONSET/$name" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$OUT_APP/Contents/Resources/AppIcon.icns"

# ─── Info.plist ─────────────────────────────────────────────────────
# No LSUIElement: a fresh double-click runs the first-run setup wizard,
# which wants a normal app (Dock presence + window focus). Once paired,
# the daemon flips itself to NSApplicationActivationPolicy::Accessory at
# runtime (see tray/macos.rs), so the steady-state menu-bar app has no
# Dock icon anyway.
cat > "$OUT_APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>               <string>$DISPLAY_NAME</string>
  <key>CFBundleDisplayName</key>        <string>$DISPLAY_NAME</string>
  <key>CFBundleIdentifier</key>         <string>$BUNDLE_ID</string>
  <key>CFBundleVersion</key>            <string>$VERSION</string>
  <key>CFBundleShortVersionString</key> <string>$VERSION</string>
  <key>CFBundleExecutable</key>         <string>$BINARY_NAME</string>
  <key>CFBundleIconFile</key>           <string>AppIcon</string>
  <key>CFBundlePackageType</key>        <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
  <key>LSMinimumSystemVersion</key>     <string>11.0</string>
  <key>NSHighResolutionCapable</key>    <true/>
</dict>
</plist>
PLIST

# ─── ad-hoc sign ────────────────────────────────────────────────────
# Stable signature so the bundle runs locally. A Developer ID re-sign
# (release.yml, when secrets are present) supersedes this.
codesign --force --deep --sign - "$OUT_APP" >/dev/null 2>&1 || \
  echo "warning: ad-hoc codesign failed (continuing; bundle is unsigned)"

echo "Built $OUT_APP (version $VERSION)"
