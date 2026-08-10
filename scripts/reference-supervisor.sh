#!/bin/sh
set -eu
[ "${OCTOSTORE_SUPERVISOR_DEBUG:-}" != 1 ] || set -x

# This supervisor deliberately uses three independent sessions:
#
#   caller/supervisor  -> guardian monitor -> detached lifetime guardian
#                         -> detached hold wrapper -> hold command
#   caller/supervisor  -> detached worker wrapper -> protected worker
#
# Each detached hold/worker group contains a separate TERM-immune anchor. The
# anchor remains in the group from publication through the one final KILL, so
# its verified PGID cannot become reusable between an identity check and a
# group signal.

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
SCRIPT_PATH=$SCRIPT_DIR/$(basename -- "$0")

WATCHDOG_DEADLINE_RESERVE_MS=250
MIN_WORK_WINDOW_MS=100
MAX_AUTHORITY_BUDGET_MS=300000

sleep_ms() {
  sleep_delay_ms=$1
  sleep_delay_seconds=$((sleep_delay_ms / 1000))
  sleep_delay_milliseconds=$((sleep_delay_ms % 1000))
  sleep "$(printf '%s.%03d' "$sleep_delay_seconds" "$sleep_delay_milliseconds")"
}

continuous_ms() {
  case "$(uname -s)" in
    Darwin)
      perl -MTime::HiRes=clock_gettime,CLOCK_MONOTONIC_RAW \
        -e 'printf "%.0f\n", clock_gettime(CLOCK_MONOTONIC_RAW) * 1000'
      ;;
    Linux)
      perl -MTime::HiRes=clock_gettime,CLOCK_BOOTTIME \
        -e 'printf "%.0f\n", clock_gettime(CLOCK_BOOTTIME) * 1000'
      ;;
    *) return 1 ;;
  esac
}

check_dependencies() {
  command -v jq >/dev/null 2>&1 || {
    echo "reference supervisor requires jq" >&2
    return 1
  }
  command -v perl >/dev/null 2>&1 || {
    echo "reference supervisor requires Perl with Time::HiRes" >&2
    return 1
  }
  continuous_ms >/dev/null 2>&1 || {
    echo "reference supervisor requires CLOCK_BOOTTIME on Linux or CLOCK_MONOTONIC_RAW on macOS" >&2
    return 1
  }
}

atomic_write_line() {
  atomic_target=$1
  atomic_value=$2
  atomic_tmp=$atomic_target.tmp.$$
  printf '%s\n' "$atomic_value" >"$atomic_tmp"
  mv "$atomic_tmp" "$atomic_target"
}

atomic_touch() {
  atomic_touch_target=$1
  atomic_touch_tmp=$atomic_touch_target.tmp.$$
  : >"$atomic_touch_tmp"
  mv "$atomic_touch_tmp" "$atomic_touch_target"
}

process_identity() {
  identity_pid=$1
  ps -o pid= -o pgid= -o lstart= -p "$identity_pid" 2>/dev/null |
    awk 'NF { $1=$1; print; exit }'
}

publish_group_identity() {
  publish_identity_file=$1
  publish_identity=$(process_identity "$$") || return 1
  publish_identity_pid=$(identity_value_pid "$publish_identity") || return 1
  publish_identity_pgid=$(identity_value_pgid "$publish_identity") || return 1
  [ "$publish_identity_pid" = "$$" ] && [ "$publish_identity_pgid" = "$$" ] || return 1
  atomic_write_line "$publish_identity_file" "$publish_identity"
}

publish_group_anchor() {
  publish_anchor_file=$1
  publish_anchor_identity=$(process_identity "$$") || return 1
  publish_anchor_pid=$(identity_value_pid "$publish_anchor_identity") || return 1
  identity_value_pgid "$publish_anchor_identity" >/dev/null || return 1
  [ "$publish_anchor_pid" = "$$" ] || return 1
  atomic_write_line "$publish_anchor_file" "$publish_anchor_identity"
}

identity_pid() {
  identity_pid_file=$1
  IFS= read -r identity_line <"$identity_pid_file" || return 1
  identity_value_pid "$identity_line"
}

identity_pgid() {
  identity_pgid_file=$1
  IFS= read -r identity_pgid_line <"$identity_pgid_file" || return 1
  identity_value_pgid "$identity_pgid_line"
}

identity_matches() {
  identity_matches_file=$1
  [ -r "$identity_matches_file" ] || return 1
  IFS= read -r identity_expected <"$identity_matches_file" || return 1
  identity_matches_pid=$(identity_value_pid "$identity_expected") || return 1
  identity_current=$(process_identity "$identity_matches_pid") || return 1
  [ "$identity_current" = "$identity_expected" ] || return 1
  identity_matches_pgid=$(identity_value_pgid "$identity_current") || return 1
  [ "$identity_matches_pgid" = "$identity_matches_pid" ]
}

identity_value_pid() {
  identity_value_line=$1
  printf '%s\n' "$identity_value_line" |
    awk '$1 ~ /^[0-9]+$/ { print $1; found=1 } END { exit !found }'
}

identity_value_pgid() {
  identity_pgid_line=$1
  printf '%s\n' "$identity_pgid_line" |
    awk '$2 ~ /^[0-9]+$/ { print $2; found=1 } END { exit !found }'
}

identity_value_matches() {
  identity_value_expected=$1
  identity_value_match_pid=$(identity_value_pid "$identity_value_expected") || return 1
  identity_value_current=$(process_identity "$identity_value_match_pid") || return 1
  [ "$identity_value_current" = "$identity_value_expected" ] || return 1
}

group_anchor_matches_value() {
  group_anchor_expected=$1
  identity_value_matches "$group_anchor_expected" || return 1
  group_anchor_pid=$(identity_value_pid "$group_anchor_expected") || return 1
  group_anchor_expected_pgid=$(identity_value_pgid "$group_anchor_expected") || return 1
  group_anchor_current=$(process_identity "$group_anchor_pid") || return 1
  group_anchor_current_pgid=$(identity_value_pgid "$group_anchor_current") || return 1
  [ "$group_anchor_current_pgid" = "$group_anchor_expected_pgid" ]
}

group_anchor_matches() {
  group_anchor_file=$1
  [ -r "$group_anchor_file" ] || return 1
  IFS= read -r group_anchor_value <"$group_anchor_file" || return 1
  group_anchor_matches_value "$group_anchor_value"
}

group_alive_with_identity_value() {
  group_value=$1
  [ -n "$group_value" ] || return 1
  group_anchor_matches_value "$group_value" || return 1
  group_value_pgid=$(identity_value_pgid "$group_value") || return 1
  group_exists "$group_value_pgid"
}

signal_group_with_identity_value() {
  signal_value=$1
  signal_value_name=$2
  [ -n "$signal_value" ] || return 0
  # A matching, TERM-immune anchor keeps this numeric PGID reserved until the
  # final KILL. Never signal a bare PGID after its identity has gone stale.
  group_anchor_matches_value "$signal_value" || return 1
  signal_value_pgid=$(identity_value_pgid "$signal_value") || return 1
  group_exists "$signal_value_pgid" || return 0
  kill -"$signal_value_name" "-$signal_value_pgid" 2>/dev/null || true
}

release_empty_wrapper_value() {
  release_value=$1
  release_value_file=$2
  [ -n "$release_value" ] || return 0
  group_anchor_matches_value "$release_value" || return 0
  release_value_pid=$(identity_value_pid "$release_value") || return 0
  release_value_pgid=$(identity_value_pgid "$release_value") || return 0
  if ! group_has_nonanchor "$release_value_pgid" "$release_value_pid"; then
    atomic_touch "$release_value_file"
  fi
}

group_exists() {
  group_exists_pid=$1
  kill -0 "-$group_exists_pid" 2>/dev/null
}

group_alive_with_identity() {
  group_alive_identity_file=$1
  group_anchor_matches "$group_alive_identity_file" || return 1
  group_alive_pgid=$(identity_pgid "$group_alive_identity_file") || return 1
  group_exists "$group_alive_pgid"
}

group_has_nonanchor() {
  group_member_pgid=$1
  group_anchor_pid=$2
  ps -axo pid= -o pgid= 2>/dev/null |
    awk -v group="$group_member_pgid" -v anchor="$group_anchor_pid" \
      '$2 == group && $1 != group && $1 != anchor { found=1 } END { exit !found }'
}

launch_detached() {
  if command -v setsid >/dev/null 2>&1; then
    setsid "$@" &
  else
    perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' "$@" &
  fi
  DETACHED_PID=$!
}

wait_for_file() {
  wait_file=$1
  wait_process_pid=$2
  wait_attempts=0
  while [ ! -e "$wait_file" ] && kill -0 "$wait_process_pid" 2>/dev/null && [ "$wait_attempts" -lt 1000 ]; do
    sleep 0.01
    wait_attempts=$((wait_attempts + 1))
  done
  [ -e "$wait_file" ] && kill -0 "$wait_process_pid" 2>/dev/null
}

hold_wrapper_mode() {
  hold_state=$1
  hold_event_pipe=$2
  shift 2
  [ "${1:-}" = "--" ] || exit 64
  shift
  [ "$#" -gt 0 ] || exit 64
  trap ':' TERM INT HUP USR1 USR2
  "$SCRIPT_PATH" --group-anchor "$hold_state" hold &
  hold_anchor_pid=$!
  if ! wait_for_file "$hold_state/hold-anchor-ready" "$hold_anchor_pid" ||
    ! group_anchor_matches "$hold_state/hold-identity" ||
    [ "$(identity_pid "$hold_state/hold-identity")" != "$hold_anchor_pid" ]; then
    exit 70
  fi
  hold_wrapper_pgid=$(identity_pgid "$hold_state/hold-identity") || exit 70
  [ "$hold_wrapper_pgid" = "$$" ] || exit 70
  atomic_write_line "$hold_state/hold-pid" "$hold_wrapper_pgid"
  atomic_touch "$hold_state/hold-ready"

  "$@" >"$hold_event_pipe" &
  hold_child=$!
  atomic_write_line "$hold_state/hold-child-pid" "$hold_child"
  while :; do
    set +e
    wait "$hold_child" 2>/dev/null
    hold_status=$?
    set -e
    if ! kill -0 "$hold_child" 2>/dev/null; then
      break
    fi
  done
  atomic_write_line "$hold_state/hold-status" "$hold_status"
  atomic_touch "$hold_state/hold-exited"
  while [ -d "$hold_state" ] && [ ! -e "$hold_state/hold-release" ]; do
    sleep_ms 20
  done
  exit "$hold_status"
}

worker_wrapper_mode() {
  worker_state=$1
  worker_term=$2
  worker_program=$3
  trap ':' TERM INT HUP USR1 USR2
  "$SCRIPT_PATH" --group-anchor "$worker_state" worker &
  worker_anchor_pid=$!
  if ! wait_for_file "$worker_state/worker-anchor-ready" "$worker_anchor_pid" ||
    ! group_anchor_matches "$worker_state/worker-identity" ||
    [ "$(identity_pid "$worker_state/worker-identity")" != "$worker_anchor_pid" ]; then
    exit 70
  fi
  worker_wrapper_pgid=$(identity_pgid "$worker_state/worker-identity") || exit 70
  [ "$worker_wrapper_pgid" = "$$" ] || exit 70
  atomic_write_line "$worker_state/worker-pid" "$worker_wrapper_pgid"
  atomic_touch "$worker_state/worker-ready"

  while [ -d "$worker_state" ] && [ ! -e "$worker_state/worker-start" ]; do
    if [ -e "$worker_state/authority-expired" ] || [ -e "$worker_state/stop-request" ]; then
      atomic_write_line "$worker_state/worker-status" 20
      atomic_touch "$worker_state/worker-exited"
      while [ -d "$worker_state" ] && [ ! -e "$worker_state/worker-release" ]; do
        sleep_ms 20
      done
      exit 20
    fi
    sleep_ms 10
  done
  [ -d "$worker_state" ] || exit 20
  [ ! -e "$worker_state/authority-expired" ] && [ ! -e "$worker_state/stop-request" ] || exit 20

  OCTOSTORE_FENCING_TERM=$worker_term
  OCTOSTORE_SUPERVISOR_READY_FILE=$worker_state/worker-application-ready
  export OCTOSTORE_FENCING_TERM
  export OCTOSTORE_SUPERVISOR_READY_FILE
  "$worker_program" &
  worker_child=$!
  atomic_write_line "$worker_state/worker-child-pid" "$worker_child"
  # A supervised authority-start event is not observable until this launch
  # acknowledgement exists. The guardian already owns and can contain the
  # process group, and the worker program has now been spawned inside it.
  atomic_touch "$worker_state/worker-started"
  while :; do
    set +e
    wait "$worker_child" 2>/dev/null
    worker_status=$?
    set -e
    if ! kill -0 "$worker_child" 2>/dev/null; then
      break
    fi
  done
  atomic_write_line "$worker_state/worker-status" "$worker_status"
  atomic_touch "$worker_state/worker-exited"
  while [ -d "$worker_state" ] && [ ! -e "$worker_state/worker-release" ]; do
    sleep_ms 20
  done
  exit "$worker_status"
}

group_anchor_mode() {
  group_anchor_state=$1
  group_anchor_name=$2
  case "$group_anchor_name" in hold|worker) ;; *) exit 64 ;; esac
  trap ':' TERM INT HUP USR1 USR2
  publish_group_anchor "$group_anchor_state/$group_anchor_name-identity" || exit 70
  atomic_touch "$group_anchor_state/$group_anchor_name-anchor-ready"
  while [ -d "$group_anchor_state" ] && [ ! -e "$group_anchor_state/$group_anchor_name-release" ]; do
    # TERM interrupts sleep; the anchor must remain alive through the final
    # KILL even though the supervisor itself runs with errexit enabled.
    sleep_ms 20 || true
  done

  if [ ! -e "$group_anchor_state/$group_anchor_name-release" ] && [ ! -d "$group_anchor_state" ]; then
    # The state directory is the containment authority.  Its unexpected
    # disappearance must fail closed: this anchor still proves that its exact
    # process group is ours, so it can kill the entire detached group before
    # a wrapper blocked in wait(1) becomes an orphan.
    group_anchor_identity=$(process_identity "$$") || exit 70
    group_anchor_pid=$(identity_value_pid "$group_anchor_identity") || exit 70
    group_anchor_pgid=$(identity_value_pgid "$group_anchor_identity") || exit 70
    [ "$group_anchor_pid" = "$$" ] || exit 70
    kill -KILL "-$group_anchor_pgid" 2>/dev/null || true
  fi
}

guardian_groups_alive() {
  group_alive_with_identity_value "$GUARDIAN_HOLD_IDENTITY" ||
    group_alive_with_identity_value "$GUARDIAN_WORKER_IDENTITY" ||
    group_alive_with_identity_value "$GUARDIAN_ESCAPED_IDENTITY"
}

guardian_signal_groups() {
  guardian_signal=$1
  signal_group_with_identity_value "$GUARDIAN_WORKER_IDENTITY" "$guardian_signal" || true
  signal_group_with_identity_value "$GUARDIAN_HOLD_IDENTITY" "$guardian_signal" || true
  signal_group_with_identity_value "$GUARDIAN_ESCAPED_IDENTITY" "$guardian_signal" || true
  if [ -n "${OCTOSTORE_SUPERVISOR_TEST_STALE_GROUP_IDENTITY:-}" ]; then
    signal_group_with_identity_value "$OCTOSTORE_SUPERVISOR_TEST_STALE_GROUP_IDENTITY" "$guardian_signal" || true
  fi
}

guardian_worker_group_intact() {
  [ -n "$GUARDIAN_WORKER_IDENTITY" ] || return 0
  [ -r "$GUARDIAN_STATE/worker-child-pid" ] || return 0
  IFS= read -r guardian_direct_worker_pid <"$GUARDIAN_STATE/worker-child-pid" || return 0
  case "$guardian_direct_worker_pid" in *[!0-9]*|'') return 1 ;; esac
  guardian_direct_worker_identity=$(process_identity "$guardian_direct_worker_pid") || return 0
  guardian_expected_worker_group=$(identity_value_pgid "$GUARDIAN_WORKER_IDENTITY") || return 1
  guardian_direct_worker_group=$(identity_value_pgid "$guardian_direct_worker_identity") || return 1
  [ "$guardian_direct_worker_group" = "$guardian_expected_worker_group" ] && return 0

  atomic_write_line "$GUARDIAN_STATE/worker-escape-identity" "$guardian_direct_worker_identity"
  echo "direct worker left the supervised process group; stopping protected work" >&2
  if [ "$guardian_direct_worker_pid" = "$guardian_direct_worker_group" ]; then
    # A direct setsid/setpgrp escape created a new group led by the same exact
    # process identity. Track that group so containment covers it as well.
    GUARDIAN_ESCAPED_IDENTITY=$guardian_direct_worker_identity
  else
    # An escape into an existing group cannot be signalled without risking
    # unrelated work. The cooperative-worker contract has been violated and
    # complete containment cannot be claimed.
    echo "direct worker escaped into a process group it does not lead" >&2
    GUARDIAN_CONTAINMENT_FAILED=1
  fi
  return 1
}

guardian_contain() {
  guardian_reason=$1
  guardian_active_kill_at=$2
  atomic_touch "$GUARDIAN_STATE/authority-expired"
  guardian_now=$(continuous_ms) || guardian_now=0
  guardian_kill_at=$((guardian_now + GUARDIAN_TERM_GRACE_MS))
  if [ "$guardian_active_kill_at" -gt 0 ] && [ "$guardian_active_kill_at" -lt "$guardian_kill_at" ]; then
    guardian_kill_at=$guardian_active_kill_at
  fi
  atomic_write_line "$GUARDIAN_STATE/guardian-result" "$guardian_reason"

  guardian_signal_groups TERM
  if [ -n "${OCTOSTORE_SUPERVISOR_TEST_WATCHDOG_NOTIFICATION_DIR:-}" ] && [ "$guardian_reason" = "authority_expired" ]; then
    atomic_touch "$OCTOSTORE_SUPERVISOR_TEST_WATCHDOG_NOTIFICATION_DIR/ready"
    while [ ! -e "$OCTOSTORE_SUPERVISOR_TEST_WATCHDOG_NOTIFICATION_DIR/continue" ]; do
      guardian_hook_now=$(continuous_ms) || guardian_hook_now=$guardian_kill_at
      [ "$guardian_hook_now" -lt "$guardian_kill_at" ] || break
      sleep_ms 5
    done
  fi

  while guardian_groups_alive; do
    release_empty_wrapper_value "$GUARDIAN_WORKER_IDENTITY" "$GUARDIAN_STATE/worker-release"
    release_empty_wrapper_value "$GUARDIAN_HOLD_IDENTITY" "$GUARDIAN_STATE/hold-release"
    guardian_now=$(continuous_ms) || guardian_now=$guardian_kill_at
    [ "$guardian_now" -lt "$guardian_kill_at" ] || break
    guardian_signal_groups TERM
    guardian_slice=$((guardian_kill_at - guardian_now))
    [ "$guardian_slice" -le 20 ] || guardian_slice=20
    sleep_ms "$guardian_slice"
  done

  if guardian_groups_alive; then
    guardian_signal_groups KILL
  fi
  guardian_verify_started=$(continuous_ms) || guardian_verify_started=$guardian_kill_at
  guardian_verify_base=$guardian_kill_at
  if [ "$guardian_verify_started" -gt "$guardian_verify_base" ]; then
    guardian_verify_base=$guardian_verify_started
  fi
  guardian_verify_deadline=$((guardian_verify_base + WATCHDOG_DEADLINE_RESERVE_MS))
  while guardian_groups_alive; do
    guardian_verify_now=$(continuous_ms) || guardian_verify_now=$guardian_verify_deadline
    [ "$guardian_verify_now" -lt "$guardian_verify_deadline" ] || break
    sleep_ms 10
  done
  if guardian_groups_alive; then
    echo "protected process groups survived guardian containment" >&2
    GUARDIAN_CONTAINMENT_FAILED=1
  fi
  atomic_write_line "$GUARDIAN_STATE/guardian-contained" "$GUARDIAN_CONTAINMENT_FAILED"
}

guardian_monitor_mode() {
  guardian_monitor_state=$1
  guardian_monitor_main_pid=$2
  shift 2
  [ "$#" -gt 0 ] || exit 64
  launch_detached "$SCRIPT_PATH" --guardian "$guardian_monitor_state" "$@"
  guardian_monitor_child=$DETACHED_PID
  atomic_write_line "$guardian_monitor_state/guardian-launched-pid" "$guardian_monitor_child"
  set +e
  wait "$guardian_monitor_child"
  guardian_monitor_status=$?
  set -e
  if [ -d "$guardian_monitor_state" ]; then
    atomic_write_line "$guardian_monitor_state/guardian-monitor-status" "$guardian_monitor_status" || true
  fi
  kill -USR1 "$guardian_monitor_main_pid" 2>/dev/null || true
  exit "$guardian_monitor_status"
}

guardian_mode() {
  GUARDIAN_STATE=$1
  guardian_liveness_pipe=$2
  GUARDIAN_TERM_GRACE_MS=$3
  guardian_worker_program=$4
  shift 4
  [ "${1:-}" = "--" ] || exit 64
  shift
  [ "$#" -gt 0 ] || exit 64
  trap ':' TERM INT HUP USR1 USR2
  GUARDIAN_CONTAINMENT_FAILED=0
  GUARDIAN_ESCAPED_IDENTITY=
  GUARDIAN_HOLD_IDENTITY=
  GUARDIAN_WORKER_IDENTITY=
  guardian_worker_pid=

  publish_group_identity "$GUARDIAN_STATE/guardian-identity" || exit 70
  exec 7<"$guardian_liveness_pipe"
  (
    trap 'exit 0' TERM INT HUP
    while IFS= read -r _ <&7; do :; done
    atomic_touch "$GUARDIAN_STATE/supervisor-dead" 2>/dev/null || true
  ) &
  guardian_liveness_reader=$!

  launch_detached "$SCRIPT_PATH" --hold-wrapper "$GUARDIAN_STATE" "$GUARDIAN_STATE/events" -- "$@"
  guardian_hold_pid=$DETACHED_PID
  if ! wait_for_file "$GUARDIAN_STATE/hold-ready" "$guardian_hold_pid" ||
    ! group_anchor_matches "$GUARDIAN_STATE/hold-identity" ||
    [ "$(identity_pgid "$GUARDIAN_STATE/hold-identity")" != "$guardian_hold_pid" ]; then
    if [ -r "$GUARDIAN_STATE/hold-identity" ]; then
      IFS= read -r GUARDIAN_HOLD_IDENTITY <"$GUARDIAN_STATE/hold-identity" || GUARDIAN_HOLD_IDENTITY=
    else
      # The launch PID is only usable while its complete, current identity
      # still proves it leads the detached group.  Do not fall back to a bare
      # numeric PGID if that proof has disappeared.
      GUARDIAN_HOLD_IDENTITY=$(process_identity "$guardian_hold_pid") || GUARDIAN_HOLD_IDENTITY=
    fi
    atomic_write_line "$GUARDIAN_STATE/guardian-result" hold_start_failed
    guardian_contain hold_start_failed 0
    kill -TERM "$guardian_liveness_reader" 2>/dev/null || true
    wait "$guardian_liveness_reader" 2>/dev/null || true
    # Keep the state and identities for recovery inspection.  If the test
    # harness tears the directory down, the in-group anchor fails closed.
    exit 70
  fi
  IFS= read -r GUARDIAN_HOLD_IDENTITY <"$GUARDIAN_STATE/hold-identity" || exit 70
  atomic_touch "$GUARDIAN_STATE/guardian-ready"

  guardian_generation=0
  guardian_fire_at=0
  guardian_kill_at=0
  guardian_reason=
  while [ -z "$guardian_reason" ]; do
    if [ -e "$GUARDIAN_STATE/supervisor-dead" ]; then
      guardian_reason=supervisor_dead
      break
    fi
    if [ -e "$GUARDIAN_STATE/stop-request" ]; then
      guardian_reason=stop_requested
      break
    fi
    if [ -e "$GUARDIAN_STATE/worker-exited" ]; then
      guardian_reason=worker_exited
      break
    fi
    if [ -e "$GUARDIAN_STATE/hold-exited" ]; then
      guardian_reason=hold_exited
      break
    fi
    if ! guardian_worker_group_intact; then
      guardian_reason=worker_group_escape
      break
    fi

    if [ -r "$GUARDIAN_STATE/authority-state" ]; then
      IFS=' ' read -r candidate_generation candidate_fire_at candidate_kill_at <"$GUARDIAN_STATE/authority-state" || {
        guardian_reason=invalid_authority_state
        break
      }
      case "$candidate_generation:$candidate_fire_at:$candidate_kill_at" in
        *[!0-9:]*|:*|*::*|*:) guardian_reason=invalid_authority_state; break ;;
      esac
      if [ "$candidate_generation" -gt "$guardian_generation" ]; then
        guardian_now=$(continuous_ms) || {
          guardian_reason=clock_failed
          break
        }
        if [ "$guardian_generation" -gt 0 ] && [ "$guardian_now" -ge "$guardian_fire_at" ]; then
          guardian_reason=authority_expired
          break
        fi
        if [ "$candidate_generation" -ne $((guardian_generation + 1)) ] ||
          [ "$candidate_fire_at" -le "$guardian_now" ] ||
          [ "$candidate_kill_at" -lt "$candidate_fire_at" ]; then
          guardian_reason=invalid_authority_state
          break
        fi
        guardian_generation=$candidate_generation
        guardian_fire_at=$candidate_fire_at
        guardian_kill_at=$candidate_kill_at
        atomic_write_line "$GUARDIAN_STATE/authority-ack" "$guardian_generation"
      elif [ "$candidate_generation" -lt "$guardian_generation" ]; then
        guardian_reason=invalid_authority_state
        break
      fi
    fi

    if [ -z "$GUARDIAN_WORKER_IDENTITY" ] && [ -r "$GUARDIAN_STATE/worker-request" ]; then
      IFS= read -r guardian_worker_term <"$GUARDIAN_STATE/worker-request" || guardian_worker_term=
      case "$guardian_worker_term" in *[!0-9]*|'') guardian_reason=invalid_worker_request; break ;; esac
      [ "$guardian_generation" -gt 0 ] || { guardian_reason=invalid_worker_request; break; }
      launch_detached "$SCRIPT_PATH" --worker-wrapper "$GUARDIAN_STATE" "$guardian_worker_term" "$guardian_worker_program"
      guardian_worker_pid=$DETACHED_PID
      guardian_worker_attempts=0
      while [ ! -e "$GUARDIAN_STATE/worker-ready" ] && kill -0 "$guardian_worker_pid" 2>/dev/null && [ "$guardian_worker_attempts" -lt 500 ]; do
        if [ -e "$GUARDIAN_STATE/supervisor-dead" ]; then
          guardian_reason=supervisor_dead
          break
        fi
        guardian_worker_now=$(continuous_ms) || guardian_worker_now=$guardian_fire_at
        if [ "$guardian_worker_now" -ge "$guardian_fire_at" ]; then
          guardian_reason=authority_expired
          break
        fi
        sleep_ms 10
        guardian_worker_attempts=$((guardian_worker_attempts + 1))
      done
      [ -z "$guardian_reason" ] || continue
      if [ ! -e "$GUARDIAN_STATE/worker-ready" ] ||
        ! group_anchor_matches "$GUARDIAN_STATE/worker-identity" ||
        [ "$(identity_pgid "$GUARDIAN_STATE/worker-identity")" != "$guardian_worker_pid" ]; then
        guardian_reason=worker_start_failed
        break
      fi
      IFS= read -r GUARDIAN_WORKER_IDENTITY <"$GUARDIAN_STATE/worker-identity" || {
        guardian_reason=worker_start_failed
        break
      }
      atomic_touch "$GUARDIAN_STATE/guardian-worker-ready"
    fi

    if [ "$guardian_generation" -gt 0 ]; then
      guardian_now=$(continuous_ms) || {
        guardian_reason=clock_failed
        break
      }
      if [ "$guardian_now" -ge "$guardian_fire_at" ]; then
        guardian_reason=authority_expired
        break
      fi
      guardian_sleep=$((guardian_fire_at - guardian_now))
      [ "$guardian_sleep" -le 20 ] || guardian_sleep=20
      sleep_ms "$guardian_sleep"
    else
      sleep_ms 20
    fi
  done

  guardian_contain "$guardian_reason" "$guardian_kill_at"
  # The reader's exit is the liveness capability. Waiting on the child itself
  # cannot be defeated if an external cleanup removes the state directory
  # before its diagnostic marker is observed.
  wait "$guardian_liveness_reader" 2>/dev/null || true
  if ! kill -0 "$guardian_hold_pid" 2>/dev/null; then
    wait "$guardian_hold_pid" 2>/dev/null || true
  fi
  if [ -n "$guardian_worker_pid" ] && ! kill -0 "$guardian_worker_pid" 2>/dev/null; then
    wait "$guardian_worker_pid" 2>/dev/null || true
  fi
  guardian_exit=$GUARDIAN_CONTAINMENT_FAILED
  if [ "$guardian_exit" -ne 0 ]; then
    # A failed containment attempt must leave its identity and diagnostic
    # records intact for the main-process fallback or operator recovery.
    exit "$guardian_exit"
  fi
  rm -rf "$GUARDIAN_STATE"
  exit "$guardian_exit"
}

case "${1:-}" in
  --check-dependencies)
    [ "$#" -eq 1 ] || exit 64
    check_dependencies || exit 64
    echo "octostore-supervisor dependencies are available"
    exit 0
    ;;
  --hold-wrapper)
    shift
    hold_wrapper_mode "$@"
    ;;
  --worker-wrapper)
    shift
    worker_wrapper_mode "$@"
    ;;
  --group-anchor)
    shift
    group_anchor_mode "$@"
    ;;
  --guardian)
    shift
    guardian_mode "$@"
    ;;
  --guardian-monitor)
    shift
    guardian_monitor_mode "$@"
    ;;
esac

usage() {
  echo "usage: $0 <election|lock> <expected-name> <expected-candidate|-> <worker-program> -- <octostore hold command...>" >&2
  exit 64
}

[ "$#" -ge 6 ] || usage
EXPECTED_KIND=$1
EXPECTED_NAME=$2
EXPECTED_CANDIDATE=$3
WORKER=$4
shift 4
[ "$1" = "--" ] || usage
shift
[ "$#" -gt 0 ] || usage
[ "$EXPECTED_KIND" = election ] || [ "$EXPECTED_KIND" = lock ] || usage
if [ "$EXPECTED_KIND" = election ]; then
  [ "$EXPECTED_CANDIDATE" != - ] && [ -n "$EXPECTED_CANDIDATE" ] || usage
else
  [ "$EXPECTED_CANDIDATE" = - ] || usage
fi
[ -x "$WORKER" ] || {
  echo "worker program is not executable: $WORKER" >&2
  exit 64
}
check_dependencies || exit 64

MAX_SILENCE_SECONDS=${OCTOSTORE_SUPERVISOR_MAX_SILENCE_SECONDS:-20}
case "$MAX_SILENCE_SECONDS" in *[!0-9]*|'') echo "maximum silence must be a positive integer" >&2; exit 64 ;; esac
[ "$MAX_SILENCE_SECONDS" -gt 0 ] || { echo "maximum silence must be positive" >&2; exit 64; }
[ "$MAX_SILENCE_SECONDS" -le 20 ] || {
  echo "maximum silence may only shorten the 20-second session-safety cap" >&2
  exit 64
}
TERM_GRACE_SECONDS=${OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS:-1}
case "$TERM_GRACE_SECONDS" in *[!0-9]*|'') echo "TERM grace must be a positive integer" >&2; exit 64 ;; esac
[ "$TERM_GRACE_SECONDS" -gt 0 ] || { echo "TERM grace must be positive" >&2; exit 64; }
TERM_GRACE_MS=$((TERM_GRACE_SECONDS * 1000))
REQUIRE_WORKER_READY=${OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY:-0}
case "$REQUIRE_WORKER_READY" in
  0|1) ;;
  *) echo "worker readiness requirement must be 0 or 1" >&2; exit 64 ;;
esac

SUPERVISOR_TMP=$(mktemp -d "${TMPDIR:-/tmp}/octostore-supervisor.XXXXXX")
EVENT_PIPE=$SUPERVISOR_TMP/events
LIVENESS_PIPE=$SUPERVISOR_TMP/supervisor-liveness
mkfifo "$EVENT_PIPE" "$LIVENESS_PIPE"
GUARDIAN_PID=
WORKER_PID=
AUTHORITY_GENERATION=0
ACTIVE_FIRE_AT=0
ACTIVE_TERM=
LAST_SEQUENCE=0
MAIN_CLEANING=0
LIVENESS_OPEN=0
MAIN_PID=$$
CONTAINED_FINISH_CODE=70
CONTAINMENT_PROVED=1
MAIN_HOLD_IDENTITY=
MAIN_WORKER_IDENTITY=

for test_directory in \
  "${OCTOSTORE_SUPERVISOR_TEST_HANDOFF_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_RENEWAL_HANDOFF_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_RENEWAL_ARM_FAILURE_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_PRE_READ_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_PRE_MONITOR_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_WATCHDOG_NOTIFICATION_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_WORKER_PUBLISHED_DIR:-}" \
  "${OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR:-}"; do
  if [ -n "$test_directory" ] && [ ! -d "$test_directory" ]; then
    echo "supervisor test directory does not exist: $test_directory" >&2
    exit 64
  fi
done

monitor_delay_ms() {
  monitor_remaining_ms=$1
  monitor_reserve_ms=$((TERM_GRACE_MS + WATCHDOG_DEADLINE_RESERVE_MS))
  [ "$monitor_remaining_ms" -gt "$monitor_reserve_ms" ] || return 1
  monitor_delay=$((monitor_remaining_ms - monitor_reserve_ms))
  if [ "$EXPECTED_KIND" = lock ]; then
    monitor_silence_cap=$((MAX_SILENCE_SECONDS * 1000))
    if [ "$monitor_delay" -gt "$monitor_silence_cap" ]; then
      monitor_delay=$monitor_silence_cap
    fi
  fi
  [ "$monitor_delay" -ge "$MIN_WORK_WINDOW_MS" ] || return 1
  printf '%s\n' "$monitor_delay"
}

fresh_authority_budget() {
  fresh_emitted_remaining_ms=$1
  fresh_emitted_unix_ms=$2
  fresh_emitted_continuous_ms=$3
  case "$fresh_emitted_remaining_ms" in *[!0-9]*|'') return 1 ;; esac
  case "$fresh_emitted_unix_ms" in *[!0-9]*|'') return 1 ;; esac
  case "$fresh_emitted_continuous_ms" in *[!0-9]*|'') return 1 ;; esac
  [ "${#fresh_emitted_remaining_ms}" -le 9 ] || return 1
  [ "${#fresh_emitted_unix_ms}" -le 13 ] || return 1
  [ "${#fresh_emitted_continuous_ms}" -le 15 ] || return 1
  [ "$fresh_emitted_remaining_ms" -gt 0 ] && [ "$fresh_emitted_remaining_ms" -le "$MAX_AUTHORITY_BUDGET_MS" ] || return 1
  fresh_received_unix_ms=$(jq -nr 'now * 1000 | floor') || return 1
  fresh_received_continuous_ms=$(continuous_ms) || return 1
  case "$fresh_received_unix_ms" in *[!0-9]*|'') return 1 ;; esac
  case "$fresh_received_continuous_ms" in *[!0-9]*|'') return 1 ;; esac
  [ "$fresh_emitted_unix_ms" -le "$fresh_received_unix_ms" ] || return 1
  [ "$fresh_emitted_continuous_ms" -le "$fresh_received_continuous_ms" ] || return 1
  fresh_wall_age=$((fresh_received_unix_ms - fresh_emitted_unix_ms))
  fresh_continuous_age=$((fresh_received_continuous_ms - fresh_emitted_continuous_ms))
  fresh_event_age=$fresh_wall_age
  if [ "$fresh_continuous_age" -gt "$fresh_event_age" ]; then
    fresh_event_age=$fresh_continuous_age
  fi
  [ "$fresh_event_age" -lt "$fresh_emitted_remaining_ms" ] || return 1
  printf '%s\n' $((fresh_emitted_remaining_ms - fresh_event_age))
}

pause_hook() {
  pause_directory=$1
  pause_payload=${2:-}
  [ -n "$pause_directory" ] || return 0
  if [ -n "$pause_payload" ]; then
    atomic_write_line "$pause_directory/ready" "$pause_payload"
  else
    atomic_touch "$pause_directory/ready"
  fi
  while [ ! -e "$pause_directory/continue" ]; do
    [ ! -e "$SUPERVISOR_TMP/authority-expired" ] || return 1
    kill -0 "$GUARDIAN_PID" 2>/dev/null || return 1
    sleep 0.01
  done
}

arm_authority() {
  arm_remaining=$1
  arm_unix=$2
  arm_continuous=$3
  arm_fresh=$(fresh_authority_budget "$arm_remaining" "$arm_unix" "$arm_continuous") || return 1
  arm_delay=$(monitor_delay_ms "$arm_fresh") || return 1
  arm_now=$(continuous_ms) || return 1
  arm_fire_at=$((arm_now + arm_delay))
  arm_original_fire_at=$((arm_continuous + arm_remaining - TERM_GRACE_MS - WATCHDOG_DEADLINE_RESERVE_MS))
  if [ "$arm_original_fire_at" -lt "$arm_fire_at" ]; then
    arm_fire_at=$arm_original_fire_at
  fi
  arm_kill_at=$((arm_fire_at + TERM_GRACE_MS))

  if [ "$AUTHORITY_GENERATION" -eq 0 ]; then
    pause_hook "${OCTOSTORE_SUPERVISOR_TEST_PRE_MONITOR_DIR:-}" || return 1
  elif [ -n "${OCTOSTORE_SUPERVISOR_TEST_RENEWAL_ARM_FAILURE_DIR:-}" ]; then
    pause_hook "$OCTOSTORE_SUPERVISOR_TEST_RENEWAL_ARM_FAILURE_DIR" "$WORKER_PID" || true
    return 1
  fi

  arm_current=$(continuous_ms) || return 1
  [ "$arm_current" -lt "$arm_fire_at" ] || return 1
  arm_generation=$((AUTHORITY_GENERATION + 1))
  atomic_write_line "$SUPERVISOR_TMP/authority-state" "$arm_generation $arm_fire_at $arm_kill_at"
  arm_attempts=0
  while [ "$arm_attempts" -lt 500 ]; do
    if [ -r "$SUPERVISOR_TMP/authority-ack" ]; then
      IFS= read -r arm_ack <"$SUPERVISOR_TMP/authority-ack" || arm_ack=
      if [ "$arm_ack" = "$arm_generation" ]; then
        break
      fi
    fi
    [ ! -e "$SUPERVISOR_TMP/authority-expired" ] || return 1
    kill -0 "$GUARDIAN_PID" 2>/dev/null || return 1
    arm_current=$(continuous_ms) || return 1
    [ "$arm_current" -lt "$arm_fire_at" ] || return 1
    sleep 0.01
    arm_attempts=$((arm_attempts + 1))
  done
  [ "${arm_ack:-}" = "$arm_generation" ] || return 1
  fresh_authority_budget "$arm_remaining" "$arm_unix" "$arm_continuous" >/dev/null || return 1
  AUTHORITY_GENERATION=$arm_generation
  ACTIVE_FIRE_AT=$arm_fire_at

  if [ "$AUTHORITY_GENERATION" -gt 1 ] && [ -n "${OCTOSTORE_SUPERVISOR_TEST_RENEWAL_HANDOFF_DIR:-}" ]; then
    pause_hook "$OCTOSTORE_SUPERVISOR_TEST_RENEWAL_HANDOFF_DIR" "$WORKER_PID" || return 1
    atomic_touch "$OCTOSTORE_SUPERVISOR_TEST_RENEWAL_HANDOFF_DIR/done"
  fi
}

start_worker() {
  start_term=$1
  start_remaining=$2
  start_unix=$3
  start_continuous=$4
  arm_authority "$start_remaining" "$start_unix" "$start_continuous" || return 1
  rm -f "$SUPERVISOR_TMP/worker-identity" "$SUPERVISOR_TMP/worker-pid" \
    "$SUPERVISOR_TMP/worker-anchor-ready" "$SUPERVISOR_TMP/worker-ready" \
    "$SUPERVISOR_TMP/worker-start" \
    "$SUPERVISOR_TMP/worker-started" "$SUPERVISOR_TMP/worker-child-pid" \
    "$SUPERVISOR_TMP/worker-application-ready" \
    "$SUPERVISOR_TMP/worker-exited" "$SUPERVISOR_TMP/worker-status" \
    "$SUPERVISOR_TMP/worker-release" "$SUPERVISOR_TMP/worker-request" \
    "$SUPERVISOR_TMP/guardian-worker-ready"
  atomic_write_line "$SUPERVISOR_TMP/worker-request" "$start_term"
  if ! wait_for_file "$SUPERVISOR_TMP/guardian-worker-ready" "$GUARDIAN_PID" ||
    ! group_anchor_matches "$SUPERVISOR_TMP/worker-identity"; then
    echo "worker process group could not be validated and published" >&2
    return 1
  fi
  WORKER_PID=$(identity_pgid "$SUPERVISOR_TMP/worker-identity") || return 1
  IFS= read -r MAIN_WORKER_IDENTITY <"$SUPERVISOR_TMP/worker-identity" || return 1
  if [ -n "${OCTOSTORE_SUPERVISOR_TEST_WORKER_PUBLISHED_DIR:-}" ]; then
    pause_hook "$OCTOSTORE_SUPERVISOR_TEST_WORKER_PUBLISHED_DIR" "$WORKER_PID" || return 1
  fi
  pause_hook "${OCTOSTORE_SUPERVISOR_TEST_HANDOFF_DIR:-}" "$WORKER_PID" || return 1
  fresh_authority_budget "$start_remaining" "$start_unix" "$start_continuous" >/dev/null || return 1
  start_now=$(continuous_ms) || return 1
  [ "$start_now" -lt "$ACTIVE_FIRE_AT" ] || return 1
  atomic_touch "$SUPERVISOR_TMP/worker-start"
  if ! wait_for_file "$SUPERVISOR_TMP/worker-started" "$WORKER_PID" ||
    ! group_alive_with_identity "$SUPERVISOR_TMP/worker-identity"; then
    echo "worker process did not acknowledge launch" >&2
    return 1
  fi
  if [ "$REQUIRE_WORKER_READY" -eq 1 ] &&
    ! wait_for_file "$SUPERVISOR_TMP/worker-application-ready" "$WORKER_PID"; then
    echo "worker process did not acknowledge application readiness" >&2
    return 1
  fi
  fresh_authority_budget "$start_remaining" "$start_unix" "$start_continuous" >/dev/null || return 1
  start_now=$(continuous_ms) || return 1
  [ "$start_now" -lt "$ACTIVE_FIRE_AT" ] || return 1
}

emit_lifecycle_event() {
  jq -cn \
    --arg event "$event_name" \
    --arg kind "$event_kind" \
    --arg name "$coordination_name" \
    --arg candidate "$candidate_id" \
    --arg term "$event_term" \
    --argjson sequence "$sequence" '
      {schema_version: 1, source: "octostore-supervisor", sequence: $sequence,
       event: $event, kind: $kind, name: $name}
      + (if $candidate == "" then {} else {candidate_id: $candidate} end)
      + (if $term == "" then {} else {term: ($term | tonumber)} end)
    '
}

request_guardian_stop() {
  [ -n "$GUARDIAN_PID" ] || return 0
  [ -d "$SUPERVISOR_TMP" ] || return 0
  atomic_touch "$SUPERVISOR_TMP/stop-request" || true
}

close_liveness() {
  [ "$LIVENESS_OPEN" -eq 1 ] || return 0
  exec 8>&-
  LIVENESS_OPEN=0
}

wait_guardian() {
  [ -n "$GUARDIAN_PID" ] || return 0
  set +e
  wait "$GUARDIAN_PID"
  guardian_status=$?
  set -e
  GUARDIAN_PID=
  return "$guardian_status"
}

fallback_groups_alive() {
  group_alive_with_identity_value "$FALLBACK_WORKER_IDENTITY" ||
    group_alive_with_identity_value "$FALLBACK_HOLD_IDENTITY" ||
    group_alive_with_identity_value "$FALLBACK_ESCAPED_IDENTITY"
}

fallback_signal_groups() {
  fallback_signal=$1
  signal_group_with_identity_value "$FALLBACK_WORKER_IDENTITY" "$fallback_signal" || true
  signal_group_with_identity_value "$FALLBACK_HOLD_IDENTITY" "$fallback_signal" || true
  signal_group_with_identity_value "$FALLBACK_ESCAPED_IDENTITY" "$fallback_signal" || true
  if [ -n "${OCTOSTORE_SUPERVISOR_TEST_STALE_GROUP_IDENTITY:-}" ]; then
    signal_group_with_identity_value "$OCTOSTORE_SUPERVISOR_TEST_STALE_GROUP_IDENTITY" "$fallback_signal" || true
  fi
}

main_fallback_contain() {
  FALLBACK_HOLD_IDENTITY=$MAIN_HOLD_IDENTITY
  FALLBACK_WORKER_IDENTITY=$MAIN_WORKER_IDENTITY
  FALLBACK_ESCAPED_IDENTITY=
  FALLBACK_CONTAINMENT_FAILED=0
  fallback_attempts=0
  while [ "$fallback_attempts" -lt 25 ]; do
    if [ -z "$FALLBACK_HOLD_IDENTITY" ] && [ -r "$SUPERVISOR_TMP/hold-identity" ]; then
      IFS= read -r FALLBACK_HOLD_IDENTITY <"$SUPERVISOR_TMP/hold-identity" || FALLBACK_HOLD_IDENTITY=
    fi
    if [ -z "$FALLBACK_WORKER_IDENTITY" ] && [ -r "$SUPERVISOR_TMP/worker-identity" ]; then
      IFS= read -r FALLBACK_WORKER_IDENTITY <"$SUPERVISOR_TMP/worker-identity" || FALLBACK_WORKER_IDENTITY=
    fi
    if [ -z "$FALLBACK_ESCAPED_IDENTITY" ] && [ -r "$SUPERVISOR_TMP/worker-escape-identity" ]; then
      IFS= read -r FALLBACK_ESCAPED_IDENTITY <"$SUPERVISOR_TMP/worker-escape-identity" || FALLBACK_ESCAPED_IDENTITY=
      if [ -n "$FALLBACK_ESCAPED_IDENTITY" ]; then
        fallback_escaped_pid=$(identity_value_pid "$FALLBACK_ESCAPED_IDENTITY") || fallback_escaped_pid=
        fallback_escaped_pgid=$(identity_value_pgid "$FALLBACK_ESCAPED_IDENTITY") || fallback_escaped_pgid=
        if [ -z "$fallback_escaped_pid" ] || [ "$fallback_escaped_pid" != "$fallback_escaped_pgid" ]; then
          echo "fallback cannot safely signal an escaped worker in an unrelated group" >&2
          FALLBACK_CONTAINMENT_FAILED=1
          FALLBACK_ESCAPED_IDENTITY=
        fi
      fi
    fi
    if [ -n "$FALLBACK_HOLD_IDENTITY" ] &&
      { [ ! -e "$SUPERVISOR_TMP/worker-request" ] || [ -n "$FALLBACK_WORKER_IDENTITY" ]; }; then
      break
    fi
    sleep_ms 10
    fallback_attempts=$((fallback_attempts + 1))
  done
  if [ -e "$SUPERVISOR_TMP/guardian-ready" ] && [ -z "$FALLBACK_HOLD_IDENTITY" ]; then
    echo "fallback could not recover the published hold process-group identity" >&2
    FALLBACK_CONTAINMENT_FAILED=1
  fi
  if [ -e "$SUPERVISOR_TMP/worker-ready" ] && [ -z "$FALLBACK_WORKER_IDENTITY" ]; then
    echo "fallback could not recover the published worker process-group identity" >&2
    FALLBACK_CONTAINMENT_FAILED=1
  fi

  atomic_touch "$SUPERVISOR_TMP/authority-expired" || true
  fallback_signal_groups TERM
  fallback_now=$(continuous_ms) || fallback_now=0
  fallback_kill_at=$((fallback_now + TERM_GRACE_MS))
  while fallback_groups_alive; do
    release_empty_wrapper_value "$FALLBACK_WORKER_IDENTITY" "$SUPERVISOR_TMP/worker-release"
    release_empty_wrapper_value "$FALLBACK_HOLD_IDENTITY" "$SUPERVISOR_TMP/hold-release"
    fallback_now=$(continuous_ms) || fallback_now=$fallback_kill_at
    [ "$fallback_now" -lt "$fallback_kill_at" ] || break
    fallback_signal_groups TERM
    fallback_slice=$((fallback_kill_at - fallback_now))
    [ "$fallback_slice" -le 20 ] || fallback_slice=20
    sleep_ms "$fallback_slice"
  done
  if fallback_groups_alive; then
    fallback_signal_groups KILL
  fi
  fallback_verify_started=$(continuous_ms) || fallback_verify_started=$fallback_kill_at
  fallback_verify_deadline=$((fallback_verify_started + WATCHDOG_DEADLINE_RESERVE_MS))
  while fallback_groups_alive; do
    fallback_now=$(continuous_ms) || fallback_now=$fallback_verify_deadline
    [ "$fallback_now" -lt "$fallback_verify_deadline" ] || break
    sleep_ms 10
  done
  if fallback_groups_alive; then
    echo "protected process groups survived main-process fallback containment" >&2
    FALLBACK_CONTAINMENT_FAILED=1
  fi
  atomic_write_line "$SUPERVISOR_TMP/guardian-contained" "$FALLBACK_CONTAINMENT_FAILED" || true
  [ "$FALLBACK_CONTAINMENT_FAILED" -eq 0 ]
}

contain_main() {
  contain_requested_code=$1
  CONTAINED_FINISH_CODE=$contain_requested_code
  CONTAINMENT_PROVED=1
  MAIN_CLEANING=1
  trap '' INT TERM HUP USR1
  request_guardian_stop

  # This hook freezes the exact point after the detached guardian has proved
  # containment but before its liveness capability is retired. It exists only
  # for the failure-injection suite.
  if [ -n "${OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR:-}" ] &&
    [ -n "$GUARDIAN_PID" ] && kill -0 "$GUARDIAN_PID" 2>/dev/null; then
    contain_attempts=0
    while [ ! -e "$SUPERVISOR_TMP/guardian-contained" ] && [ "$contain_attempts" -lt 500 ]; do
      sleep 0.01
      contain_attempts=$((contain_attempts + 1))
    done
    if [ -e "$SUPERVISOR_TMP/guardian-contained" ]; then
      atomic_touch "$OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR/ready"
      while [ ! -e "$OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR/continue" ]; do
        sleep 0.01
      done
    fi
  fi

  close_liveness
  if ! wait_guardian; then
    echo "detached guardian exited before reporting verified containment; using main-process fallback" >&2
    if [ -d "$SUPERVISOR_TMP" ] && main_fallback_contain; then
      # The guardian did not prove its own result.  Preserve the fallback's
      # identities and containment record instead of deleting recovery
      # evidence; normal successful guardian exits remove their own state.
      :
    else
      echo "guardian and fallback could not verify complete containment" >&2
      CONTAINED_FINISH_CODE=70
      CONTAINMENT_PROVED=0
    fi
  fi
  if [ -n "${OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR:-}" ] &&
    [ ! -e "$OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR/ready" ]; then
    atomic_touch "$OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR/ready"
    while [ ! -e "$OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR/continue" ]; do
      sleep 0.01
    done
  fi
}

finish_main() {
  finish_code=$1
  contain_main "$finish_code"
  exit "$CONTAINED_FINISH_CODE"
}

finish_terminal_event() {
  terminal_code=$1
  contain_main "$terminal_code"
  if [ "$CONTAINMENT_PROVED" -eq 1 ] && [ "$CONTAINED_FINISH_CODE" -eq "$terminal_code" ]; then
    emit_lifecycle_event
  else
    echo "terminal event withheld because containment could not be proved" >&2
  fi
  exit "$CONTAINED_FINISH_CODE"
}

# shellcheck disable=SC2329 # invoked by the EXIT trap
cleanup_main() {
  [ "$MAIN_CLEANING" -eq 0 ] || return 0
  contain_main 70 || true
}

# shellcheck disable=SC2329 # invoked by signal traps
forward_signal() {
  forward_code=$1
  finish_main "$forward_code"
}

# shellcheck disable=SC2329 # invoked by the USR1 trap
guardian_unavailable() {
  [ "$MAIN_CLEANING" -eq 0 ] || return 0
  echo "detached guardian became unavailable" >&2
  if [ -n "$ACTIVE_TERM" ]; then
    event_name=uncertain
    event_kind=$EXPECTED_KIND
    coordination_name=$EXPECTED_NAME
    if [ "$EXPECTED_KIND" = election ]; then
      candidate_id=$EXPECTED_CANDIDATE
    else
      candidate_id=
    fi
    event_term=$ACTIVE_TERM
    sequence=$((LAST_SEQUENCE + 1))
    finish_terminal_event 20
  fi
  finish_main 70
}

trap cleanup_main EXIT
trap 'forward_signal 130' INT
trap 'forward_signal 143' TERM
trap 'forward_signal 129' HUP
trap guardian_unavailable USR1

"$SCRIPT_PATH" --guardian-monitor "$SUPERVISOR_TMP" "$MAIN_PID" \
  "$LIVENESS_PIPE" "$TERM_GRACE_MS" "$WORKER" -- "$@" &
GUARDIAN_PID=$!
# Opening a FIFO read/write is non-blocking and keeps one unambiguous lifetime
# capability in this process. Detached children close descriptor 8 before
# doing any work, so EOF still proves that this supervisor is gone.
exec 8<>"$LIVENESS_PIPE"
LIVENESS_OPEN=1
if ! wait_for_file "$SUPERVISOR_TMP/guardian-ready" "$GUARDIAN_PID" ||
  ! identity_matches "$SUPERVISOR_TMP/guardian-identity" ||
  ! group_anchor_matches "$SUPERVISOR_TMP/hold-identity"; then
  echo "detached lifetime guardian could not be armed" >&2
  finish_main 70
fi
IFS= read -r MAIN_HOLD_IDENTITY <"$SUPERVISOR_TMP/hold-identity" || finish_main 70

exec 3<"$EVENT_PIPE"
pause_hook "${OCTOSTORE_SUPERVISOR_TEST_PRE_READ_DIR:-}" || finish_main 70

while IFS= read -r event_line <&3; do
  event_fields=$(printf '%s\n' "$event_line" | jq -er '
    if .schema_version != 1 then error("unsupported schema_version") else . end |
    if (.authority_remaining_ms != null) and
       (((.authority_remaining_ms | type) != "number") or (.authority_remaining_ms != (.authority_remaining_ms | floor)))
    then error("authority_remaining_ms must be an integer") else . end |
    if (.authority_observed_unix_ms != null) and
       (((.authority_observed_unix_ms | type) != "number") or (.authority_observed_unix_ms != (.authority_observed_unix_ms | floor)))
    then error("authority_observed_unix_ms must be an integer") else . end |
    if (.authority_observed_continuous_ms != null) and
       (((.authority_observed_continuous_ms | type) != "number") or (.authority_observed_continuous_ms != (.authority_observed_continuous_ms | floor)))
    then error("authority_observed_continuous_ms must be an integer") else . end |
    [.event, .kind, .name, (.candidate_id // ""), .sequence, (.term // ""), (.authority_remaining_ms // ""), (.authority_observed_unix_ms // ""), (.authority_observed_continuous_ms // "")] | @tsv
  ') || {
    echo "malformed or unsupported hold event; refusing to continue work" >&2
    finish_main 70
  }
  event_name=$(printf '%s\n' "$event_fields" | cut -f1)
  event_kind=$(printf '%s\n' "$event_fields" | cut -f2)
  coordination_name=$(printf '%s\n' "$event_fields" | cut -f3)
  candidate_id=$(printf '%s\n' "$event_fields" | cut -f4)
  sequence=$(printf '%s\n' "$event_fields" | cut -f5)
  event_term=$(printf '%s\n' "$event_fields" | cut -f6)
  authority_remaining_ms=$(printf '%s\n' "$event_fields" | cut -f7)
  authority_observed_unix_ms=$(printf '%s\n' "$event_fields" | cut -f8)
  authority_observed_continuous_ms=$(printf '%s\n' "$event_fields" | cut -f9)
  case "$sequence" in *[!0-9]*|'') echo "hold event has an invalid sequence" >&2; finish_main 70 ;; esac
  [ "$sequence" -gt "$LAST_SEQUENCE" ] || {
    echo "hold event sequence did not increase" >&2
    finish_main 70
  }
  LAST_SEQUENCE=$sequence
  [ "$event_kind" = "$EXPECTED_KIND" ] && [ "$coordination_name" = "$EXPECTED_NAME" ] || {
    echo "hold event does not match the expected coordination target" >&2
    finish_main 70
  }
  if [ "$EXPECTED_KIND" = election ]; then
    [ "$candidate_id" = "$EXPECTED_CANDIDATE" ] || {
      echo "hold event does not match the expected election candidate" >&2
      finish_main 70
    }
  else
    [ -z "$candidate_id" ] || {
      echo "lock hold event unexpectedly contains a candidate identity" >&2
      finish_main 70
    }
  fi

  case "$event_name" in
    waiting)
      [ -z "$WORKER_PID" ] || {
        echo "hold returned to waiting after acquisition; stopping worker" >&2
        finish_main 20
      }
      # Waiting is safe to report immediately: no authority or worker launch
      # is implied by this event.
      emit_lifecycle_event
      ;;
    leader|acquired)
      [ -z "$WORKER_PID" ] || { echo "duplicate authority-start event" >&2; finish_main 70; }
      case "$event_term" in *[!0-9]*|'') echo "authority event is missing a numeric fencing term" >&2; finish_main 70 ;; esac
      [ "$event_term" -gt 0 ] || { echo "authority term must be positive" >&2; finish_main 70; }
      case "$authority_remaining_ms" in *[!0-9]*|'') echo "authority event is missing a remaining safety budget" >&2; finish_main 70 ;; esac
      case "$authority_observed_unix_ms" in *[!0-9]*|'') echo "authority event is missing an emission time" >&2; finish_main 70 ;; esac
      case "$authority_observed_continuous_ms" in *[!0-9]*|'') echo "authority event is missing a continuous-clock emission time" >&2; finish_main 70 ;; esac
      initial_fresh_budget=$(fresh_authority_budget "$authority_remaining_ms" "$authority_observed_unix_ms" "$authority_observed_continuous_ms") || {
        echo "authority event arrived without a fresh safety budget" >&2
        finish_main 20
      }
      monitor_delay_ms "$initial_fresh_budget" >/dev/null || {
        echo "authority event arrived without a usable remaining safety budget" >&2
        finish_main 20
      }
      ACTIVE_TERM=$event_term
      start_worker "$ACTIVE_TERM" "$authority_remaining_ms" "$authority_observed_unix_ms" "$authority_observed_continuous_ms" || {
        echo "authority could not be armed before worker start" >&2
        finish_main 20
      }
      # Report authority only after the guardian has acknowledged the deadline
      # and the protected worker has acknowledged its gated launch.
      emit_lifecycle_event
      ;;
    renewed)
      [ -n "$ACTIVE_TERM" ] && [ "$event_term" = "$ACTIVE_TERM" ] || {
        echo "renewed event changed or omitted the fencing term" >&2
        finish_main 70
      }
      group_alive_with_identity "$SUPERVISOR_TMP/worker-identity" || {
        echo "worker exited while authority remained live" >&2
        finish_main 70
      }
      arm_authority "$authority_remaining_ms" "$authority_observed_unix_ms" "$authority_observed_continuous_ms" || {
        echo "renewed authority could not replace the active deadline" >&2
        finish_main 20
      }
      emit_lifecycle_event
      ;;
    released)
      [ -z "$ACTIVE_TERM" ] || [ "$event_term" = "$ACTIVE_TERM" ] || {
        echo "released event changed the fencing term" >&2
        finish_main 70
      }
      finish_terminal_event 0
      ;;
    lost|uncertain)
      [ -z "$ACTIVE_TERM" ] || [ "$event_term" = "$ACTIVE_TERM" ] || {
        echo "$event_name event changed the fencing term" >&2
        finish_main 70
      }
      finish_terminal_event 20
      ;;
    error)
      finish_terminal_event 70
      ;;
    *)
      echo "unknown hold event '$event_name'; refusing to continue work" >&2
      finish_main 70
      ;;
  esac
done
exec 3<&-

unexpected_status=70
if [ -r "$SUPERVISOR_TMP/guardian-result" ]; then
  IFS= read -r guardian_result <"$SUPERVISOR_TMP/guardian-result" || guardian_result=
  case "$guardian_result" in
    authority_expired|supervisor_dead) unexpected_status=20 ;;
    worker_exited|worker_group_escape|invalid_authority_state|clock_failed) unexpected_status=70 ;;
    hold_exited)
      if [ -r "$SUPERVISOR_TMP/hold-status" ]; then
        IFS= read -r unexpected_status <"$SUPERVISOR_TMP/hold-status" || unexpected_status=70
      fi
      ;;
  esac
fi
finish_main "$unexpected_status"
