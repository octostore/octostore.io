#!/bin/sh
set -eu

: "${OCTOSTORE_SMOKE_WORKER_LOG:?set OCTOSTORE_SMOKE_WORKER_LOG}"
: "${OCTOSTORE_FENCING_TERM:?supervisor must provide the fencing term}"

stopped() {
  printf 'stopped %s\n' "$$" >>"$OCTOSTORE_SMOKE_WORKER_LOG"
  exit 0
}

trap stopped INT TERM
printf 'started %s term %s\n' "$$" "$OCTOSTORE_FENCING_TERM" >>"$OCTOSTORE_SMOKE_WORKER_LOG"
if [ -n "${OCTOSTORE_SUPERVISOR_READY_FILE:-}" ]; then
  : >"$OCTOSTORE_SUPERVISOR_READY_FILE"
fi
while :; do
  sleep 1
done
