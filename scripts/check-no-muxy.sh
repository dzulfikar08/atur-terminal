#!/bin/bash
# Lists every "muxy" (any case) in built artifacts: binaries, .app bundles, .deb, .AppImage,
# .tar.gz/.zip/.dmg-extracted folders, or plain directories. Names of files and the strings
# inside them are both checked.
#
# Allowed: LICENSE*, NOTICE*, THIRD_PARTY* files and the credit line "Based on Muxy".
# Hits are sorted into three groups:
#   internal    Rust crate paths, panic locations and mangled symbols (crates/muxy-*, muxy_core::,
#               muxy_dir17h...). They never reach a user or the OS.
#   legacy-read the names the app only READS so installs and peers from before the rename keep
#               working (.muxy/ folders, MUXY_* variables, muxy:// links, MUXY-STDIO handshakes,
#               old theme and data directory names, the "muxy" manifest key, MUXYSAV file magic,
#               extension JS aliases). Bare "muxy" literals of at most 16 characters with no
#               space are the same legacy keys with unrelated neighbouring bytes.
#   VISIBLE     anything else: thread names, messages, UI strings, paths written to disk,
#               Info.plist, .desktop, deb control. These fail the check.
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
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

ALLOWED_FILE='(^|/)(LICENSE|NOTICE|THIRD_PARTY)[^/]*$|(^|/)usr/share/doc/[^/]+/copyright$'
ALLOWED_LINE='Based on Muxy'
LEGACY='\.muxy/|/muxy/|\.config/muxy|muxy/(worktree|voice|layouts)|MUXY_|MUXY-(STDIO|UDP)|MUXYSAV|muxy://|muxy/1|pending-import\.muxy|com\.muxy|app\.muxy|--muxy-|__muxy|muxy-(beta|dev|pr-number|ext|pane|[0-9a-f]{8})|Muxy (Dev|Beta|2|Light|Runtime|1\.x)|muxy\.(configuration|theme|showStatusBar|tips|general|worktrees|sidebarCollapsedStyle|activeSidebar|localization|appLayout|richInput|quickTerminal|recording|ai|projectPicker|tabs|ext)|muxy\.[a-z]+ =|muxy: |(const|globalThis\.atur =|return|of Object\.values\() ?\(?muxy|`muxy`|`--muxy|\(c\) 2026 Muxy|muxy-cli|Application Support/Muxy|(window|globalThis)\.(atur = )?(window|globalThis)?\.?muxy|[(\[ ]muxy\.[a-z]+[.(]|window\.atur = window\.muxy|^[[:space:]]*(//.*)?muxy[.[:space:]]'
INTERNAL='crates/muxy-|muxy_[a-z0-9_]+|muxy-app/(muxy|gpui)|/muxy-[a-z-]+/src/'

visible=0
internal=0
legacy=0

extract() {
    local source dest="$2"
    source="$(realpath "$1")"
    mkdir -p "$dest"
    case "$source" in
        *.deb) dpkg-deb -x "$source" "$dest" ;;
        *.zip) unzip -q "$source" -d "$dest" ;;
        *.tar.gz|*.tgz|*.tar.xz|*.tar) tar -xf "$source" -C "$dest" ;;
        *.AppImage) (cd "$dest" && cp "$source" ./image && chmod +x ./image && ./image --appimage-extract >&2 && rm -f ./image) ;;
        *.dmg)
            if [[ "$(uname)" == Darwin ]]; then
                local mount="$WORK/mount-$$-$RANDOM"
                mkdir -p "$mount"
                hdiutil attach -nobrowse -readonly -mountpoint "$mount" "$source" >&2 || return 1
                cp -R "$mount"/. "$dest"/ >&2
                hdiutil detach "$mount" -force >&2 || true
            else
                7z x -y -o"$dest" "$source" >&2
            fi ;;
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
    local inner outer old
    inner="$(grep -E -i "$INTERNAL" <<<"$bad" || true)"
    outer="$(grep -v -E -i "$INTERNAL" <<<"$bad" || true)"
    old="$(grep -E -i "$LEGACY" <<<"$outer" || true)"
    outer="$(grep -v -E -i "$LEGACY" <<<"$outer" || true)"
    # A bare word fused with neighbouring bytes (muxypeer, ;Muxyut) is a one-word legacy key; a
    # "Muxy" followed by another word stays visible.
    bare="$(grep -E -i '(^|[^[:alnum:]_./:-])[[:alnum:];]*muxy[[:alnum:];]*($|[^[:alnum:]_./:[:space:]-]|[[:space:]]*$)' <<<"$outer" | grep -v -E -i 'muxy[[:space:]]+[[:alnum:]]' || true)"
    # The lowercase directory name "muxy" of the old Linux data directory sits next to unrelated
    # literals; it is legacy unless it forms a name with - _ . / : or starts a sentence-like word run.
    lower="$(python3 -c '
import re, sys
bad = re.compile(r"(?i:muxy)(?<!muxy)|[-_./:]muxy|muxy[-_./:]")
for line in sys.stdin:
    line = line.rstrip("\n")
    if "muxy" in line and not bad.search(line):
        print(line)
' <<<"$outer" || true)"
    [[ -n "$lower" ]] && bare="$bare"$'\n'"$lower"
    bare="$(sed '/^$/d' <<<"$bare")"
    if [[ -n "$bare" ]]; then
        old="$old"$'\n'"$bare"
        outer="$(grep -v -x -F -f <(printf '%s\n' "$bare") <<<"$outer" || true)"
    fi
    [[ -n "$old" ]] && legacy=$((legacy + 1))
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
        if extract "$artifact" "$dest" 2>"$WORK/extract.err" && [[ -n "$(find "$dest" -type f -print -quit)" ]]; then
            scan_tree "$dest" "$name"
        elif [[ "$name" =~ \.(deb|zip|tar|tar\.gz|tgz|tar\.xz|AppImage|dmg)$ ]]; then
            echo "error: could not unpack $name" >&2
            head -n 20 "$WORK/extract.err" >&2
            exit 2
        else
            report "$name" "$artifact" "$name"
        fi
        rm -rf "$dest"
    else
        echo "error: $artifact is not a file or directory" >&2
        exit 2
    fi
done

echo "visible: $visible file(s); internal: $internal file(s); legacy-read: $legacy file(s)"
((visible == 0)) && ((!STRICT || internal == 0))
