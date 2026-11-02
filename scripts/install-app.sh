#!/bin/bash
# Build CleanCraft and install it as /Applications/CleanCraft.app (macOS).
#
# One binary does everything: opened from Finder or the Dock it is the desktop app; with
# `serve` it is the MCP server agents launch; with other subcommands it is the CLI.
#
# Usage: bash scripts/install-app.sh            (installs to /Applications)
#        APP_DIR=~/Applications bash scripts/install-app.sh
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
APP_DIR="${APP_DIR:-/Applications}"
APP="$APP_DIR/CleanCraft.app"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "This script builds a macOS app bundle; run it on a Mac." >&2
  exit 1
fi
if ! command -v cargo >/dev/null; then
  echo "Rust is not installed. Install it from https://rustup.rs and run this again." >&2
  exit 1
fi

echo "==> Building CleanCraft $VERSION (release)"
export MACOSX_DEPLOYMENT_TARGET=11.0
# The [env] guard in .cargo/config.toml only affects processes cargo starts, not this build.
cargo build --release -p cleancraft

STAGE="$(mktemp -d)/CleanCraft.app"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"
cp "$ROOT/target/release/cleancraft" "$STAGE/Contents/MacOS/CleanCraft"

echo "==> Making icon"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z $size $size "$ROOT/assets/AppIcon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$ROOT/assets/AppIcon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$STAGE/Contents/Resources/CleanCraft.icns"

cat > "$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleName</key><string>CleanCraft</string>
  <key>CFBundleDisplayName</key><string>CleanCraft</string>
  <key>CFBundleIdentifier</key><string>com.cristianizzo.cleancraft</string>
  <key>CFBundleExecutable</key><string>CleanCraft</string>
  <key>CFBundleIconFile</key><string>CleanCraft</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CleanCraftBuildCommit</key><string>${COMMIT}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSRequiresAquaSystemAppearance</key><false/>
  <key>NSAppleEventsUsageDescription</key>
  <string>CleanCraft asks Finder to empty the Trash and System Events to list login items, only when you click those actions.</string>
  <key>NSHumanReadableCopyright</key>
  <string>MIT OR Apache-2.0. CleanCraft, an original app.</string>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>com.cristianizzo.cleancraft</string>
      <key>CFBundleURLSchemes</key>
      <array><string>cleancraft</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# The menu bar agent: a helper app inside CleanCraft.app with its own bundle id and no Dock
# icon (LSUIElement), so `open -b com.cristianizzo.cleancraft` always reaches the main app
# and never the agent. It is the same program; Settings > "Show CleanCraft in the menu bar"
# writes the LaunchAgent that starts it as `CleanCraftAgent agent`. This script never starts
# it.
HELPER="$STAGE/Contents/Library/LoginItems/CleanCraftAgent.app"
mkdir -p "$HELPER/Contents/MacOS" "$HELPER/Contents/Resources"
cp "$ROOT/target/release/cleancraft" "$HELPER/Contents/MacOS/CleanCraftAgent"
cp "$STAGE/Contents/Resources/CleanCraft.icns" "$HELPER/Contents/Resources/CleanCraft.icns"
cat > "$HELPER/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleName</key><string>CleanCraft Menu Bar</string>
  <key>CFBundleIdentifier</key><string>com.cristianizzo.cleancraft.agent</string>
  <key>CFBundleExecutable</key><string>CleanCraftAgent</string>
  <key>CFBundleIconFile</key><string>CleanCraft</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# CLEANCRAFT_SIGN_IDENTITY (a certificate in the keychain) or ad hoc. An ad hoc signature is
# tied to this exact build, so macOS forgets Full Disk Access when the app is replaced; one
# certificate kept from release to release keeps it (release.yml sets this).
SIGN="${CLEANCRAFT_SIGN_IDENTITY:--}"
echo "==> Signing (${CLEANCRAFT_SIGN_IDENTITY:-ad hoc})"
# Inner helper first, then the app around it.
codesign --force --sign "$SIGN" "$HELPER"
codesign --force --deep --sign "$SIGN" "$STAGE"

echo "==> Installing to $APP"
if [[ -e "$APP" ]]; then
  mv "$APP" "$(mktemp -d)/CleanCraft.app.old"
fi
mkdir -p "$APP_DIR"
cp -R "$STAGE" "$APP"
# Let Launch Services know the app and its cleancraft:// links now.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP" >/dev/null 2>&1 || true

mkdir -p "$HOME/.local/bin"
ln -sf "$APP/Contents/MacOS/CleanCraft" "$HOME/.local/bin/cleancraft"

BIN="$APP/Contents/MacOS/CleanCraft"
cat <<DONE

Installed $APP  (open it from Launchpad, Spotlight or the Dock)
Command line: $HOME/.local/bin/cleancraft

For Mail, Safari and Trash data, turn on Full Disk Access:
  System Settings > Privacy & Security > Full Disk Access > CleanCraft

Agents (MCP):
  claude mcp add -s user cleancraft -- "$BIN" serve

Menu bar: off until you turn on Settings > "Show CleanCraft in the menu bar". When it is
already on, the new version runs there from your next login (this script never starts it).
Open a page from Terminal: cleancraft open quick_sweep
DONE
