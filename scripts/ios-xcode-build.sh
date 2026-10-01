#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
case "$PLATFORM_NAME" in
  iphoneos) snap_target=aarch64-apple-ios ;;
  iphonesimulator)
    case "$NATIVE_ARCH_ACTUAL" in
      arm64) snap_target=aarch64-apple-ios-sim ;;
      *) snap_target=x86_64-apple-ios ;;
    esac ;;
  *) echo "Expected an iOS device or simulator target" >&2; exit 1 ;;
esac
snap_profile=debug
if [ "$CONFIGURATION" = Release ]; then
  snap_profile=release
  cargo build --locked --release --no-default-features --features mobile --target "$snap_target" --bin flicker
else
  cargo build --locked --no-default-features --features mobile --target "$snap_target" --bin flicker
fi
mkdir -p "$TARGET_BUILD_DIR/$WRAPPER_NAME"
cp "target/$snap_target/$snap_profile/flicker" "$TARGET_BUILD_DIR/$EXECUTABLE_PATH"
