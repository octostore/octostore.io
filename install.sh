#!/bin/sh
set -e

REPO="${OCTOSTORE_REPO:-octostore/octostore.io}"
BINARY="octostore"
INSTALL_DIR="${OCTOSTORE_INSTALL_DIR:-/usr/local/bin}"
DOWNLOAD_BASE="${OCTOSTORE_DOWNLOAD_BASE:-https://github.com/${REPO}/releases}"
TMP=""
SUMS_TMP=""

cleanup() {
  [ -n "$TMP" ] && rm -f "$TMP"
  [ -n "$SUMS_TMP" ] && rm -f "$SUMS_TMP"
}
trap cleanup EXIT INT TERM

# Detect OS and arch
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)  PLATFORM="linux" ;;
  darwin) PLATFORM="macos" ;;
  *)      echo "❌ Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64)  SUFFIX="${PLATFORM}-amd64" ;;
  arm64|aarch64) SUFFIX="${PLATFORM}-arm64" ;;
  *)             echo "❌ Unsupported architecture: $ARCH"; exit 1 ;;
esac

ASSET="octostore-${SUFFIX}"

# Resolve the requested release, or discover the latest stable release.
if [ -n "${OCTOSTORE_VERSION:-}" ]; then
  LATEST="$OCTOSTORE_VERSION"
  case "$LATEST" in
    v*) ;;
    *) LATEST="v${LATEST}" ;;
  esac
  echo "🐙 Installing requested release ${LATEST}..."
else
  echo "🐙 Detecting latest release..."
  LATEST=$(curl -sI "${DOWNLOAD_BASE}/latest" | grep -i "^location:" | sed 's/.*tag\///' | tr -d '\r\n')
fi

if [ -z "$LATEST" ]; then
  echo "❌ Could not find latest release"
  exit 1
fi

URL="${DOWNLOAD_BASE}/download/${LATEST}/${ASSET}"
echo "📦 Downloading ${ASSET} (${LATEST})..."

TMP=$(mktemp)
SUMS_TMP=$(mktemp)
curl -fL --show-error --silent -o "$TMP" "$URL" || {
  echo "❌ Download failed: $URL"
  exit 1
}
curl -fL --show-error --silent \
  -o "$SUMS_TMP" \
  "${DOWNLOAD_BASE}/download/${LATEST}/SHA256SUMS" || {
  echo "❌ Could not download release checksums"
  exit 1
}

EXPECTED=$(awk -v asset="$ASSET" '$2 == asset { print $1 }' "$SUMS_TMP")
if [ -z "$EXPECTED" ]; then
  echo "❌ SHA256SUMS does not contain $ASSET"
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "$TMP" | awk '{print $1}')
else
  ACTUAL=$(shasum -a 256 "$TMP" | awk '{print $1}')
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "❌ Checksum mismatch for $ASSET"
  exit 1
fi

chmod +x "$TMP"
VERSION=$($TMP --version | awk '{print $2}')
EXPECTED_VERSION=${LATEST#v}
if [ "$VERSION" != "$EXPECTED_VERSION" ]; then
  echo "❌ Binary reports $VERSION, expected $EXPECTED_VERSION"
  exit 1
fi

# Install
if [ -w "$INSTALL_DIR" ]; then
  mv "$TMP" "${INSTALL_DIR}/${BINARY}"
else
  echo "🔑 Installing to ${INSTALL_DIR} (needs sudo)..."
  sudo mv "$TMP" "${INSTALL_DIR}/${BINARY}"
fi

echo "✅ Installed ${BINARY} to ${INSTALL_DIR}/${BINARY}"
echo ""
echo "Usage:"
echo "  STATIC_TOKENS='local:change-me' octostore"
echo "  curl http://localhost:3000/health"
