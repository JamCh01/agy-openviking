#!/bin/sh
set -eu

# All progress and diagnostic messages MUST stay on stderr so stdout remains pure
# for Antigravity hook JSON and MCP stdio JSON-RPC frames.
exec 1>&2

VERSION="${1:-0.1.0}"
DEST_BIN="${2:-${OPENVIKING_PLUGIN_DATA:-$HOME/.openviking/agy-plugin}/bin/ov-agy-v${VERSION}}"
REPO="${OPENVIKING_AGY_RELEASE_REPO:-JamCh01/agy-openviking}"
BASE_URL="${OPENVIKING_AGY_RELEASE_BASE_URL:-https://github.com/${REPO}/releases/download/v${VERSION}}"

OS=$(uname -s)
ARCH=$(uname -m)

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
    *)
        echo "[ov-agy] Unsupported operating system: $OS" >&2
        exit 1
        ;;
esac

ARCHIVE="ov-agy-v${VERSION}-${TARGET}.tar.gz"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ov-agy-fetch.XXXXXX")
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

echo "[ov-agy] Downloading ${ARCHIVE} from ${BASE_URL}..." >&2
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "$TMP_DIR/$ARCHIVE" >&2
curl -fsSL "${BASE_URL}/checksums.txt" -o "$TMP_DIR/checksums.txt" >&2

EXPECTED_SHA=$(awk -v file="$ARCHIVE" '$2 == file || $2 == "*"file { print $1; exit }' "$TMP_DIR/checksums.txt")
if [ -z "$EXPECTED_SHA" ]; then
    echo "[ov-agy] Missing SHA256 entry for ${ARCHIVE} in checksums.txt" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA=$(sha256sum "$TMP_DIR/$ARCHIVE" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_SHA=$(shasum -a 256 "$TMP_DIR/$ARCHIVE" | awk '{ print $1 }')
else
    echo "[ov-agy] Neither sha256sum nor shasum found for SHA256 verification" >&2
    exit 1
fi

if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
    echo "[ov-agy] SHA256 mismatch for ${ARCHIVE}: expected ${EXPECTED_SHA}, got ${ACTUAL_SHA}" >&2
    exit 1
fi

tar -xzf "$TMP_DIR/$ARCHIVE" -C "$TMP_DIR" >&2

DEST_DIR=$(dirname "$DEST_BIN")
mkdir -p "$DEST_DIR"
TMP_BIN="${DEST_BIN}.tmp.$$"
cp "$TMP_DIR/ov-agy" "$TMP_BIN"
chmod +x "$TMP_BIN"
mv "$TMP_BIN" "$DEST_BIN"

echo "[ov-agy] Installed verified binary at ${DEST_BIN}" >&2
