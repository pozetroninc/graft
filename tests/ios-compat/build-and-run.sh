#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"
RUST_LIB="$REPO_ROOT/target/aarch64-apple-ios-sim/release/libgraft_ext.a"

echo "=== Step 1: Cross-compile graft-ext for iOS Simulator ==="

DYLD_FALLBACK_LIBRARY_PATH="/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib" \
LIBCLANG_PATH="/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib" \
cargo rustc -p graft-ext \
    --features static \
    --no-default-features \
    --target aarch64-apple-ios-sim \
    --release \
    --crate-type staticlib

echo "Static library: $RUST_LIB"

echo ""
echo "=== Step 2: Build the iOS app ==="

mkdir -p "$BUILD_DIR/GraftTest.app"

SDK_PATH=$(xcrun --sdk iphonesimulator --show-sdk-path)

xcrun --sdk iphonesimulator swiftc \
    -parse-as-library \
    -target arm64-apple-ios16.0-simulator \
    -sdk "$SDK_PATH" \
    -import-objc-header "$SCRIPT_DIR/GraftTest/BridgingHeader.h" \
    -L "$(dirname "$RUST_LIB")" \
    -lgraft_ext \
    -lz \
    -lsqlite3 \
    -lresolv \
    -framework Security \
    -framework SystemConfiguration \
    -framework SwiftUI \
    -framework UIKit \
    "$SCRIPT_DIR/GraftTest/GraftTestApp.swift" \
    "$SCRIPT_DIR/GraftTest/ContentView.swift" \
    "$SCRIPT_DIR/GraftTest/GraftBridge.swift" \
    -o "$BUILD_DIR/GraftTest.app/GraftTest"

# Create Info.plist
cat > "$BUILD_DIR/GraftTest.app/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>GraftTest</string>
    <key>CFBundleIdentifier</key>
    <string>dev.orbitinghail.graft.test</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>GraftTest</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSRequiresIPhoneOS</key>
    <true/>
    <key>UILaunchScreen</key>
    <dict/>
    <key>UIRequiredDeviceCapabilities</key>
    <array>
        <string>arm64</string>
    </array>
    <key>UISupportedInterfaceOrientations</key>
    <array>
        <string>UIInterfaceOrientationPortrait</string>
    </array>
    <key>MinimumOSVersion</key>
    <string>16.0</string>
    <key>DTPlatformName</key>
    <string>iphonesimulator</string>
    <key>CFBundleSupportedPlatforms</key>
    <array>
        <string>iPhoneSimulator</string>
    </array>
</dict>
</plist>
PLIST

echo "App bundle: $BUILD_DIR/GraftTest.app"

echo ""
echo "=== Step 3: Install and launch on simulator ==="

# Find a booted simulator or boot one
DEVICE_ID=$(xcrun simctl list devices booted -j | python3 -c "
import json, sys
data = json.load(sys.stdin)
for runtime, devices in data.get('devices', {}).items():
    for d in devices:
        if d.get('state') == 'Booted':
            print(d['udid'])
            sys.exit(0)
" 2>/dev/null || true)

if [ -z "$DEVICE_ID" ]; then
    echo "No booted simulator found. Booting one..."
    DEVICE_ID=$(xcrun simctl list devices available -j | python3 -c "
import json, sys
data = json.load(sys.stdin)
for runtime, devices in data.get('devices', {}).items():
    if 'iOS' not in runtime:
        continue
    for d in devices:
        if d.get('isAvailable') and 'iPhone' in d.get('name', ''):
            print(d['udid'])
            sys.exit(0)
sys.exit(1)
")
    xcrun simctl boot "$DEVICE_ID"
    echo "Booted simulator: $DEVICE_ID"
    sleep 5
fi

echo "Using simulator: $DEVICE_ID"

xcrun simctl install "$DEVICE_ID" "$BUILD_DIR/GraftTest.app"
echo "App installed"

xcrun simctl launch "$DEVICE_ID" dev.orbitinghail.graft.test
echo "App launched"

echo ""
echo "=== Done ==="
echo "Open the Simulator app to see the test results on screen."
echo "To take a screenshot: xcrun simctl io $DEVICE_ID screenshot screenshot.png"
