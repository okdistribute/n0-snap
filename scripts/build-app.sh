#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --bin flicker
mkdir -p n0-snap.app/Contents/MacOS
cp target/debug/flicker n0-snap.app/Contents/MacOS/flicker
cat > n0-snap.app/Contents/Info.plist <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>n0-snap</string>
<key>CFBundleDisplayName</key><string>n0-snap</string>
<key>CFBundleExecutable</key><string>flicker</string>
<key>CFBundleIdentifier</key><string>dev.flicker.prototype</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSLocalNetworkUsageDescription</key><string>n0-snap uses iroh to connect with your friends and your media host.</string>
</dict></plist>
PLIST
printf 'Built %s/n0-snap.app\n' "$PWD"
