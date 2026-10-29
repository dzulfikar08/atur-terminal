#!/bin/bash
# CI helpers for Developer ID signing and notarization. Secrets arrive through
# the environment; nothing is printed.
#
#   ci-macos-sign.sh import            import the Developer ID identity into a temporary keychain
#                                      and export SIGN_IDENTITY (via $GITHUB_ENV)
#   ci-macos-sign.sh notarize FILE...  notarize, staple (DMG) and verify each file
#   ci-macos-sign.sh cleanup           delete the temporary keychain
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEYCHAIN="${RUNNER_TEMP:-/tmp}/atur-terminal-signing.keychain-db"

case "${1:-}" in
    import)
        : "${MACOS_DEVELOPER_ID_P12_BASE64:?}" "${MACOS_DEVELOPER_ID_P12_PASSWORD:?}"
        PASSWORD="$(uuidgen)"
        P12="${RUNNER_TEMP:-/tmp}/developer-id.p12"
        printf '%s' "$MACOS_DEVELOPER_ID_P12_BASE64" | base64 --decode > "$P12"
        security create-keychain -p "$PASSWORD" "$KEYCHAIN"
        security set-keychain-settings -lut 21600 "$KEYCHAIN"
        security unlock-keychain -p "$PASSWORD" "$KEYCHAIN"
        security import "$P12" -k "$KEYCHAIN" -P "$MACOS_DEVELOPER_ID_P12_PASSWORD" \
            -T /usr/bin/codesign -T /usr/bin/security >/dev/null
        rm -f "$P12"
        security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PASSWORD" "$KEYCHAIN" >/dev/null
        # Put the keychain first in the search list so codesign finds the identity.
        security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')
        IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" \
            | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1)"
        if [[ -z "$IDENTITY" ]]; then
            echo "Error: no Developer ID Application identity in the imported p12" >&2
            exit 1
        fi
        echo "Signing identity: $IDENTITY"
        echo "SIGN_IDENTITY=$IDENTITY" >> "$GITHUB_ENV"
        ;;
    notarize)
        shift
        : "${ASC_KEY_BASE64:?}" "${ASC_KEY_ID:?}" "${ASC_ISSUER_ID:?}"
        ASC_KEY_PATH="${RUNNER_TEMP:-/tmp}/AuthKey_${ASC_KEY_ID}.p8"
        printf '%s' "$ASC_KEY_BASE64" | base64 --decode > "$ASC_KEY_PATH"
        chmod 600 "$ASC_KEY_PATH"
        export ASC_KEY_PATH
        trap 'rm -f "$ASC_KEY_PATH"' EXIT
        for FILE in "$@"; do
            echo "==> Notarizing $FILE"
            bash "$ROOT/scripts/notarize-release.sh" "$FILE"
        done
        ;;
    cleanup)
        security delete-keychain "$KEYCHAIN" 2>/dev/null || true
        ;;
    *)
        echo "Usage: $0 import | notarize FILE... | cleanup" >&2
        exit 1
        ;;
esac
