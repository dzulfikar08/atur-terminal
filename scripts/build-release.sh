#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH=""
VERSION=""
BUILD_NUMBER=""
SIGN_IDENTITY="-"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch|--version|--build-number|--sign-identity)
            if [[ $# -lt 2 || -z "$2" ]]; then
                echo "Error: $1 requires a value" >&2
                exit 1
            fi
            case "$1" in
                --arch) ARCH="$2" ;;
                --version) VERSION="$2" ;;
                --build-number) BUILD_NUMBER="$2" ;;
                --sign-identity) SIGN_IDENTITY="$2" ;;
            esac
            shift 2
            ;;
        *)
            echo "Usage: $0 --arch <arm64|x86_64> --version <X.Y.Z-beta.N | X.Y.Z --build-number N> [--sign-identity <identity>]" >&2
            exit 1
            ;;
    esac
done

case "$ARCH" in
    arm64) TARGET="aarch64-apple-darwin" ;;
    x86_64) TARGET="x86_64-apple-darwin" ;;
    *) echo "Error: arch must be arm64 or x86_64" >&2; exit 1 ;;
esac
ICON_SOURCE="$ROOT/packaging/macos/AppIcon.png"
APP_NAME="$(python3 "$ROOT/scripts/release.py" app-name "$VERSION")"
if [[ "$(uname -s)" != Darwin ]]; then
    echo "Error: packaging requires macOS and Xcode" >&2
    exit 1
fi

export MACOSX_DEPLOYMENT_TARGET=14.0
export LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast
export CARGO_TARGET_DIR="$ROOT/target"
export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed
ATUR_TERM_ZIG="$(command -v zig)"
export ATUR_TERM_ZIG
export PATH="$ROOT/scripts/zig:$PATH"
OUTPUT_DIR="$CARGO_TARGET_DIR/packages/$VERSION/$ARCH"
if [[ -e "$OUTPUT_DIR" ]]; then
    echo "Error: output already exists: $OUTPUT_DIR" >&2
    exit 1
fi

cd "$ROOT"
echo "==> Building app, CLI and server for $TARGET"
cargo clean -p libghostty-vt-sys
cargo build --locked --release --target "$TARGET" -p muxy-app -p muxy-cli -p muxy-server
BIN_DIR="$CARGO_TARGET_DIR/$TARGET/release"

mkdir -p "$(dirname "$OUTPUT_DIR")"
STAGING="$(mktemp -d "$(dirname "$OUTPUT_DIR")/.${ARCH}.XXXXXX")"
trap 'rm -rf "$STAGING"' EXIT
APP="$STAGING/dmg/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$STAGING/symbols" "$STAGING/artifacts"

for BINARY in atur-terminal atur-term atur-term-server; do
    install -m 755 "$BIN_DIR/$BINARY" "$APP/Contents/MacOS/$BINARY"
    EXECUTABLE="$APP/Contents/MacOS/$BINARY"
    if [[ "$(lipo -archs "$EXECUTABLE")" != "$ARCH" ]]; then
        echo "Error: $BINARY does not contain exactly $ARCH" >&2
        exit 1
    fi
    # A release must not depend on Homebrew or Cargo build-directory dylibs.
    otool -L "$EXECUTABLE" > "$STAGING/artifacts/$BINARY-dependencies.txt"
    while IFS= read -r LIBRARY; do
        case "$LIBRARY" in
            /System/Library/*|/usr/lib/*) ;;
            *) echo "Error: unbundled dependency in $BINARY: $LIBRARY" >&2; exit 1 ;;
        esac
    done < <(tail -n +2 "$STAGING/artifacts/$BINARY-dependencies.txt" | awk '{print $1}')
    ditto "$BIN_DIR/$BINARY.dSYM" "$STAGING/symbols/$BINARY.dSYM"
    strip -Sx "$EXECUTABLE"
done

python3 "$ROOT/scripts/release.py" plist "$VERSION" "$APP/Contents/Info.plist" ${BUILD_NUMBER:+--build "$BUILD_NUMBER"}
plutil -lint "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
python3 "$ROOT/scripts/release.py" licenses > "$APP/Contents/Resources/LICENSE"

ICONSET="$STAGING/AppIcon.iconset"
mkdir -p "$ICONSET"
for SIZE in 16 32 128 256 512; do
    sips -z "$SIZE" "$SIZE" "$ICON_SOURCE" \
        --out "$ICONSET/icon_${SIZE}x${SIZE}.png" >/dev/null
    DOUBLE=$((SIZE * 2))
    sips -z "$DOUBLE" "$DOUBLE" "$ICON_SOURCE" \
        --out "$ICONSET/icon_${SIZE}x${SIZE}@2x.png" >/dev/null
done
iconutil --convert icns --output "$APP/Contents/Resources/AppIcon.icns" "$ICONSET"

SIGN_ARGS=(--force --sign "$SIGN_IDENTITY")
if [[ "$SIGN_IDENTITY" == - ]]; then
    SIGN_ARGS+=(--timestamp=none)
else
    SIGN_ARGS+=(--options runtime --timestamp)
fi
# Sign every executable before sealing the bundle.
for BINARY in atur-term atur-term-server atur-terminal; do
    if [[ "$BINARY" == atur-terminal ]]; then
        codesign "${SIGN_ARGS[@]}" --entitlements "$ROOT/packaging/macos/AturTerminal.entitlements" "$APP/Contents/MacOS/$BINARY"
    else
        codesign "${SIGN_ARGS[@]}" "$APP/Contents/MacOS/$BINARY"
    fi
done
codesign "${SIGN_ARGS[@]}" --entitlements "$ROOT/packaging/macos/AturTerminal.entitlements" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
for BINARY in atur-term atur-term-server; do
    python3 "$ROOT/scripts/release.py" check-build "$VERSION" "$APP/Contents/MacOS/$BINARY"
done

# Copy the sealed executables without stripping or signing them again.
mkdir "$STAGING/cli"
cp "$APP/Contents/MacOS/atur-term" "$APP/Contents/MacOS/atur-term-server" "$APP/Contents/Resources/LICENSE" "$STAGING/cli/"
CLI_ARCHIVE="$STAGING/artifacts/atur-terminal-${VERSION}-macos-${ARCH}.zip"
(cd "$STAGING/cli" && zip -X "$CLI_ARCHIVE" atur-term atur-term-server LICENSE)

ln -s /Applications "$STAGING/dmg/Applications"
DMG="$STAGING/artifacts/AturTerminal-${VERSION}-${ARCH}.dmg"
# CI runners intermittently fail with "hdiutil: create failed - Resource busy".
for ATTEMPT in 1 2 3 4 5; do
    rm -f "$DMG"
    if hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING/dmg" -format UDZO -fs HFS+ "$DMG"; then
        break
    fi
    if [[ "$ATTEMPT" == 5 ]]; then
        echo "Error: hdiutil create failed after $ATTEMPT attempts" >&2
        exit 1
    fi
    sleep $((ATTEMPT * 5))
done
codesign "${SIGN_ARGS[@]}" "$DMG"
codesign --verify --strict --verbose=2 "$DMG"
ditto -c -k --sequesterRsrc --keepParent "$STAGING/symbols" \
    "$STAGING/artifacts/AturTerminal-${VERSION}-${ARCH}-symbols.zip"
mv "$APP" "$STAGING/artifacts/"
mv "$STAGING/artifacts" "$OUTPUT_DIR"
echo "==> Built $OUTPUT_DIR"
