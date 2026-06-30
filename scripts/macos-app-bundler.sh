#!/usr/bin/env bash
# Build a macOS .app bundle for lorebird with a proper Finder/Dock icon.
#
# The Cargo-built binary links GTK from the Nix store (absolute install
# names) but still needs the dev-shell environment (XDG_DATA_DIRS,
# GDK_PIXBUF_MODULE_FILE, plus PATH for `pass`/`gpg` used by the send hook).
# So the bundle launcher re-enters the Nix dev shell via `nix develop -c`.
#
# Usage: scripts/macos-app-bundler.sh [debug|release]   (default: debug)
set -euo pipefail

PROFILE="${1:-debug}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RES="$ROOT/crates/lorebird-gtk/resources"
BIN="$ROOT/target/$PROFILE/lorebird"
APP_NAME="LoreBird"
BUNDLE_ID="org.lorebird.app"
APP="$ROOT/dist/$APP_NAME.app"

[[ -x "$BIN" ]] || { echo "Error: binary not found: $BIN (build it first)"; exit 1; }
command -v iconutil >/dev/null && command -v sips >/dev/null \
    || { echo "Error: iconutil/sips not found (macOS only)"; exit 1; }

echo "==> Generating $APP_NAME.icns"
# macOS-compliant squircle master (macOS does not mask icons).
MASTER="$RES/macos/lorebird-macos-1024.png"
[[ -f "$MASTER" ]] || { echo "Error: macOS icon master not found: $MASTER"; exit 1; }
ICONSET="$(mktemp -d)/$APP_NAME.iconset"
mkdir -p "$ICONSET"
# Apple's required iconset slots, all downscaled from the 1024 master.
gen() { sips -z "$1" "$1" "$MASTER" --out "$ICONSET/$2.png" >/dev/null; }
gen 16   icon_16x16
gen 32   icon_16x16@2x
gen 32   icon_32x32
gen 64   icon_32x32@2x
gen 128  icon_128x128
gen 256  icon_128x128@2x
gen 256  icon_256x256
gen 512  icon_256x256@2x
gen 512  icon_512x512
cp "$MASTER" "$ICONSET/icon_512x512@2x.png"

echo "==> Assembling bundle: $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/$APP_NAME.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>     <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>      <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>         <string>0.1.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleExecutable</key>      <string>$APP_NAME</string>
    <key>CFBundleIconFile</key>        <string>$APP_NAME</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>
</dict>
</plist>
PLIST

# Launcher: re-enter the Nix dev shell so GTK finds its runtime data, then
# exec the built binary. The app sets its own Dock icon at runtime too
# (see crates/lorebird-gtk/src/platform.rs).
cat > "$APP/Contents/MacOS/$APP_NAME" <<LAUNCH
#!/bin/bash
PROJECT="$ROOT"
PROFILE="$PROFILE"
exec >>"\$HOME/Library/Logs/lorebird.log" 2>&1
if [ -e /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ]; then
  . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
fi
cd "\$PROJECT" || exit 1
BIN="\$PROJECT/target/release/lorebird"
[ -x "\$BIN" ] || BIN="\$PROJECT/target/debug/lorebird"
exec nix develop -c "\$BIN"
LAUNCH
chmod +x "$APP/Contents/MacOS/$APP_NAME"

# Refresh Finder/LaunchServices icon cache for this bundle.
touch "$APP"
echo "==> Done: $APP"
echo "    Open it:  open \"$APP\""
