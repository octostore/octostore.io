#!/bin/sh
set -e

REPO="octostore/octostore.io"
BINARY="octostore-test"
INSTALL_DIR="/usr/local/bin"

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

ASSET="${BINARY}-${SUFFIX}"

# Get latest release URL
echo "🐙 Detecting latest release..."
LATEST=$(curl -sI "https://github.com/${REPO}/releases/latest" | grep -i "^location:" | sed 's/.*tag\///' | tr -d '\r\n')

if [ -z "$LATEST" ]; then
  echo "❌ Could not find latest release"
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST}/${ASSET}"
echo "📦 Downloading ${ASSET} (${LATEST})..."

TMP=$(mktemp)
HTTP_CODE=$(curl -sL -w "%{http_code}" -o "$TMP" "$URL")

if [ "$HTTP_CODE" != "200" ]; then
  rm -f "$TMP"
  echo "❌ Download failed (HTTP ${HTTP_CODE})"
  echo "   URL: ${URL}"
  echo "   Available at: https://github.com/${REPO}/releases/tag/${LATEST}"
  exit 1
fi

chmod +x "$TMP"

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
echo "  octostore-test --token YOUR_TOKEN"
echo "  octostore-test --url https://your-instance.com --token YOUR_TOKEN"
