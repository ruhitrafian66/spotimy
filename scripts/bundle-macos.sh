#!/bin/sh
# Builds dist/Spotimy.app from the release binary.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
APP=dist/Spotimy.app
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/spotimy "$APP/Contents/MacOS/spotimy"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Spotimy</string>
  <key>CFBundleDisplayName</key><string>Spotimy</string>
  <key>CFBundleIdentifier</key><string>local.spotimy</string>
  <key>CFBundleExecutable</key><string>spotimy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# An ad-hoc signature. Apple Silicon does not start unsigned code.
codesign --force --sign - "$APP"
echo "Built $APP"
