#!/usr/bin/env bash
# Builds an unsigned Reloaded.ipa for iPhone. Needs macOS with Xcode, and Rust with the
# aarch64-apple-ios target. Sideloadly (or AltStore) signs the .ipa with your Apple ID when it
# installs it.
#
#   ios/build-ios.sh            -> ios/build/Reloaded.ipa
#
# RUST_TOOLCHAIN picks the toolchain (default: the one the Android build uses).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-1.99.0}"
TARGET=aarch64-apple-ios
MIN_IOS=14.0
APP_NAME=Reloaded
OUT="$ROOT/ios/build"
APP="$OUT/Payload/$APP_NAME.app"
ASSETS="$ROOT/android/app/src/main/assets"

export IPHONEOS_DEPLOYMENT_TARGET="$MIN_IOS"

rm -rf "$OUT"
mkdir -p "$APP/assets"

echo "==> Motor (Rust, $TARGET)"
cd "$ROOT/src/OpenGMK"
# The crate is a cdylib for Android; iOS apps link everything statically into one executable.
# --print native-static-libs lists the system libraries the Rust side needs at link time.
cargo +"$RUST_TOOLCHAIN" rustc -p gm8emulator --lib --release --target "$TARGET" --crate-type staticlib \
    -- --print native-static-libs 2>&1 | tee "$OUT/cargo.log"
LIB="$ROOT/src/OpenGMK/target/$TARGET/release/libgm8emulator.a"
test -s "$LIB"

NATIVE_LIBS_FILE="$ROOT/src/OpenGMK/target/$TARGET/release/native-static-libs.txt"
NATIVE_LIBS="$(sed -n 's/.*native-static-libs: //p' "$OUT/cargo.log" | tail -n 1)"
if [ -n "$NATIVE_LIBS" ]; then
    echo "$NATIVE_LIBS" > "$NATIVE_LIBS_FILE"
elif [ -s "$NATIVE_LIBS_FILE" ]; then
    # Nothing was recompiled, so rustc printed nothing; reuse what it said last time.
    NATIVE_LIBS="$(cat "$NATIVE_LIBS_FILE")"
else
    NATIVE_LIBS="-framework AudioToolbox -framework Security -framework CoreFoundation -lSystem -lc -lm -liconv"
fi
echo "    bibliotecas del sistema: $NATIVE_LIBS"

echo "==> App (Objective-C)"
SDK="$(xcrun --sdk iphoneos --show-sdk-path)"
CLANG="$(xcrun --sdk iphoneos --find clang)"
# shellcheck disable=SC2086 # NATIVE_LIBS is a list of flags
"$CLANG" -arch arm64 -isysroot "$SDK" -miphoneos-version-min="$MIN_IOS" \
    -fobjc-arc -fmodules -O2 -Wall \
    "$ROOT/ios/host/main.m" "$LIB" \
    -framework UIKit -framework Foundation -framework QuartzCore -framework OpenGLES \
    -framework AVFoundation -framework AudioToolbox \
    $NATIVE_LIBS -lc++ -lz \
    -o "$APP/$APP_NAME"

echo "==> Paquete"
cp "$ROOT/ios/Info.plist" "$APP/Info.plist"
plutil -replace DTPlatformVersion -string "$(xcrun --sdk iphoneos --show-sdk-version)" "$APP/Info.plist"
plutil -replace DTSDKName -string "iphoneos$(xcrun --sdk iphoneos --show-sdk-version)" "$APP/Info.plist"
plutil -convert binary1 "$APP/Info.plist"
printf 'APPL????' > "$APP/PkgInfo"

# What the Android launcher ships for the engine: the touch controls' artwork, the MIDI soundfont
# and the mod manifests. The engine copies them where it expects them on first start.
cp "$ASSETS"/*.png "$APP/assets/"
cp "$ASSETS/soundfont.sf2" "$APP/assets/"
cp -R "$ASSETS/mods" "$APP/assets/mods"

# Ad-hoc signature only; the installer re-signs with the player's Apple ID.
codesign --force --sign - --timestamp=none "$APP"

cd "$OUT"
zip -qry "$APP_NAME.ipa" Payload
echo "==> Listo: $OUT/$APP_NAME.ipa ($(du -h "$APP_NAME.ipa" | cut -f1))"
