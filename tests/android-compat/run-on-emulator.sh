#!/usr/bin/env bash
set -euo pipefail

# Push the test binary to an Android device/emulator and run it.
#
# Prerequisites:
#   - adb in PATH (from Android SDK platform-tools)
#   - An aarch64 emulator or device connected (verify with `adb devices`)
#   - Test binary built via ./build.sh --test
#
# Usage:
#   ./run-on-emulator.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$SCRIPT_DIR/graft-android-test"

if [ ! -f "$BINARY" ]; then
    echo "ERROR: Test binary not found at $BINARY"
    echo "Run ./build.sh --test first."
    exit 1
fi

ADB="${ADB:-adb}"

echo "==> Checking for connected devices..."
$ADB devices -l

DEVICE_DIR="/data/local/tmp/graft-test"

echo "==> Creating device directory $DEVICE_DIR..."
$ADB shell "mkdir -p $DEVICE_DIR"

echo "==> Pushing test binary..."
$ADB push "$BINARY" "$DEVICE_DIR/graft-android-test"

echo "==> Setting executable permission..."
$ADB shell "chmod +x $DEVICE_DIR/graft-android-test"

echo "==> Running test on device..."
echo "---"
# Run the test binary. Use TMPDIR on device for temp files.
$ADB shell "cd $DEVICE_DIR && TMPDIR=$DEVICE_DIR/tmp RUST_LOG=warn ./graft-android-test"
EXIT_CODE=$?
echo "---"

echo "==> Cleaning up..."
$ADB shell "rm -rf $DEVICE_DIR"

if [ $EXIT_CODE -eq 0 ]; then
    echo "==> ALL TESTS PASSED on Android device"
else
    echo "==> TESTS FAILED (exit code: $EXIT_CODE)"
fi

exit $EXIT_CODE
