#!/usr/bin/env bash
# deploy/deploy.sh — called by GitHub Actions on demo-host for each tagged release.
# Usage: ./deploy/deploy.sh <tag>  (e.g. v0.4.2)
set -euo pipefail

TAG="${1:?Usage: $0 <tag>}"
EXPECTED_VERSION="${TAG#v}"
BINARY=/opt/octostore-lock/octostore
REPO=octostore/octostore.io
SERVICE=octostore
ASSET=octostore-linux-amd64
TMP_BINARY=$(mktemp /tmp/octostore-new.XXXXXX)
TMP_SUMS=$(mktemp /tmp/octostore-sums.XXXXXX)
PREVIOUS_COMMIT=""
BACKUP_BINARY="${BINARY}.previous"

cleanup() {
  rm -f "$TMP_BINARY" "$TMP_SUMS"
}
trap cleanup EXIT

rollback() {
  echo "ERROR: deployment failed; restoring the previous binary and site" >&2
  if [[ -f "$BACKUP_BINARY" ]]; then
    sudo systemctl stop "$SERVICE" || true
    mv "$BACKUP_BINARY" "$BINARY"
    sudo systemctl start "$SERVICE" || true
  fi
  if [[ -n "$PREVIOUS_COMMIT" ]]; then
    git checkout --detach --force "$PREVIOUS_COMMIT" || true
  fi
}

echo "=== Deploy $TAG ==="

# 1. Check out the exact tagged site/config files
cd /opt/octostore-lock
PREVIOUS_COMMIT=$(git rev-parse HEAD)
git fetch --force origin "refs/tags/$TAG:refs/tags/$TAG"
git checkout --detach --force "$TAG"

# 2. Download and verify the published binary
if ! curl -fsSL "https://github.com/$REPO/releases/download/$TAG/$ASSET" -o "$TMP_BINARY"; then
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi
if ! curl -fsSL "https://github.com/$REPO/releases/download/$TAG/SHA256SUMS" -o "$TMP_SUMS"; then
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi
EXPECTED_SHA=$(awk -v asset="$ASSET" '$2 == asset { print $1 }' "$TMP_SUMS")
ACTUAL_SHA=$(sha256sum "$TMP_BINARY" | awk '{print $1}')
if [[ -z "$EXPECTED_SHA" || "$EXPECTED_SHA" != "$ACTUAL_SHA" ]]; then
  echo "ERROR: checksum verification failed for $ASSET" >&2
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi
if ! chmod +x "$TMP_BINARY"; then
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi

# 3. Verify binary reports the correct version
BINARY_VERSION=$($TMP_BINARY --version | awk '{print $2}')
if [[ "$BINARY_VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "ERROR: binary reports '$BINARY_VERSION', expected '$EXPECTED_VERSION'" >&2
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi
echo "Binary version verified: $BINARY_VERSION"

# 4. Back up → stop → kill any zombie on port 3030 → swap → start
if ! cp "$BINARY" "$BACKUP_BINARY"; then
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi
if ! sudo systemctl stop "$SERVICE"; then
  rm -f "$BACKUP_BINARY"
  git checkout --detach --force "$PREVIOUS_COMMIT"
  exit 1
fi
sleep 1
sudo fuser -k 3030/tcp 2>/dev/null || true
sleep 1
if ! mv "$TMP_BINARY" "$BINARY"; then
  rollback
  exit 1
fi
if ! sudo systemctl start "$SERVICE"; then
  rollback
  exit 1
fi

# 5. Wait for active (not just activating)
for i in $(seq 1 10); do
  STATE=$(sudo systemctl is-active "$SERVICE" 2>/dev/null || true)
  if [[ "$STATE" == "active" ]]; then break; fi
  if [[ "$STATE" == "failed" ]]; then
    echo "ERROR: service failed to start" >&2
    sudo journalctl -u "$SERVICE" -n 20 --no-pager >&2
    rollback
    exit 1
  fi
  echo "  waiting... ($STATE)"
  sleep 2
done

# 6. Verify live API version (retry up to 30s — service needs time to open port)
echo "Waiting for API to become ready..."
LIVE_VERSION=""
for i in $(seq 1 15); do
  LIVE_VERSION=$(curl -sf --max-time 3 https://api.octostore.io/openapi.yaml 2>/dev/null \
    | grep '  version:' | awk '{print $2}') || true
  if [[ "$LIVE_VERSION" == "$EXPECTED_VERSION" ]]; then break; fi
  echo "  API not ready yet (got: '${LIVE_VERSION:-<empty>}'), retry $i/15..."
  sleep 2
done

if [[ "$LIVE_VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "ERROR: live API reports '$LIVE_VERSION', expected '$EXPECTED_VERSION'" >&2
  sudo journalctl -u "$SERVICE" -n 10 --no-pager >&2
  rollback
  exit 1
fi

rm -f "$BACKUP_BINARY"
echo "=== Deploy complete: $TAG (live: $LIVE_VERSION) ==="
