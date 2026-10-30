#!/bin/bash
set -euo pipefail

if [[ $# -ne 1 || ! -e "$1" ]]; then
    echo "Usage: $0 <signed.dmg|signed.zip|signed.app>" >&2
    exit 1
fi
case "$1" in
    *.dmg|*.zip) [[ -f "$1" ]] || exit 1 ;;
    *.app) [[ -d "$1" ]] || exit 1 ;;
    *) echo "Error: expected a DMG, ZIP or app bundle" >&2; exit 1 ;;
esac
if [[ -n "${NOTARY_KEYCHAIN_PROFILE:-}" ]]; then
    CREDENTIALS=(--keychain-profile "$NOTARY_KEYCHAIN_PROFILE")
elif [[ -n "${ASC_KEY_PATH:-}" ]]; then
    : "${ASC_KEY_ID:?ASC_KEY_ID is required}"
    : "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required}"
    CREDENTIALS=(--key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID")
else
    : "${APPLE_ID:?APPLE_ID is required}"
    : "${APPLE_APP_SPECIFIC_PASSWORD:?APPLE_APP_SPECIFIC_PASSWORD is required}"
    : "${APPLE_TEAM_ID:?APPLE_TEAM_ID is required}"
    CREDENTIALS=(--apple-id "$APPLE_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" --team-id "$APPLE_TEAM_ID")
fi

TEMP="$(mktemp -d)"
trap 'rm -rf "$TEMP"' EXIT
SUBMISSION="$1"
if [[ "$1" == *.app ]]; then
    SUBMISSION="$TEMP/application.zip"
    ditto -c -k --sequesterRsrc --keepParent "$1" "$SUBMISSION"
fi
for ATTEMPT in 1 2 3; do
    STATUS=0
    xcrun notarytool submit "$SUBMISSION" "${CREDENTIALS[@]}" --wait --timeout 30m \
        --output-format json > "$TEMP/submission.json" 2> "$TEMP/submission.error" || STATUS=$?
    cat "$TEMP/submission.json"
    cat "$TEMP/submission.error" >&2
    if [[ "$STATUS" -eq 0 || "$ATTEMPT" -eq 3 ]] || ! grep -q NSURLErrorDomain "$TEMP/submission.error"; then
        break
    fi
    echo "Notarization network error; retrying in 5 seconds ($ATTEMPT/3)" >&2
    sleep 5
done
SUBMISSION_ID="$(python3 -c 'import json, sys; print(json.load(sys.stdin).get("id", ""))' \
    < "$TEMP/submission.json" 2>/dev/null || true)"
if [[ -n "$SUBMISSION_ID" ]]; then
    xcrun notarytool log "$SUBMISSION_ID" "${CREDENTIALS[@]}" || true
fi
if [[ "$STATUS" -ne 0 ]]; then
    exit "$STATUS"
fi
python3 -c 'import json, sys; sys.exit(0 if json.load(sys.stdin).get("status") == "Accepted" else 1)' \
    < "$TEMP/submission.json"
# Stapling asks Apple's ticket service over the network; that connection drops now and then
# (stapler exit 68), so each stapler call is retried.
stapler() {
    local attempt
    for attempt in 1 2 3 4 5; do
        xcrun stapler "$@" && return 0
        echo "stapler $1 failed; retrying in 15 seconds ($attempt/5)" >&2
        sleep 15
    done
    return 1
}
case "$1" in
    *.app)
        stapler staple "$1"
        stapler validate "$1"
        codesign --verify --deep --strict --verbose=2 "$1"
        spctl --assess --type execute --verbose=2 "$1"
        ;;
    *.dmg)
        stapler staple "$1"
        stapler validate "$1"
        spctl --assess --type open --context context:primary-signature --verbose=2 "$1"
        ;;
    *.zip)
        # ZIPs and bare executables cannot be stapled. Verify the submitted bytes.
        for BINARY in atur-term atur-term-server; do
            unzip -p "$1" "$BINARY" > "$TEMP/$BINARY"
            chmod 755 "$TEMP/$BINARY"
            codesign --verify --strict --verbose=2 "$TEMP/$BINARY"
            codesign --verify --verbose=4 --requirement notarized --check-notarization "$TEMP/$BINARY"
            "$TEMP/$BINARY" --build-info
        done
        ;;
esac
