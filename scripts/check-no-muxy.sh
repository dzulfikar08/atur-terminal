#!/bin/bash
# Lists every "muxy" (any case) in built artifacts: binaries, .app bundles, .deb, .AppImage,
# .tar.gz/.zip/.dmg-extracted folders, or plain directories. Names of files and the strings
# inside them are both checked.
#
# Allowed: LICENSE*, NOTICE*, THIRD_PARTY* files and the credit line "Based on Muxy".
# Rust crate paths and symbols (crates/muxy-*, muxy_core::, ...) are reported separately as
# "internal"; they only come from debug info and panic locations and do not make the check fail
# unless --strict is given.
#
# Usage: scripts/check-no-muxy.sh [--strict] [--show N] ARTIFACT...
# Exit: 0 clean, 1 visible hits (or any hit with --strict), 2 usage or extraction error.
set -uo pipefail

STRICT=0
SHOW=5
ARGS=()
while (($#)); do
    case "$1" in
        --strict) STRICT=1 ;;
        --show) SHOW="${2:?--show needs a number}"; shift ;;
        -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) ARGS+=("$1") ;;
    esac
    shift
done
if ((${#ARGS[@]} == 0)); then
    echo "Usage: $0 [--strict] [--show N] ARTIFACT..." >&2
    exit 2
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

ALLOWED_FILE='(^|/)(LICENSE|NOTICE|THIRD_PARTY)[^/]*$'
ALLOWED_LINE='Based on Muxy'
INTERNAL='crates/muxy-|muxy[_-](app-core|app|cli|client|core|protocol|server|terminal|ui|mobile)\b|muxy_[a-z_]+(::|\.rs)|muxy-app/muxy'

visible=0
internal=0

extract() {
    local source="$1" dest="$2"
    mkdir -p "$dest"
    case "$source" in
        *.deb) dpkg-deb -x "$source" "$dest" ;;
        *.zip) unzip -q "$source" -d "$dest" ;;
        *.tar.gz|*.tgz|*.tar.xz|*.tar) tar -xf "$source" -C "$dest" ;;
        *.AppImage) (cd "$dest" && cp "$source" ./image && chmod +x ./image && ./image --appimage-extract >/dev/null 2>&1) ;;
        *.dmg) 7z x -y -o"$dest" "$source" >/dev/null ;;
        *) return 1 ;;
    esac
}

report() {
    local label="$1" file="$2" rel="$3"
    # grep -a reads binaries as text; -o keeps a little context per hit.
    local hits
    hits="$(grep -a -o -i -E '[[:print:]]{0,40}muxy[[:print:]]{0,40}' "$file" 2>/dev/null | sort -u)"
    [[ -n "$hits" ]] || return 0
    local bad
    bad="$(grep -v -E "$ALLOWED_LINE" <<<"$hits" || true)"
    [[ -n "$bad" ]] || return 0
    local inner outer
    inner="$(grep -E -i "$INTERNAL" <<<"$bad" || true)"
    outer="$(grep -v -E -i "$INTERNAL" <<<"$bad" || true)"
    if [[ -n "$outer" ]]; then
        visible=$((visible + 1))
        printf 'VISIBLE %s: %s (%s unique)\n' "$label" "$rel" "$(wc -l <<<"$outer")"
        head -n "$SHOW" <<<"$outer" | sed 's/^/    /'
    fi
    if [[ -n "$inner" ]]; then
        internal=$((internal + 1))
        printf 'internal %s: %s (%s unique)\n' "$label" "$rel" "$(wc -l <<<"$inner")"
        ((STRICT)) && head -n "$SHOW" <<<"$inner" | sed 's/^/    /'
    fi
}

scan_tree() {
    local root="$1" label="$2" file rel
    while IFS= read -r -d '' file; do
        rel="${file#"$root"/}"
        if [[ "$rel" =~ [Mm][Uu][Xx][Yy] && ! "$rel" =~ $ALLOWED_FILE ]]; then
            visible=$((visible + 1))
            printf 'VISIBLE %s: file name %s\n' "$label" "$rel"
        fi
        [[ "$rel" =~ $ALLOWED_FILE ]] && continue
        [[ -L "$file" ]] && continue
        report "$label" "$file" "$rel"
    done < <(find "$root" \( -type f -o -type l \) -print0)
}

index=0
for artifact in "${ARGS[@]}"; do
    index=$((index + 1))
    if [[ -d "$artifact" ]]; then
        scan_tree "${artifact%/}" "$(basename "$artifact")"
    elif [[ -f "$artifact" ]]; then
        name="$(basename "$artifact")"
        if [[ "$name" =~ [Mm][Uu][Xx][Yy] ]]; then
            visible=$((visible + 1))
            printf 'VISIBLE artifact name: %s\n' "$name"
        fi
        dest="$WORK/$index"
        if extract "$artifact" "$dest" 2>/dev/null; then
            scan_tree "$dest" "$name"
        else
            report "$name" "$artifact" "$name"
        fi
        rm -rf "$dest"
    else
        echo "error: $artifact is not a file or directory" >&2
        exit 2
    fi
done

echo "visible: $visible file(s); internal-only: $internal file(s)"
((visible == 0)) && ((!STRICT || internal == 0))
