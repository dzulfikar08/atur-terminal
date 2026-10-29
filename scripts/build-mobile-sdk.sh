#!/bin/bash
# Builds the mobile SDK for the phone apps: an iOS XCFramework with Swift
# bindings and, when cargo-ndk is installed, Android libraries with Kotlin bindings.
#
# PLATFORMS="android" (or "ios") builds only that half. iOS needs macOS and Xcode; Android builds
# anywhere cargo-ndk and the NDK are installed. Mobile builds drop the release profile's debug
# info, which would make the libraries several times larger.
set -euo pipefail

PLATFORMS="${PLATFORMS:-ios android}"
export CARGO_PROFILE_RELEASE_DEBUG="${CARGO_PROFILE_RELEASE_DEBUG:-0}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/target/mobile-sdk}"
TOOLCHAIN="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml")"
IOS_TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
ANDROID_ABIS=(arm64-v8a armeabi-v7a x86_64)

wants() { [[ " $PLATFORMS " == *" $1 "* ]]; }

if wants ios && [[ "$(uname -s)" != Darwin ]]; then
    echo "Error: building the iOS SDK requires macOS and Xcode" >&2
    exit 1
fi
case "$(uname -s)" in
    Darwin) HOST_LIBRARY=libatur_mobile.dylib ;;
    *) HOST_LIBRARY=libatur_mobile.so ;;
esac

cd "$ROOT"
# Replace only what this script writes; OUT may be a directory the user owns.
wants ios && rm -rf "$OUT/swift" "$OUT/ios" "$OUT/AturMobile.xcframework"
wants android && rm -rf "$OUT/android"
mkdir -p "$OUT"

bindings() {
    local language="$1" destination="$2"
    cargo run --locked -p atur-mobile --features bindgen --bin uniffi-bindgen -- \
        generate --library "$ROOT/target/debug/$HOST_LIBRARY" \
        --language "$language" --out-dir "$destination"
}

# Bindings are generated from a host build; they are identical for every target.
cargo build --locked -p atur-mobile --lib

if wants ios; then
    rustup target add --toolchain "$TOOLCHAIN" "${IOS_TARGETS[@]}"
    for target in "${IOS_TARGETS[@]}"; do
        cargo build --locked --release -p atur-mobile --lib --target "$target"
    done

    bindings swift "$OUT/swift"
    HEADERS="$OUT/ios/headers"
    mkdir -p "$HEADERS" "$OUT/ios/simulator"
    cp "$OUT/swift/atur_mobileFFI.h" "$HEADERS/"
    cp "$OUT/swift/atur_mobileFFI.modulemap" "$HEADERS/module.modulemap"
    lipo -create \
        "target/aarch64-apple-ios-sim/release/libatur_mobile.a" \
        "target/x86_64-apple-ios/release/libatur_mobile.a" \
        -output "$OUT/ios/simulator/libatur_mobile.a"
    xcodebuild -create-xcframework \
        -library "target/aarch64-apple-ios/release/libatur_mobile.a" -headers "$HEADERS" \
        -library "$OUT/ios/simulator/libatur_mobile.a" -headers "$HEADERS" \
        -output "$OUT/AturMobile.xcframework"
fi

if ! wants android; then
    :
elif command -v cargo-ndk >/dev/null; then
    targets=()
    for abi in "${ANDROID_ABIS[@]}"; do
        targets+=(-t "$abi")
    done
    rustup target add --toolchain "$TOOLCHAIN" \
        aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
    cargo ndk "${targets[@]}" -o "$OUT/android/jniLibs" build --locked --release -p atur-mobile --lib
    bindings kotlin "$OUT/android/kotlin"
else
    echo "Skipping Android: install cargo-ndk to build it" >&2
fi

echo "Mobile SDK written to $OUT"
