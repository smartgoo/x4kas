#!/usr/bin/env bash
# Wrap the GUI binary in an x4kas.app bundle, so Finder launches it without a Terminal
# window and shows its icon. Used by the release workflow; runs on macOS (sips, iconutil).
#
#   scripts/macos-app.sh <x4kas binary> <version, e.g. 0.1.0> <output dir>
set -euo pipefail

binary="$1"
version="$2"
out="$3"
icon="$(dirname "$0")/../crates/x4kas-gui/assets/icon.png"

app="${out}/x4kas.app"
rm -rf "$app"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
cp "$binary" "${app}/Contents/MacOS/x4kas"

# The icon at every size Finder asks for, up to the source's 512px.
iconset="$(mktemp -d)/x4kas.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon" --out "${iconset}/icon_${size}x${size}.png" > /dev/null
  double=$((size * 2))
  if [ "$double" -le 512 ]; then
    sips -z "$double" "$double" "$icon" --out "${iconset}/icon_${size}x${size}@2x.png" > /dev/null
  fi
done
iconutil -c icns "$iconset" -o "${app}/Contents/Resources/x4kas.icns"

cat > "${app}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>x4kas</string>
  <key>CFBundleDisplayName</key><string>x4kas</string>
  <key>CFBundleIdentifier</key><string>com.smartgoo.x4kas</string>
  <key>CFBundleExecutable</key><string>x4kas</string>
  <key>CFBundleIconFile</key><string>x4kas</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Sign the finished bundle (ad hoc: no Developer ID). The linker signs the bare binary
# alone, and a bundle whose signature doesn't cover its Info.plist and resources is
# reported as "damaged" by Gatekeeper.
codesign --force --sign - "$app"
