#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
OCTOSTORE_BIN=${OCTOSTORE_BIN:-octostore}
OCTOSTORE_SUPERVISOR=${OCTOSTORE_SUPERVISOR:-$ROOT/scripts/reference-supervisor.sh}
OCTOSTORE_ATLAS_WORKER=${OCTOSTORE_ATLAS_WORKER:-./agent-atlas-worker}
OCTOSTORE_COMET_WORKER=${OCTOSTORE_COMET_WORKER:-./agent-comet-worker}
DEMO_DIR=${OCTOSTORE_DEMO_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/octostore-two-agents.XXXXXX")}
DEMO_OWNS_DIR=0
if [[ -z "${OCTOSTORE_DEMO_DIR:-}" ]]; then
  DEMO_OWNS_DIR=1
fi
ATLAS_PID=""
COMET_PID=""
ATLAS_PROCESS_PID=""
COMET_PROCESS_PID=""

stop_supervisor() {
  local pid=${1:-}
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  stop_supervisor "$COMET_PID"
  stop_supervisor "$ATLAS_PID"
  if [[ "$DEMO_OWNS_DIR" -eq 1 && "${OCTOSTORE_DEMO_KEEP:-0}" != 1 ]]; then
    rm -rf "$DEMO_DIR"
  fi
}
trap cleanup EXIT INT TERM

command -v jq >/dev/null 2>&1 || { echo "two-agent demo requires jq" >&2; exit 64; }
[[ -x "$OCTOSTORE_SUPERVISOR" ]] || { echo "reference supervisor is not executable" >&2; exit 64; }
[[ -x "$OCTOSTORE_ATLAS_WORKER" ]] || { echo "Atlas worker is not executable" >&2; exit 64; }
[[ -x "$OCTOSTORE_COMET_WORKER" ]] || { echo "Comet worker is not executable" >&2; exit 64; }
mkdir -p "$DEMO_DIR"

# Create one room outside both candidates, then share exactly that ID.
OCTOSTORE_ELECTION=$(
  "$OCTOSTORE_BIN" election create --json | jq -er '.election_id'
)
export OCTOSTORE_ELECTION
echo "shared room: $OCTOSTORE_ELECTION"

# Launch both supervised candidates concurrently. Bare 60 means 60 seconds.
"$OCTOSTORE_SUPERVISOR" election "$OCTOSTORE_ELECTION" agent-atlas \
  "$OCTOSTORE_ATLAS_WORKER" -- \
  "$OCTOSTORE_BIN" election hold "$OCTOSTORE_ELECTION" \
    --candidate agent-atlas --ttl 5 --acquire-timeout 60 --json \
  >"$DEMO_DIR/agent-atlas.jsonl" 2>"$DEMO_DIR/agent-atlas.err" &
ATLAS_PID=$!
ATLAS_PROCESS_PID=$ATLAS_PID

"$OCTOSTORE_SUPERVISOR" election "$OCTOSTORE_ELECTION" agent-comet \
  "$OCTOSTORE_COMET_WORKER" -- \
  "$OCTOSTORE_BIN" election hold "$OCTOSTORE_ELECTION" \
    --candidate agent-comet --ttl 5 --acquire-timeout 60 --json \
  >"$DEMO_DIR/agent-comet.jsonl" 2>"$DEMO_DIR/agent-comet.err" &
COMET_PID=$!
COMET_PROCESS_PID=$COMET_PID

for _ in $(seq 1 600); do
  atlas_leads=$(grep -c '"event":"leader"' "$DEMO_DIR/agent-atlas.jsonl" 2>/dev/null || true)
  comet_leads=$(grep -c '"event":"leader"' "$DEMO_DIR/agent-comet.jsonl" 2>/dev/null || true)
  atlas_waits=$(grep -c '"event":"waiting"' "$DEMO_DIR/agent-atlas.jsonl" 2>/dev/null || true)
  comet_waits=$(grep -c '"event":"waiting"' "$DEMO_DIR/agent-comet.jsonl" 2>/dev/null || true)
  if [[ "$atlas_leads" -gt 0 && "$comet_waits" -gt 0 ]]; then
    LEADER_PID=$ATLAS_PID
    FOLLOWER_PID=$COMET_PID
    LEADER_NAME=agent-atlas
    FOLLOWER_NAME=agent-comet
    FOLLOWER_LOG=$DEMO_DIR/agent-comet.jsonl
    break
  fi
  if [[ "$comet_leads" -gt 0 && "$atlas_waits" -gt 0 ]]; then
    LEADER_PID=$COMET_PID
    FOLLOWER_PID=$ATLAS_PID
    LEADER_NAME=agent-comet
    FOLLOWER_NAME=agent-atlas
    FOLLOWER_LOG=$DEMO_DIR/agent-atlas.jsonl
    break
  fi
  kill -0 "$ATLAS_PID" 2>/dev/null || { echo "Atlas supervisor exited early" >&2; exit 70; }
  kill -0 "$COMET_PID" 2>/dev/null || { echo "Comet supervisor exited early" >&2; exit 70; }
  sleep 0.05
done

[[ -n "${LEADER_PID:-}" ]] || { echo "demo did not observe one leader and one waiter" >&2; exit 70; }
echo "$LEADER_NAME leads; $FOLLOWER_NAME waits"
sleep "${OCTOSTORE_DEMO_SETTLE_SECONDS:-0.25}"

# Cancel the current leader. Its hold releases, and the waiting supervisor
# promotes the other worker without front-loading a central workflow graph.
kill -TERM "$LEADER_PID"
set +e
wait "$LEADER_PID"
LEADER_STATUS=$?
set -e
[[ "$LEADER_STATUS" -eq 143 ]] || { echo "$LEADER_NAME exited $LEADER_STATUS" >&2; exit 70; }
if [[ "$LEADER_PID" = "$ATLAS_PID" ]]; then ATLAS_PID=""; else COMET_PID=""; fi

for _ in $(seq 1 600); do
  grep -q '"event":"leader"' "$FOLLOWER_LOG" 2>/dev/null && break
  kill -0 "$FOLLOWER_PID" 2>/dev/null || { echo "$FOLLOWER_NAME exited before takeover" >&2; exit 70; }
  sleep 0.05
done
grep -q '"event":"leader"' "$FOLLOWER_LOG" || { echo "$FOLLOWER_NAME never took over" >&2; exit 70; }
echo "$FOLLOWER_NAME took over after cancellation"
sleep "${OCTOSTORE_DEMO_SETTLE_SECONDS:-0.25}"

kill -TERM "$FOLLOWER_PID"
set +e
wait "$FOLLOWER_PID"
FOLLOWER_STATUS=$?
set -e
[[ "$FOLLOWER_STATUS" -eq 143 ]] || { echo "$FOLLOWER_NAME exited $FOLLOWER_STATUS" >&2; exit 70; }
ATLAS_PID=""
COMET_PID=""

for supervisor_pid in "$ATLAS_PROCESS_PID" "$COMET_PROCESS_PID"; do
  if kill -0 "$supervisor_pid" 2>/dev/null; then
    echo "supervisor process $supervisor_pid remained after cancellation" >&2
    exit 70
  fi
done
echo "no remaining supervisor processes"

if grep -E 'leader_token|lease_id|Authorization: Bearer' \
  "$DEMO_DIR"/*.jsonl "$DEMO_DIR"/*.err >/dev/null; then
  echo "secret-shaped value appeared in supervised demo output" >&2
  exit 70
fi

echo "two-agent supervised demo passed: waiting, takeover, and cancellation"
