#!/usr/bin/env bash
set -euo pipefail

# Build Graft Android compatibility test artifacts.
#
# Prerequisites:
#   - Android NDK at ~/Library/Android/sdk/ndk/30.0.14904198
#   - cargo-ndk v4.1.2+ installed
#   - aarch64-linux-android Rust target installed
#
# Usage:
#   ./build.sh           # build everything (test binary + JNI lib + graft-ext)
#   ./build.sh --test    # build only the adb-pushable test binary
#   ./build.sh --jni     # build only the JNI bridge library (for the Android app)
#   ./build.sh --lib     # build only graft-ext shared library

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$HOME/Library/Android/sdk/ndk/30.0.14904198}"
export ANDROID_NDK_HOME

# Ensure libclang is findable (macOS with CommandLineTools or Homebrew)
if [ -z "${DYLD_LIBRARY_PATH:-}" ]; then
    if [ -f "/Library/Developer/CommandLineTools/usr/lib/libclang.dylib" ]; then
        export DYLD_LIBRARY_PATH="/Library/Developer/CommandLineTools/usr/lib"
    elif [ -f "/opt/homebrew/opt/llvm/lib/libclang.dylib" ]; then
        export DYLD_LIBRARY_PATH="/opt/homebrew/opt/llvm/lib"
    fi
fi

TARGET="aarch64-linux-android"
PLATFORM=24

# Find llvm-strip from NDK
NDK_STRIP="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-strip"
if [ ! -f "$NDK_STRIP" ]; then
    NDK_STRIP="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-aarch64/bin/llvm-strip"
fi
if [ ! -f "$NDK_STRIP" ]; then
    NDK_STRIP="$(command -v llvm-strip 2>/dev/null || echo strip)"
fi

BUILD_LIB=false
BUILD_TEST=false
BUILD_JNI=false

case "${1:-all}" in
    --lib)  BUILD_LIB=true ;;
    --test) BUILD_TEST=true ;;
    --jni)  BUILD_JNI=true ;;
    all)    BUILD_LIB=true; BUILD_TEST=true; BUILD_JNI=true ;;
    *)      echo "Usage: $0 [--lib|--test|--jni]"; exit 1 ;;
esac

if [ "$BUILD_LIB" = true ]; then
    echo "==> Building graft-ext shared library for $TARGET..."
    cd "$REPO_ROOT"
    cargo ndk -t "$TARGET" --platform "$PLATFORM" -- \
        build -p graft-ext --features static --no-default-features --release

    SO_PATH="$REPO_ROOT/target/$TARGET/release/libgraft_ext.so"
    JNILIB_DIR="$SCRIPT_DIR/app/src/main/jniLibs/arm64-v8a"
    mkdir -p "$JNILIB_DIR"

    echo "==> Stripping $SO_PATH..."
    "$NDK_STRIP" "$SO_PATH" -o "$JNILIB_DIR/libgraft_ext.so"
    ls -lh "$JNILIB_DIR/libgraft_ext.so"
    echo "==> Shared library ready at $JNILIB_DIR/libgraft_ext.so"
fi

if [ "$BUILD_JNI" = true ]; then
    echo "==> Building JNI bridge library for $TARGET..."
    cd "$SCRIPT_DIR/jni-bridge"
    cargo ndk -t "$TARGET" --platform "$PLATFORM" -- build --release

    SO_PATH="$SCRIPT_DIR/jni-bridge/target/$TARGET/release/libgraft_android_jni.so"
    JNILIB_DIR="$SCRIPT_DIR/app/src/main/jniLibs/arm64-v8a"
    mkdir -p "$JNILIB_DIR"

    echo "==> Stripping JNI library..."
    "$NDK_STRIP" "$SO_PATH" -o "$JNILIB_DIR/libgraft_android_jni.so"
    ls -lh "$JNILIB_DIR/libgraft_android_jni.so"
    echo "==> JNI library ready at $JNILIB_DIR/libgraft_android_jni.so"
fi

if [ "$BUILD_TEST" = true ]; then
    echo "==> Building test binary for $TARGET..."
    cd "$SCRIPT_DIR/rust-test"
    cargo ndk -t "$TARGET" --platform "$PLATFORM" -- build --release

    BIN_PATH="$SCRIPT_DIR/rust-test/target/$TARGET/release/graft-android-test"
    echo "==> Stripping test binary..."
    "$NDK_STRIP" "$BIN_PATH" -o "$SCRIPT_DIR/graft-android-test"
    ls -lh "$SCRIPT_DIR/graft-android-test"
    echo "==> Test binary ready at $SCRIPT_DIR/graft-android-test"
fi

echo ""
echo "Build complete."
[ "$BUILD_TEST" = true ] && echo "  Run ./run-on-emulator.sh to test via adb push."
[ "$BUILD_JNI" = true ] && echo "  The Android app can be built with: cd $SCRIPT_DIR && ./gradlew :app:assembleDebug"
