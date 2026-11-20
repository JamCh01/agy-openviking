#!/bin/sh
set -eu

# All progress and diagnostic messages MUST stay on stderr so stdout remains pure
# for Antigravity hook JSON and MCP stdio JSON-RPC frames.
exec 1>&2

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PLUGIN_ROOT_RESOLVED=${PLUGIN_ROOT:-$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)}

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    MANIFEST_FILE="$PLUGIN_ROOT_RESOLVED/.antigravity-plugin/plugin.json"
    if [ -f "$MANIFEST_FILE" ]; then
        VERSION=$(tr -d '\r' < "$MANIFEST_FILE" | awk -F'"' '
            {
                for (i = 1; i <= NF; i++) {
                    if ($i == "version" && (i + 2) <= NF && $(i + 1) ~ /^[[:space:]]*:[[:space:]]*$/) {
                        print $(i + 2)
                        exit
                    }
                }
            }
        ')
    fi
fi

VERSION=${VERSION#v}
if [ -z "$VERSION" ]; then
    echo "[ov-agy] Missing target release version" >&2
    exit 1
fi

OS=$(uname -s)
ARCH=$(uname -m)

BIN_NAME="ov-agy"
ARCHIVE_EXT="tar.gz"
BIN_EXT=""

case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            x86_64)        TARGET="x86_64-apple-darwin" ;;
            *) echo "[ov-agy] Unsupported macOS architecture: $ARCH" >&2; exit 1 ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
            x86_64|amd64)  TARGET="x86_64-unknown-linux-musl" ;;
            *) echo "[ov-agy] Unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
        esac
        ;;
    MINGW*|MSYS*|CYGWIN*)
        case "$ARCH" in
            x86_64|amd64)
                TARGET="x86_64-pc-windows-msvc"
                BIN_NAME="ov-agy.exe"
                ARCHIVE_EXT="zip"
                BIN_EXT=".exe"
                ;;
            *) echo "[ov-agy] Unsupported Windows architecture: $ARCH" >&2; exit 1 ;;
        esac
        ;;
    *)
        echo "[ov-agy] Unsupported operating system: $OS" >&2
        exit 1
        ;;
esac

DEST_BIN="${2:-${OPENVIKING_PLUGIN_DATA:-${PLUGIN_DATA:-$HOME/.openviking/agy-plugin}}/bin/ov-agy-v${VERSION}${BIN_EXT}}"
REPO="${OPENVIKING_AGY_RELEASE_REPO:-JamCh01/agy-openviking}"
RAW_BASE_URL="${OPENVIKING_RELEASE_BASE_URL:-${OPENVIKING_AGY_RELEASE_BASE_URL:-https://github.com/${REPO}/releases/download/v${VERSION}}}"
BASE_URL="${RAW_BASE_URL%/}"

ARCHIVE="ov-agy-v${VERSION}-${TARGET}.${ARCHIVE_EXT}"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ov-agy-fetch.XXXXXX")
STAGE_BIN=""
cleanup() {
    rm -rf "$TMP_DIR"
    if [ -n "$STAGE_BIN" ] && [ -e "$STAGE_BIN" ]; then
        rm -f "$STAGE_BIN"
    fi
}
trap cleanup EXIT INT TERM

echo "[ov-agy] Downloading ${ARCHIVE} from ${BASE_URL}..." >&2
curl -fsSL --connect-timeout 10 --max-time 60 "${BASE_URL}/${ARCHIVE}" -o "$TMP_DIR/$ARCHIVE" >&2
curl -fsSL --connect-timeout 10 --max-time 30 "${BASE_URL}/checksums.txt" -o "$TMP_DIR/checksums.txt" >&2

EXPECTED_SHA=$(tr -d '\r' < "$TMP_DIR/checksums.txt" | awk -v file="$ARCHIVE" '$2 == file || $2 == "*"file { print tolower($1); exit }')
if [ -z "$EXPECTED_SHA" ]; then
    echo "[ov-agy] Missing SHA256 entry for ${ARCHIVE} in checksums.txt" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA=$(sha256sum "$TMP_DIR/$ARCHIVE" | awk '{ print tolower($1) }')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_SHA=$(shasum -a 256 "$TMP_DIR/$ARCHIVE" | awk '{ print tolower($1) }')
else
    echo "[ov-agy] Neither sha256sum nor shasum found for SHA256 verification" >&2
    exit 1
fi

if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
    echo "[ov-agy] SHA256 mismatch for ${ARCHIVE}: expected ${EXPECTED_SHA}, got ${ACTUAL_SHA}" >&2
    exit 1
fi

EXTRACT_DIR="$TMP_DIR/extracted"
mkdir -p "$EXTRACT_DIR"
if [ "$ARCHIVE_EXT" = "zip" ]; then
    if command -v unzip >/dev/null 2>&1; then
        unzip -q "$TMP_DIR/$ARCHIVE" -d "$EXTRACT_DIR" >&2
    else
        tar -xf "$TMP_DIR/$ARCHIVE" -C "$EXTRACT_DIR" >&2
    fi
else
    tar -xzf "$TMP_DIR/$ARCHIVE" -C "$EXTRACT_DIR" >&2
fi

if [ ! -f "$EXTRACT_DIR/$BIN_NAME" ]; then
    echo "[ov-agy] Extracted archive does not contain flat root binary ${BIN_NAME}" >&2
    exit 1
fi

DEST_DIR=$(dirname "$DEST_BIN")
mkdir -p "$DEST_DIR"
STAGE_BIN=$(mktemp "$DEST_DIR/.ov-agy.tmp.XXXXXX")
cp "$EXTRACT_DIR/$BIN_NAME" "$STAGE_BIN"
chmod 0755 "$STAGE_BIN"
mv -f "$STAGE_BIN" "$DEST_BIN"
STAGE_BIN=""

echo "[ov-agy] Installed verified binary at ${DEST_BIN}" >&2
