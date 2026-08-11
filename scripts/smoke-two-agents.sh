#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
BINARY=${1:-"$ROOT/target/debug/octostore"}
SUPERVISOR=${OCTOSTORE_SUPERVISOR:-$ROOT/scripts/reference-supervisor.sh}
PORT=${OCTOSTORE_SMOKE_PORT:-$((24000 + ($$ % 10000)))}
AUTHORITY="http://127.0.0.1:$PORT"
SMOKE_DIR=$(mktemp -d "$ROOT/.smoke-two-agents.XXXXXX")
SERVER_PID=""

stop_process() {
  local pid=${1:-}
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}

cleanup() {
  stop_process "$SERVER_PID"
  if [[ "${OCTOSTORE_SMOKE_KEEP:-0}" = 1 ]]; then
    echo "retained two-agent smoke fixture: $SMOKE_DIR" >&2
  else
    rm -rf "$SMOKE_DIR"
  fi
}
trap cleanup EXIT INT TERM

[[ -x "$BINARY" ]] || {
  echo "smoke binary is not executable: $BINARY" >&2
  exit 64
}
[[ -x "$SUPERVISOR" ]] || {
  echo "smoke supervisor is not executable: $SUPERVISOR" >&2
  exit 64
}
command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 64; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 64; }
if curl --fail --silent --max-time 1 "$AUTHORITY/health" >/dev/null 2>&1; then
  echo "smoke port is already serving HTTP: $PORT" >&2
  exit 64
fi

BIND_ADDR="127.0.0.1:$PORT" \
DATABASE_URL="$SMOKE_DIR/octostore.db" \
STATIC_TOKENS='smoke-user:smoke-token' \
"$BINARY" serve >"$SMOKE_DIR/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 100); do
  if curl --fail --silent --max-time 1 "$AUTHORITY/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "OctoStore exited before becoming healthy" >&2
    sed -n '1,120p' "$SMOKE_DIR/server.log" >&2
    exit 70
  fi
  sleep 0.05
done
curl --fail --silent --max-time 1 "$AUTHORITY/health" >/dev/null

ATLAS_WORKER=$SMOKE_DIR/agent-atlas-worker
COMET_WORKER=$SMOKE_DIR/agent-comet-worker
printf '#!/bin/sh\nOCTOSTORE_SMOKE_WORKER_LOG=%s; export OCTOSTORE_SMOKE_WORKER_LOG\nexec %s\n' \
  "$SMOKE_DIR/agent-atlas-worker.log" "$ROOT/tests/fixtures/recording-worker.sh" >"$ATLAS_WORKER"
printf '#!/bin/sh\nOCTOSTORE_SMOKE_WORKER_LOG=%s; export OCTOSTORE_SMOKE_WORKER_LOG\nexec %s\n' \
  "$SMOKE_DIR/agent-comet-worker.log" "$ROOT/tests/fixtures/recording-worker.sh" >"$COMET_WORKER"
chmod 700 "$ATLAS_WORKER" "$COMET_WORKER"

for attempt in 1 2 3; do
  DEMO_DIR="$SMOKE_DIR/demo-$attempt"
  if OCTOSTORE_URL=$AUTHORITY \
    OCTOSTORE_BIN=$BINARY \
    OCTOSTORE_SUPERVISOR=$SUPERVISOR \
    OCTOSTORE_ATLAS_WORKER=$ATLAS_WORKER \
    OCTOSTORE_COMET_WORKER=$COMET_WORKER \
    OCTOSTORE_DEMO_DIR=$DEMO_DIR \
    OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY=1 \
      "$ROOT/scripts/two-agent-supervised-demo.sh"; then
    break
  fi
  if [[ "$attempt" -eq 3 ]]; then
    echo "two-agent smoke failed after $attempt independent attempts" >&2
    exit 70
  fi
  echo "two-agent smoke attempt $attempt exited before proving coordination; retrying" >&2
  sleep 0.1
done

for worker_log in "$SMOKE_DIR/agent-atlas-worker.log" "$SMOKE_DIR/agent-comet-worker.log"; do
  grep -Eq '^started [0-9]+ term [1-9][0-9]*$' "$worker_log"
  grep -q '^stopped ' "$worker_log"
done

if grep -R -E 'leader_token|lease_id|smoke-token|Authorization: Bearer' "$SMOKE_DIR"/demo-* >/dev/null; then
  echo "secret-shaped value appeared in two-agent demo output" >&2
  exit 70
fi

echo "two-agent smoke passed: public supervised demo ran verbatim"
