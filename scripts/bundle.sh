#!/usr/bin/env bash
# Assemble `File Explorer.app` (and optionally a .dmg) from a release build.
#
# A .app is a directory with an Info.plist, so this is a shell script rather
# than `cargo-bundle` (plan §4). Deviation, deliberate: cargo-bundle is an
# extra pinned dependency for something this short, and a script is debuggable
# and runs identically on your machine and on the CI runner — which matters,
# because "it bundles on CI only" is the failure mode that costs a day.
#
#   scripts/bundle.sh           # build + .app in target/bundle
#   scripts/bundle.sh --dmg     # also produce a .dmg beside it
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="File Explorer"
BUNDLE_ID="dev.noelanderton.file-explorer"
# One source of truth: the workspace version, same string the app reports.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
OUT="target/bundle"
APP="$OUT/$APP_NAME.app"

echo "==> building release binary (version $VERSION)"
cargo build --release -p file-explorer-app --bin file-explorer

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/file-explorer "$APP/Contents/MacOS/file-explorer"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>file-explorer</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleIconFile</key><string>AppIcon</string>
</dict>
</plist>
PLIST

# The icon is optional until M8 draws one; without it macOS shows a generic
# document icon rather than failing to launch.
if [ -f crates/app/assets/AppIcon.icns ]; then
  cp crates/app/assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
else
  echo "    (no AppIcon.icns yet — the bundle gets the generic icon)"
fi

# Ad-hoc sign the assembled bundle.
#
# Not a substitute for a Developer ID — Gatekeeper still calls this an
# unidentified developer. It fixes a different problem: Rust's linker ad-hoc
# signs the bare binary, and that signature stops matching the moment the
# binary is put in a bundle with an Info.plist and a Resources folder.
# `spctl` then reports "code has no resources but signature indicates they
# must be present", and macOS can refuse to run the app at all — or call it
# *damaged* — even when the user tries to allow it. Signing the whole bundle
# makes it internally consistent.
echo "==> ad-hoc signing"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --deep --strict "$APP" && echo "    signature is internally consistent"

echo "==> $APP"

if [ "${1:-}" = "--dmg" ]; then
  DMG="$OUT/FileExplorer-$VERSION.dmg"
  echo "==> packaging $DMG"
  rm -f "$DMG"
  STAGE="$(mktemp -d)"
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$STAGE"
  echo "==> $DMG"
fi
