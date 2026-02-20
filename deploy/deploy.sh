#!/usr/bin/env bash
# deploy/deploy.sh — called by GitHub Actions on demo-host for each tagged release.
# Usage: ./deploy/deploy.sh <tag>  (e.g. v0.4.2)
set -euo pipefail

TAG="${1:?Usage: $0 <tag>}"
EXPECTED_VERSION="${TAG#v}"
BINARY=/opt/octostore-lock/octostore
REPO=octostore/octostore.io
SERVICE=octostore

echo "=== Deploy $TAG ==="

# 1. Pull latest site/config files
cd /opt/octostore-lock
git fetch origin main
git checkout main
git reset --hard origin/main

# 2. Download binary from GitHub release
curl -fsSL \
  "https://github.com/$REPO/releases/download/$TAG/octostore-linux-amd64" \
  -o /tmp/octostore-new
chmod +x /tmp/octostore-new

# 3. Verify binary reports the correct version
BINARY_VERSION=$(/tmp/octostore-new --version | awk '{print $2}')
if [[ "$BINARY_VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "ERROR: binary reports '$BINARY_VERSION', expected '$EXPECTED_VERSION'" >&2
  rm /tmp/octostore-new
  exit 1
fi
echo "Binary version verified: $BINARY_VERSION"

# 4. Stop → kill any zombie on port 3030 → swap → start
sudo systemctl stop "$SERVICE"
sleep 1
sudo fuser -k 3030/tcp 2>/dev/null || true
sleep 1
rm -f "$BINARY"
mv /tmp/octostore-new "$BINARY"
sudo systemctl start "$SERVICE"

# 5. Wait for active (not just activating)
for i in $(seq 1 10); do
  STATE=$(sudo systemctl is-active "$SERVICE" 2>/dev/null || true)
  if [[ "$STATE" == "active" ]]; then break; fi
  if [[ "$STATE" == "failed" ]]; then
    echo "ERROR: service failed to start" >&2
    sudo journalctl -u "$SERVICE" -n 20 --no-pager >&2
    exit 1
  fi
  echo "  waiting... ($STATE)"
  sleep 2
done

# 6. Verify live API version
sleep 1
LIVE_VERSION=$(curl -sf https://api.octostore.io/openapi.yaml | grep '  version:' | awk '{print $2}')
if [[ "$LIVE_VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "ERROR: live API reports '$LIVE_VERSION', expected '$EXPECTED_VERSION'" >&2
  exit 1
fi

echo "=== Deploy complete: $TAG (live: $LIVE_VERSION) ==="
