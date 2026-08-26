#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
WORK_PARENT=$(dirname -- "$ROOT")
COMPAT_ROOT=$(mktemp -d "$WORK_PARENT/.octostore-downgrade.XXXXXX")
OLD_SOURCE="$COMPAT_ROOT/v0.13.2"
OLD_TARGET="$COMPAT_ROOT/target-v0.13.2"
CURRENT_TARGET="$COMPAT_ROOT/target-v0.14"
OLD_COMMIT=90eb7ebfdbdc2004c8340c1bdabe0a786d858959
DATABASE="$COMPAT_ROOT/compatibility.db"
CURRENT_BINARY="$CURRENT_TARGET/debug/octostore"
OLD_BINARY="$OLD_TARGET/debug/octostore"
LOCK_NAME=compat.downgrade
STATIC_IDENTITIES='compat:compat-token,observer:observer-token'
ADMIN_KEY=compat-admin-key
NIL_UUID=00000000-0000-0000-0000-000000000000
SENTINEL_HOLDER=ffffffff-ffff-ffff-ffff-ffffffffffff
BASE_PORT=$((42000 + ($$ % 10000)))
CURRENT_PORT=$BASE_PORT
OLD_PORT=$((BASE_PORT + 1))
FORWARD_PORT=$((BASE_PORT + 2))
SERVER_PID=

cleanup_server() {
  if [[ -n $SERVER_PID ]]; then
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=
  fi
}

cleanup() {
  cleanup_server
  if [[ -d $OLD_SOURCE ]]; then
    git -C "$ROOT" worktree remove --force "$OLD_SOURCE" >/dev/null 2>&1 || true
  fi
  chmod -R u+w "$COMPAT_ROOT" 2>/dev/null || true
  rm -rf "$COMPAT_ROOT"
}
trap cleanup EXIT INT TERM HUP

fail() {
  echo "downgrade compatibility smoke failed: $*" >&2
  exit 1
}

for utility in cargo curl git jq; do
  command -v "$utility" >/dev/null || fail "required command is unavailable: $utility"
done

wait_for_health() {
  local port=$1
  local attempt
  for ((attempt = 1; attempt <= 80; attempt += 1)); do
    if curl --noproxy '*' --fail --silent --show-error --max-time 1 \
      "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
      return
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      return 1
    fi
    sleep 0.1
  done
  return 1
}

start_current() {
  local port=$1
  local log=$2
  BIND_ADDR="127.0.0.1:$port" \
  DATABASE_URL="$DATABASE" \
  STATIC_TOKENS="$STATIC_IDENTITIES" \
  ADMIN_KEY="$ADMIN_KEY" \
  PUBLIC_ELECTIONS=false \
    "$CURRENT_BINARY" serve >"$log" 2>&1 &
  SERVER_PID=$!
  wait_for_health "$port" || fail "v0.14 server did not become healthy; see $log"
}

start_old() {
  local port=$1
  local log=$2
  BIND_ADDR="127.0.0.1:$port" \
  DATABASE_URL="$DATABASE" \
  STATIC_TOKENS="$STATIC_IDENTITIES" \
  ADMIN_KEY="$ADMIN_KEY" \
  PUBLIC_ELECTIONS=false \
    "$OLD_BINARY" >"$log" 2>&1 &
  SERVER_PID=$!
  wait_for_health "$port" || fail "v0.13.2 server did not become healthy; see $log"
}

post_acquire() {
  local port=$1
  local output=$2
  local token=$3
  curl --noproxy '*' --silent --show-error --max-time 3 \
    --output "$output" --write-out '%{http_code}' \
    --request POST "http://127.0.0.1:$port/locks/$LOCK_NAME/acquire" \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data '{"ttl_seconds":300}'
}

post_renew() {
  local port=$1
  local output=$2
  local token=$3
  local lease_id=$4
  curl --noproxy '*' --silent --show-error --max-time 3 \
    --output "$output" --write-out '%{http_code}' \
    --request POST "http://127.0.0.1:$port/locks/$LOCK_NAME/renew" \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data "{\"lease_id\":\"$lease_id\",\"ttl_seconds\":300}"
}

post_release() {
  local port=$1
  local output=$2
  local token=$3
  local lease_id=$4
  curl --noproxy '*' --silent --show-error --max-time 3 \
    --output "$output" --write-out '%{http_code}' \
    --request POST "http://127.0.0.1:$port/locks/$LOCK_NAME/release" \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data "{\"lease_id\":\"$lease_id\"}"
}

[[ $(git -C "$ROOT" rev-parse 'v0.13.2^{commit}') == "$OLD_COMMIT" ]] \
  || fail "v0.13.2 does not resolve to the reviewed rollback commit"
git -C "$ROOT" worktree add --detach "$OLD_SOURCE" "$OLD_COMMIT" >/dev/null
CARGO_TARGET_DIR="$OLD_TARGET" \
  cargo build --quiet --locked --manifest-path "$OLD_SOURCE/Cargo.toml" --bin octostore
CARGO_TARGET_DIR="$CURRENT_TARGET" \
  cargo build --quiet --locked --manifest-path "$ROOT/Cargo.toml" --bin octostore
[[ $($OLD_BINARY --version) == 'octostore 0.13.2' ]] || fail "wrong rollback binary"
[[ $($CURRENT_BINARY --version) == 'octostore 0.14.4' ]] || fail "wrong candidate binary"

start_current "$CURRENT_PORT" "$COMPAT_ROOT/current-create.log"
CREATE_CODE=$(curl --noproxy '*' --silent --show-error --max-time 3 \
  --output "$COMPAT_ROOT/current-create.json" --write-out '%{http_code}' \
  --request POST "http://127.0.0.1:$CURRENT_PORT/locks/$LOCK_NAME/acquire" \
  --header 'Authorization: Bearer compat-token' \
  --header 'Content-Type: application/json' \
  --data '{"ttl_seconds":300,"lock_delay_seconds":10}')
[[ $CREATE_CODE == 200 ]] || fail "v0.14 could not create the seed lease"
[[ $(jq -r '.status' "$COMPAT_ROOT/current-create.json") == acquired ]] \
  || fail "v0.14 seed response was not acquired"
SEED_LEASE=$(jq -r '.lease_id' "$COMPAT_ROOT/current-create.json")
RELEASE_CODE=$(curl --noproxy '*' --silent --show-error --max-time 3 \
  --output "$COMPAT_ROOT/current-release.json" --write-out '%{http_code}' \
  --request POST "http://127.0.0.1:$CURRENT_PORT/locks/$LOCK_NAME/release" \
  --header 'Authorization: Bearer compat-token' \
  --header 'Content-Type: application/json' \
  --data "{\"lease_id\":\"$SEED_LEASE\"}")
[[ $RELEASE_CODE == 200 ]] || fail "v0.14 could not create durable cooling"
COOLING_STARTED=$SECONDS
DELAY_CODE=$(post_acquire "$CURRENT_PORT" "$COMPAT_ROOT/current-delayed.json" observer-token)
[[ $DELAY_CODE == 409 ]] \
  || fail "v0.14 did not enforce cooling immediately after release: HTTP $DELAY_CODE $(<"$COMPAT_ROOT/current-delayed.json")"
[[ $(jq -r '.status' "$COMPAT_ROOT/current-delayed.json") == delayed ]] \
  || fail "v0.14 cooling response was not delayed"
AVAILABLE_AT=$(jq -r '.available_at' "$COMPAT_ROOT/current-delayed.json")
cleanup_server

start_old "$OLD_PORT" "$COMPAT_ROOT/v0132.log"
((SECONDS - COOLING_STARTED < 10)) \
  || fail "admin rollback checks did not begin before the cooling deadline"
ADMIN_ACQUIRE_CODE=$(post_acquire \
  "$OLD_PORT" "$COMPAT_ROOT/v0132-admin-held.json" "$ADMIN_KEY")
[[ $ADMIN_ACQUIRE_CODE == 200 ]] \
  || fail "v0.13.2 admin acquire check returned HTTP $ADMIN_ACQUIRE_CODE"
[[ $(jq -r '.status' "$COMPAT_ROOT/v0132-admin-held.json") == held ]] \
  || fail "v0.13.2 admin acquired the compatibility sentinel: $(<"$COMPAT_ROOT/v0132-admin-held.json")"
[[ $(jq -r '.holder_id' "$COMPAT_ROOT/v0132-admin-held.json") == "$SENTINEL_HOLDER" ]] \
  || fail "v0.13.2 did not load the impossible compatibility holder"
[[ $(jq -r '.expires_at' "$COMPAT_ROOT/v0132-admin-held.json") == "$AVAILABLE_AT" ]] \
  || fail "v0.13.2 admin acquire changed the cooling deadline"

ADMIN_RENEW_CODE=$(post_renew \
  "$OLD_PORT" "$COMPAT_ROOT/v0132-admin-renew.json" "$ADMIN_KEY" "$NIL_UUID")
[[ $ADMIN_RENEW_CODE != 200 ]] \
  || fail "v0.13.2 admin renewed the compatibility sentinel with the nil lease"
ADMIN_RELEASE_CODE=$(post_release \
  "$OLD_PORT" "$COMPAT_ROOT/v0132-admin-release.json" "$ADMIN_KEY" "$NIL_UUID")
[[ $ADMIN_RELEASE_CODE != 200 ]] \
  || fail "v0.13.2 admin deleted the compatibility sentinel with the nil lease"
((SECONDS - COOLING_STARTED < 10)) \
  || fail "admin rollback mutation checks crossed the cooling deadline"

HELD_CODE=$(post_acquire "$OLD_PORT" "$COMPAT_ROOT/v0132-held.json" observer-token)
[[ $HELD_CODE == 200 ]] || fail "v0.13.2 held check returned HTTP $HELD_CODE"
[[ $(jq -r '.status' "$COMPAT_ROOT/v0132-held.json") == held ]] \
  || fail "v0.13.2 admitted a successor before cooling ended: $(<"$COMPAT_ROOT/v0132-held.json"); log: $(tail -n 12 "$COMPAT_ROOT/v0132.log")"
[[ $(jq -r '.holder_id' "$COMPAT_ROOT/v0132-held.json") == "$SENTINEL_HOLDER" ]] \
  || fail "v0.13.2 did not load the compatibility sentinel"
[[ $(jq -r '.expires_at' "$COMPAT_ROOT/v0132-held.json") == "$AVAILABLE_AT" ]] \
  || fail "v0.13.2 changed the candidate's cooling deadline"

DEADLINE=$((COOLING_STARTED + 16))
while :; do
  OLD_CODE=$(post_acquire "$OLD_PORT" "$COMPAT_ROOT/v0132-acquire.json" compat-token)
  if [[ $OLD_CODE == 200 ]] \
    && [[ $(jq -r '.status' "$COMPAT_ROOT/v0132-acquire.json") == acquired ]]; then
    break
  fi
  ((SECONDS < DEADLINE)) \
    || fail "v0.13.2 could not acquire within one cooldown; a second delay may have been applied"
  sleep 0.2
done
OLD_LEASE=$(jq -r '.lease_id' "$COMPAT_ROOT/v0132-acquire.json")
[[ -n $OLD_LEASE && $OLD_LEASE != null ]] || fail "v0.13.2 omitted its successor lease"
cleanup_server

start_current "$FORWARD_PORT" "$COMPAT_ROOT/current-forward.log"
FORWARD_CODE=$(post_acquire "$FORWARD_PORT" "$COMPAT_ROOT/current-forward.json" compat-token)
[[ $FORWARD_CODE == 200 ]] || fail "v0.14 forward restart returned HTTP $FORWARD_CODE"
[[ $(jq -r '.status' "$COMPAT_ROOT/current-forward.json") == acquired ]] \
  || fail "v0.14 did not load the v0.13.2 successor lease"
[[ $(jq -r '.lease_id' "$COMPAT_ROOT/current-forward.json") == "$OLD_LEASE" ]] \
  || fail "v0.14 replaced the v0.13.2 successor lease during forward restart"
cleanup_server

echo "downgrade compatibility smoke passed: v0.14 impossible sentinel -> v0.13.2 admin rejection/hold/acquire -> v0.14 successor recovery"
