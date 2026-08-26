#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
BINARY=${1:-"$ROOT/target/debug/octostore"}
SUPERVISOR=${2:-${OCTOSTORE_SUPERVISOR:-$ROOT/scripts/reference-supervisor.sh}}

# A fresh runner can transiently lose the loopback bind race while a preceding
# coordination fixture is releasing its listener. Retry the whole isolated
# proof, never an individual assertion, and preserve the final failure.
if [[ -z ${OCTOSTORE_SMOKE_SUPERVISOR_ATTEMPT:-} ]]; then
  for attempt in 1 2 3; do
    if OCTOSTORE_SMOKE_SUPERVISOR_ATTEMPT=$attempt \
      OCTOSTORE_SMOKE_PORT=$((34000 + (( $$ + attempt * 7919 ) % 10000))) \
      "$0" "$BINARY" "$SUPERVISOR"; then
      exit 0
    fi
    if [[ "$attempt" -eq 3 ]]; then
      echo "supervisor smoke failed after $attempt independent attempts" >&2
      exit 70
    fi
    echo "supervisor smoke attempt $attempt exited before proving containment; retrying" >&2
    sleep 0.1
  done
fi

PORT=${OCTOSTORE_SMOKE_PORT:-$((34000 + ($$ % 10000)))}
AUTHORITY="http://127.0.0.1:$PORT"
SMOKE_DIR=$(mktemp -d "$ROOT/.smoke-supervisor.XXXXXX")
SERVER_PID=""
SUPERVISOR_PID=""

fail_supervisor_start() {
  local supervisor_status=70
  if [[ -n "$SUPERVISOR_PID" ]]; then
    set +e
    wait "$SUPERVISOR_PID"
    supervisor_status=$?
    set -e
    SUPERVISOR_PID=""
  fi
  echo "supervisor exited $supervisor_status before starting worker" >&2
  sed -n '1,120p' "$SMOKE_DIR/supervisor.err" >&2
  sed -n '1,80p' "$SMOKE_DIR/supervisor.out" >&2
  sed -n '1,80p' "$SMOKE_DIR/server.log" >&2
  exit 70
}

stop_process() {
  local pid=${1:-}
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}

cleanup() {
  stop_process "$SUPERVISOR_PID"
  stop_process "$SERVER_PID"
  rm -rf "$SMOKE_DIR"
}
trap cleanup EXIT INT TERM

[[ -x "$BINARY" ]] || { echo "smoke binary is not executable: $BINARY" >&2; exit 64; }
[[ -x "$SUPERVISOR" ]] || { echo "smoke supervisor is not executable: $SUPERVISOR" >&2; exit 64; }
[[ -x "$ROOT/tests/fixtures/recording-worker.sh" ]] || { echo "recording worker is not executable" >&2; exit 64; }
command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 64; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 64; }
command -v perl >/dev/null 2>&1 || { echo "perl with Time::HiRes is required" >&2; exit 64; }
case "$(uname -s)" in
  Darwin) SUPERVISOR_CLOCK=CLOCK_MONOTONIC_RAW ;;
  Linux) SUPERVISOR_CLOCK=CLOCK_BOOTTIME ;;
  *) echo "supervisor smoke requires Linux or macOS" >&2; exit 64 ;;
esac
perl "-MTime::HiRes=clock_gettime,$SUPERVISOR_CLOCK" \
  -e "clock_gettime($SUPERVISOR_CLOCK)" >/dev/null 2>&1 || {
  echo "perl with Time::HiRes $SUPERVISOR_CLOCK support is required" >&2
  exit 64
}

BIND_ADDR="127.0.0.1:$PORT" \
DATABASE_URL="$SMOKE_DIR/octostore.db" \
"$BINARY" serve >"$SMOKE_DIR/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 100); do
  curl --fail --silent --max-time 1 "$AUTHORITY/health" >/dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "OctoStore exited before becoming healthy" >&2; exit 70; }
  sleep 0.05
done
curl --fail --silent --max-time 1 "$AUTHORITY/health" >/dev/null
ROOM=$("$BINARY" election create --server "$AUTHORITY" --json | jq -er '.election_id')

export OCTOSTORE_SMOKE_WORKER_LOG="$SMOKE_DIR/worker.log"
"$SUPERVISOR" election "$ROOM" supervised-agent \
  "$ROOT/tests/fixtures/recording-worker.sh" -- \
  "$BINARY" election hold "$ROOM" --candidate supervised-agent \
  --ttl 5 --request-timeout 1s --server "$AUTHORITY" --json \
  >"$SMOKE_DIR/supervisor.out" 2>"$SMOKE_DIR/supervisor.err" &
SUPERVISOR_PID=$!

for _ in $(seq 1 100); do
  grep -q '^started ' "$SMOKE_DIR/worker.log" 2>/dev/null && break
  kill -0 "$SUPERVISOR_PID" 2>/dev/null || fail_supervisor_start
  sleep 0.05
done
grep -Eq '^started [0-9]+ term [1-9][0-9]*$' "$SMOKE_DIR/worker.log"

kill -KILL "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""

set +e
for _ in $(seq 1 200); do
  kill -0 "$SUPERVISOR_PID" 2>/dev/null || break
  sleep 0.05
done
wait "$SUPERVISOR_PID"
SUPERVISOR_STATUS=$?
set -e
SUPERVISOR_PID=""
[[ "$SUPERVISOR_STATUS" -eq 20 ]] || {
  echo "supervisor exited $SUPERVISOR_STATUS, expected 20 after uncertainty" >&2
  sed -n '1,160p' "$SMOKE_DIR/supervisor.err" >&2
  exit 70
}
grep -q '^stopped ' "$SMOKE_DIR/worker.log"

if grep -E 'leader_token|lease_id' "$SMOKE_DIR/supervisor.out" "$SMOKE_DIR/supervisor.err" >/dev/null; then
  echo "secret-shaped value appeared in supervisor output" >&2
  exit 70
fi

echo "supervisor smoke passed: worker gated on authority and stopped on uncertainty"
