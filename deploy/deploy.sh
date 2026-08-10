#!/usr/bin/env bash
# deploy/deploy.sh — called by GitHub Actions on the production host.
# Usage: ./deploy/deploy.sh <tag> <expected-commit-sha> <attested-target-sha256> <attested-previous-sha256>
set -Eeuo pipefail

TAG="${1:?Usage: $0 <tag> <expected-commit-sha> <attested-target-sha256> <attested-previous-sha256>}"
EXPECTED_COMMIT="${2:?Usage: $0 <tag> <expected-commit-sha> <attested-target-sha256> <attested-previous-sha256>}"
EXPECTED_ASSET_SHA="${3:?Usage: $0 <tag> <expected-commit-sha> <attested-target-sha256> <attested-previous-sha256>}"
EXPECTED_PREVIOUS_ASSET_SHA="${4:?Usage: $0 <tag> <expected-commit-sha> <attested-target-sha256> <attested-previous-sha256>}"
EXPECTED_VERSION="${TAG#v}"

PRODUCTION_BINARY=/opt/octostore-lock/octostore
PRODUCTION_DEPLOY_ROOT=/opt/octostore-lock
PRODUCTION_API_ORIGIN=https://api.octostore.io
PRODUCTION_SITE_ORIGIN=https://octostore.io
PRODUCTION_INSTALLER_ORIGIN=https://raw.githubusercontent.com/octostore/octostore.io
PRODUCTION_RELEASE_BASE=https://github.com/octostore/octostore.io/releases/download
PRODUCTION_INSTALL_RELEASE_BASE=https://github.com/octostore/octostore.io/releases
PRODUCTION_RELEASE_METADATA_BASE=https://api.github.com/repos/octostore/octostore.io/releases/tags

BINARY=$PRODUCTION_BINARY
DEPLOY_ROOT=$PRODUCTION_DEPLOY_ROOT
API_ORIGIN=$PRODUCTION_API_ORIGIN
SITE_ORIGIN=$PRODUCTION_SITE_ORIGIN
INSTALLER_ORIGIN=$PRODUCTION_INSTALLER_ORIGIN
RELEASE_BASE=$PRODUCTION_RELEASE_BASE
INSTALL_RELEASE_BASE=$PRODUCTION_INSTALL_RELEASE_BASE
RELEASE_METADATA_BASE=$PRODUCTION_RELEASE_METADATA_BASE
VERIFY_PARENT=/tmp
SERVICE=octostore
SERVICE_PORT=3030
ASSET=octostore-linux-amd64
VERIFY_ATTEMPTS=15
RETRY_DELAY=2
SKIP_CANARY=false
FAILPOINT=""
FAIL_MODE=error
TEST_MODE=false
CANARY_FIXTURE=""

configure_environment() {
  local forbidden
  case "${OCTOSTORE_DEPLOY_TEST_MODE:-}" in
    "")
      for forbidden in \
        OCTOSTORE_DEPLOY_TEST_ROOT \
        OCTOSTORE_DEPLOY_ROOT \
        OCTOSTORE_DEPLOY_BINARY \
        OCTOSTORE_DEPLOY_API_ORIGIN \
        OCTOSTORE_DEPLOY_SITE_ORIGIN \
        OCTOSTORE_DEPLOY_INSTALLER_ORIGIN \
        OCTOSTORE_DEPLOY_RELEASE_BASE \
        OCTOSTORE_DEPLOY_INSTALL_RELEASE_BASE \
        OCTOSTORE_DEPLOY_RELEASE_METADATA_BASE \
        OCTOSTORE_DEPLOY_VERIFY_PARENT \
        OCTOSTORE_DEPLOY_VERIFY_ATTEMPTS \
        OCTOSTORE_DEPLOY_RETRY_DELAY \
        OCTOSTORE_DEPLOY_SKIP_CANARY \
        OCTOSTORE_DEPLOY_FAILPOINT \
        OCTOSTORE_DEPLOY_FAIL_MODE \
        OCTOSTORE_DEPLOY_CANARY_FIXTURE; do
        if [[ -n ${!forbidden+x} ]]; then
          echo "ERROR: $forbidden is fixture-only and cannot override production" >&2
          return 64
        fi
      done
      ;;
    fixture-only)
      TEST_MODE=true
      : "${OCTOSTORE_DEPLOY_TEST_ROOT:?fixture mode requires OCTOSTORE_DEPLOY_TEST_ROOT}"
      : "${OCTOSTORE_DEPLOY_ROOT:?fixture mode requires OCTOSTORE_DEPLOY_ROOT}"
      : "${OCTOSTORE_DEPLOY_BINARY:?fixture mode requires OCTOSTORE_DEPLOY_BINARY}"
      : "${OCTOSTORE_DEPLOY_API_ORIGIN:?fixture mode requires OCTOSTORE_DEPLOY_API_ORIGIN}"
      : "${OCTOSTORE_DEPLOY_SITE_ORIGIN:?fixture mode requires OCTOSTORE_DEPLOY_SITE_ORIGIN}"
      : "${OCTOSTORE_DEPLOY_INSTALLER_ORIGIN:?fixture mode requires OCTOSTORE_DEPLOY_INSTALLER_ORIGIN}"
      : "${OCTOSTORE_DEPLOY_RELEASE_BASE:?fixture mode requires OCTOSTORE_DEPLOY_RELEASE_BASE}"
      : "${OCTOSTORE_DEPLOY_INSTALL_RELEASE_BASE:?fixture mode requires OCTOSTORE_DEPLOY_INSTALL_RELEASE_BASE}"
      : "${OCTOSTORE_DEPLOY_RELEASE_METADATA_BASE:?fixture mode requires OCTOSTORE_DEPLOY_RELEASE_METADATA_BASE}"
      : "${OCTOSTORE_DEPLOY_VERIFY_PARENT:?fixture mode requires OCTOSTORE_DEPLOY_VERIFY_PARENT}"

      DEPLOY_ROOT=$OCTOSTORE_DEPLOY_ROOT
      BINARY=$OCTOSTORE_DEPLOY_BINARY
      API_ORIGIN=$OCTOSTORE_DEPLOY_API_ORIGIN
      SITE_ORIGIN=$OCTOSTORE_DEPLOY_SITE_ORIGIN
      INSTALLER_ORIGIN=$OCTOSTORE_DEPLOY_INSTALLER_ORIGIN
      RELEASE_BASE=$OCTOSTORE_DEPLOY_RELEASE_BASE
      INSTALL_RELEASE_BASE=$OCTOSTORE_DEPLOY_INSTALL_RELEASE_BASE
      RELEASE_METADATA_BASE=$OCTOSTORE_DEPLOY_RELEASE_METADATA_BASE
      VERIFY_PARENT=$OCTOSTORE_DEPLOY_VERIFY_PARENT
      VERIFY_ATTEMPTS=${OCTOSTORE_DEPLOY_VERIFY_ATTEMPTS:-1}
      RETRY_DELAY=${OCTOSTORE_DEPLOY_RETRY_DELAY:-0}
      SKIP_CANARY=${OCTOSTORE_DEPLOY_SKIP_CANARY:-false}
      FAILPOINT=${OCTOSTORE_DEPLOY_FAILPOINT:-}
      FAIL_MODE=${OCTOSTORE_DEPLOY_FAIL_MODE:-error}
      CANARY_FIXTURE=${OCTOSTORE_DEPLOY_CANARY_FIXTURE:-}

      case "$DEPLOY_ROOT/" in
        "$OCTOSTORE_DEPLOY_TEST_ROOT"/*) ;;
        *) echo "ERROR: fixture deploy root must be below OCTOSTORE_DEPLOY_TEST_ROOT" >&2; return 64 ;;
      esac
      case "$BINARY" in
        "$DEPLOY_ROOT"/*) ;;
        *) echo "ERROR: fixture binary must be below the fixture deploy root" >&2; return 64 ;;
      esac
      case "$VERIFY_PARENT/" in
        "$OCTOSTORE_DEPLOY_TEST_ROOT"/*) ;;
        *) echo "ERROR: fixture verify parent must be below OCTOSTORE_DEPLOY_TEST_ROOT" >&2; return 64 ;;
      esac
      if [[ "$DEPLOY_ROOT" == "$PRODUCTION_DEPLOY_ROOT" \
        || "$BINARY" == "$PRODUCTION_BINARY" \
        || "$API_ORIGIN" == "$PRODUCTION_API_ORIGIN" \
        || "$SITE_ORIGIN" == "$PRODUCTION_SITE_ORIGIN" \
        || "$INSTALLER_ORIGIN" == "$PRODUCTION_INSTALLER_ORIGIN" \
        || "$RELEASE_BASE" == "$PRODUCTION_RELEASE_BASE" \
        || "$INSTALL_RELEASE_BASE" == "$PRODUCTION_INSTALL_RELEASE_BASE" ]]; then
        echo "ERROR: fixture mode refuses every production deployment constant" >&2
        return 64
      fi
      if [[ "$API_ORIGIN" != https://*.invalid \
        || "$SITE_ORIGIN" != https://*.invalid \
        || "$INSTALLER_ORIGIN" != https://*.invalid \
        || "$RELEASE_BASE" != https://*.invalid/* \
        || "$INSTALL_RELEASE_BASE" != https://*.invalid/* ]]; then
        echo "ERROR: fixture network origins must use reserved .invalid hosts" >&2
        return 64
      fi
      if [[ ! "$VERIFY_ATTEMPTS" =~ ^[1-9][0-9]*$ || ! "$RETRY_DELAY" =~ ^[0-9]+$ ]]; then
        echo "ERROR: fixture retry controls must be non-negative integers" >&2
        return 64
      fi
      case "$SKIP_CANARY" in true|false) ;; *) echo "ERROR: invalid fixture canary setting" >&2; return 64 ;; esac
      if [[ "$SKIP_CANARY" == false ]]; then
        case "$CANARY_FIXTURE" in
          "$OCTOSTORE_DEPLOY_TEST_ROOT"/*) ;;
          *) echo "ERROR: fixture canary must be an executable below OCTOSTORE_DEPLOY_TEST_ROOT" >&2; return 64 ;;
        esac
        if [[ ! -x "$CANARY_FIXTURE" ]]; then
          echo "ERROR: fixture canary is not executable" >&2
          return 64
        fi
      fi
      case "$FAILPOINT" in ""|after_checkout|after_service_stop|after_binary_swap) ;;
        *) echo "ERROR: invalid fixture failure point '$FAILPOINT'" >&2; return 64 ;;
      esac
      case "$FAIL_MODE" in error|INT|TERM|HUP|KILL) ;;
        *) echo "ERROR: invalid fixture failure mode '$FAIL_MODE'" >&2; return 64 ;;
      esac
      if [[ -z "$FAILPOINT" && "$FAIL_MODE" != error ]]; then
        echo "ERROR: fixture failure mode requires a failure point" >&2
        return 64
      fi
      ;;
    *)
      echo "ERROR: OCTOSTORE_DEPLOY_TEST_MODE must be unset in production" >&2
      return 64
      ;;
  esac
}

configure_environment

for required in awk chmod cmp cp curl find git grep install jq mkdir mktemp mv perl rm rmdir sed seq sh sha256sum sleep sort stat tr uname; do
  command -v "$required" >/dev/null 2>&1 || {
    echo "ERROR: deploy host is missing required command: $required" >&2
    exit 64
  }
done
if [[ "$TEST_MODE" == false ]]; then
  for required in fuser journalctl sudo systemctl; do
    command -v "$required" >/dev/null 2>&1 || {
      echo "ERROR: production deploy host is missing required command: $required" >&2
      exit 64
    }
  done
fi
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "ERROR: expected a stable semver tag, got '$TAG'" >&2
  exit 64
fi
if [[ ! "$EXPECTED_COMMIT" =~ ^[0-9a-f]{40}$ ]]; then
  echo "ERROR: expected a 40-character lowercase commit SHA" >&2
  exit 64
fi
if [[ ! "$EXPECTED_ASSET_SHA" =~ ^[0-9a-f]{64}$ ]]; then
  echo "ERROR: expected a 64-character lowercase attested asset SHA-256" >&2
  exit 64
fi
if [[ ! "$EXPECTED_PREVIOUS_ASSET_SHA" =~ ^[0-9a-f]{64}$ ]]; then
  echo "ERROR: expected a 64-character lowercase attested previous SHA-256" >&2
  exit 64
fi
if ! git -C "$DEPLOY_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
  || [[ ! -d "$DEPLOY_ROOT/site" ]]; then
  echo "ERROR: deploy root is not an OctoStore checkout with a site directory" >&2
  exit 64
fi
if [[ ! -d "$VERIFY_PARENT" ]]; then
  echo "ERROR: verification parent directory does not exist" >&2
  exit 64
fi

VERIFY_DIR=$(mktemp -d "$VERIFY_PARENT/octostore-deploy.XXXXXX")
TMP_BINARY="$VERIFY_DIR/$ASSET"
LIVE_BINARY="$VERIFY_DIR/live-$ASSET"
STATE_FILE="$DEPLOY_ROOT/.octostore-deploy-state"
LOCK_DIR="$DEPLOY_ROOT/.octostore-deploy-lock"
BACKUP_BINARY="${BINARY}.known-good"
BACKUP_STAGING="${BACKUP_BINARY}.tmp.$$"
CANDIDATE_BINARY="${BINARY}.candidate.$$"
ROLLBACK_BINARY="${BINARY}.rollback.$$"
LOCK_HELD=false
HANDLING_FAILURE=false
DEPLOY_COMPLETE=false
PREVIOUS_COMMIT=""
PREVIOUS_VERSION=""
PREVIOUS_BINARY_SHA=""
STATE_PHASE=""
STATE_PREVIOUS_COMMIT=""
STATE_PREVIOUS_VERSION=""
STATE_PREVIOUS_BINARY_SHA=""
STATE_TARGET_COMMIT=""
STATE_TARGET_VERSION=""

run_systemctl() {
  if [[ "$TEST_MODE" == true ]]; then
    systemctl "$@"
  else
    sudo systemctl "$@"
  fi
}

show_service_logs() {
  if [[ "$TEST_MODE" == true ]]; then
    journalctl -u "$SERVICE" -n 20 --no-pager >&2 || true
  else
    sudo journalctl -u "$SERVICE" -n 20 --no-pager >&2 || true
  fi
}

clear_service_listener() {
  if [[ "$TEST_MODE" == true ]]; then
    fuser -k "$SERVICE_PORT/tcp" 2>/dev/null || true
  else
    sudo fuser -k "$SERVICE_PORT/tcp" 2>/dev/null || true
  fi
}

cleanup() {
  local status=$?
  rm -f "$BACKUP_STAGING" "$CANDIDATE_BINARY" "$ROLLBACK_BINARY" "${STATE_FILE}.tmp.$$"
  if [[ -n ${VERIFY_DIR:-} && -d "$VERIFY_DIR" ]]; then
    rm -rf "$VERIFY_DIR"
  fi
  if [[ "$LOCK_HELD" == true ]]; then
    rm -f "$LOCK_DIR/pid"
    rmdir "$LOCK_DIR" 2>/dev/null || true
  fi
  return "$status"
}
trap cleanup EXIT

acquire_deploy_lock() {
  local owner=""
  local attempt
  for attempt in 1 2 3; do
    if mkdir "$LOCK_DIR" 2>/dev/null; then
      printf '%s\n' "$$" >"$LOCK_DIR/pid"
      LOCK_HELD=true
      return 0
    fi
    if [[ -r "$LOCK_DIR/pid" ]]; then
      owner=$(sed -n '1p' "$LOCK_DIR/pid")
    fi
    if [[ "$owner" =~ ^[1-9][0-9]*$ ]] && kill -0 "$owner" 2>/dev/null; then
      echo "ERROR: deployment already running as process $owner" >&2
      return 75
    fi
    rm -f "$LOCK_DIR/pid"
    rmdir "$LOCK_DIR" 2>/dev/null || true
  done
  echo "ERROR: could not acquire the deployment lock" >&2
  return 75
}

write_state() {
  local phase=$1
  local temporary="${STATE_FILE}.tmp.$$"
  (
    umask 077
    {
      printf 'phase=%s\n' "$phase"
      printf 'previous_commit=%s\n' "$PREVIOUS_COMMIT"
      printf 'previous_version=%s\n' "$PREVIOUS_VERSION"
      printf 'previous_binary_sha=%s\n' "$PREVIOUS_BINARY_SHA"
      printf 'target_commit=%s\n' "$EXPECTED_COMMIT"
      printf 'target_version=%s\n' "$EXPECTED_VERSION"
    } >"$temporary"
  )
  mv "$temporary" "$STATE_FILE"
  STATE_PHASE=$phase
}

load_state() {
  local key value
  STATE_PHASE=""
  STATE_PREVIOUS_COMMIT=""
  STATE_PREVIOUS_VERSION=""
  STATE_PREVIOUS_BINARY_SHA=""
  STATE_TARGET_COMMIT=""
  STATE_TARGET_VERSION=""
  while IFS='=' read -r key value; do
    case "$key" in
      phase) STATE_PHASE=$value ;;
      previous_commit) STATE_PREVIOUS_COMMIT=$value ;;
      previous_version) STATE_PREVIOUS_VERSION=$value ;;
      previous_binary_sha) STATE_PREVIOUS_BINARY_SHA=$value ;;
      target_commit) STATE_TARGET_COMMIT=$value ;;
      target_version) STATE_TARGET_VERSION=$value ;;
      *) echo "ERROR: invalid key in deployment state: $key" >&2; return 1 ;;
    esac
  done <"$STATE_FILE"
  case "$STATE_PHASE" in prepared|checkout_changed|service_stopped|binary_swapped|service_started|proving) ;;
    *) echo "ERROR: invalid deployment phase '$STATE_PHASE'" >&2; return 1 ;;
  esac
  if [[ ! "$STATE_PREVIOUS_COMMIT" =~ ^[0-9a-f]{40}$ \
    || ! "$STATE_PREVIOUS_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
    || ! "$STATE_PREVIOUS_BINARY_SHA" =~ ^[0-9a-f]{64}$ \
    || ! "$STATE_TARGET_COMMIT" =~ ^[0-9a-f]{40}$ \
    || ! "$STATE_TARGET_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "ERROR: deployment state contains invalid commit or version data" >&2
    return 1
  fi
  if [[ ! -x "$BACKUP_BINARY" ]]; then
    echo "ERROR: deployment state exists without an executable known-good binary" >&2
    return 1
  fi
  if [[ $(sha256sum "$BACKUP_BINARY" | awk '{print $1}') != "$STATE_PREVIOUS_BINARY_SHA" ]]; then
    echo "ERROR: known-good binary does not match the hash recorded in deployment state" >&2
    return 1
  fi
}

site_url_for_relative_path() {
  local relative=$1
  case "$relative" in
    index.html) printf '%s/\n' "$SITE_ORIGIN" ;;
    */index.html) printf '%s/%s\n' "$SITE_ORIGIN" "${relative%index.html}" ;;
    *) printf '%s/%s\n' "$SITE_ORIGIN" "$relative" ;;
  esac
}

file_mode() {
  local path=$1
  if stat -c '%a' "$path" >/dev/null 2>&1; then
    stat -c '%a' "$path"
  else
    stat -f '%Lp' "$path"
  fi
}

set_safe_site_modes() {
  find "$DEPLOY_ROOT/site" -type d -exec chmod 0755 {} +
  find "$DEPLOY_ROOT/site" -type f -exec chmod 0644 {} +
}

verify_safe_site_modes() {
  local path mode
  while IFS= read -r -d '' path; do
    mode=$(file_mode "$path")
    if [[ "$mode" != 755 ]]; then
      echo "ERROR: served site directory is not mode 0755: $path ($mode)" >&2
      return 1
    fi
  done < <(find "$DEPLOY_ROOT/site" -type d -print0)
  while IFS= read -r -d '' path; do
    mode=$(file_mode "$path")
    if [[ "$mode" != 644 ]]; then
      echo "ERROR: served site file is not mode 0644: $path ($mode)" >&2
      return 1
    fi
  done < <(find "$DEPLOY_ROOT/site" -type f -print0)
}

verify_exact_site_file() {
  local url=$1
  local expected_file=$2
  local received_file=$3
  local expected_commit=$4
  local attempt
  for ((attempt = 1; attempt <= VERIFY_ATTEMPTS; attempt++)); do
    if curl -fsSL --max-time 5 "$url" -o "$received_file" \
      && cmp -s "$expected_file" "$received_file"; then
      return 0
    fi
    sleep "$RETRY_DELAY"
  done
  echo "ERROR: live content at $url does not exactly match commit $expected_commit" >&2
  return 1
}

verify_exact_site_tree() {
  local commit=$1
  local evidence_name=$2
  local expected_list="$VERIFY_DIR/site-expected-$evidence_name"
  local actual_list="$VERIFY_DIR/site-actual-$evidence_name"
  local relative url received count=0

  git -c core.quotePath=false ls-tree -r --name-only "$commit" -- site \
    | sed -n 's#^site/##p' \
    | LC_ALL=C sort >"$expected_list"
  (
    cd "$DEPLOY_ROOT/site"
    find . -type f -print | sed 's#^\./##' | LC_ALL=C sort
  ) >"$actual_list"
  if find "$DEPLOY_ROOT/site" -type l -print | sed -n '1p' | grep -q .; then
    echo "ERROR: served site tree contains a symlink, which is outside the exact-file manifest" >&2
    return 1
  fi
  verify_safe_site_modes || return 1
  if [[ ! -s "$expected_list" ]] || ! cmp -s "$expected_list" "$actual_list"; then
    echo "ERROR: served site files do not exactly match the Git manifest for $commit" >&2
    return 1
  fi

  while IFS= read -r relative; do
    if [[ ! "$relative" =~ ^[A-Za-z0-9._/-]+$ || ! -f "$DEPLOY_ROOT/site/$relative" ]]; then
      echo "ERROR: unsupported or missing served site path '$relative'" >&2
      return 1
    fi
    url=$(site_url_for_relative_path "$relative")
    received="$VERIFY_DIR/site-received-$evidence_name"
    verify_exact_site_file "$url" "$DEPLOY_ROOT/site/$relative" "$received" "$commit" || return 1
    count=$((count + 1))
  done <"$expected_list"
  if ((count == 0)); then
    echo "ERROR: exact site manifest was empty" >&2
    return 1
  fi
  echo "EVIDENCE ${evidence_name}_site_commit=$commit files=$count"
  echo "EVIDENCE ${evidence_name}_site_modes=directories:0755,files:0644"
}

read_api_version() {
  curl -fsS --max-time 3 "$API_ORIGIN/openapi.yaml" 2>/dev/null \
    | awk '$1 == "version:" { print $2; exit }'
}

verify_api_version() {
  local expected=$1
  local evidence_name=$2
  local live_version=""
  local attempt
  for ((attempt = 1; attempt <= VERIFY_ATTEMPTS; attempt++)); do
    live_version=$(read_api_version) || live_version=""
    if [[ "$live_version" == "$expected" ]]; then
      echo "EVIDENCE ${evidence_name}_api_version=$live_version"
      return 0
    fi
    sleep "$RETRY_DELAY"
  done
  echo "ERROR: live API reports '${live_version:-<empty>}', expected '$expected'" >&2
  return 1
}

verify_api_health() {
  local evidence_name=$1
  local health_file="$VERIFY_DIR/health-$evidence_name.json"
  local attempt
  for ((attempt = 1; attempt <= VERIFY_ATTEMPTS; attempt++)); do
    if curl -fsS --max-time 3 "$API_ORIGIN/health" -o "$health_file" \
      && jq -e '
        type == "object"
        and .status == "ok"
        and .storage == "wal"
        and (.db_size_bytes | type == "number")
        and .db_size_bytes >= 0
        and ((.db_size_bytes | floor) == .db_size_bytes)
      ' "$health_file" >/dev/null; then
      echo "EVIDENCE ${evidence_name}_api_health=status:ok,storage:wal"
      return 0
    fi
    sleep "$RETRY_DELAY"
  done
  echo "ERROR: live API health response does not match the documented contract" >&2
  return 1
}

verify_public_installer() {
  local commit=$1
  local version=$2
  local evidence_name=$3
  local expected_file="$VERIFY_DIR/installer-expected-$evidence_name.sh"
  local received_file="$VERIFY_DIR/installer-received-$evidence_name.sh"
  local url="$INSTALLER_ORIGIN/v$version/install.sh"
  local installer_sha attempt

  if ! git show "$commit:install.sh" >"$expected_file"; then
    echo "ERROR: commit $commit does not contain the public install.sh" >&2
    return 1
  fi
  for ((attempt = 1; attempt <= VERIFY_ATTEMPTS; attempt++)); do
    if curl -fsSL --max-time 5 "$url" -o "$received_file" \
      && cmp -s "$expected_file" "$received_file"; then
      installer_sha=$(sha256sum "$expected_file" | awk '{print $1}')
      echo "EVIDENCE ${evidence_name}_installer_commit=$commit version=$version checksum=$installer_sha"
      return 0
    fi
    sleep "$RETRY_DELAY"
  done
  echo "ERROR: public installer at $url does not exactly match install.sh from commit $commit" >&2
  return 1
}

wait_for_service_active() {
  local attempt state=""
  for ((attempt = 1; attempt <= VERIFY_ATTEMPTS; attempt++)); do
    state=$(run_systemctl is-active "$SERVICE" 2>/dev/null) || true
    [[ "$state" == active ]] && return 0
    [[ "$state" == failed ]] && break
    sleep "$RETRY_DELAY"
  done
  echo "ERROR: service did not become active (state=${state:-unknown})" >&2
  show_service_logs
  return 1
}

rollback_deployment() {
  local reason=$1
  local rollback_failed=false
  local service_active=false
  local binary_restored=false
  local can_restore_binary=true

  echo "ERROR: $reason; beginning checked rollback" >&2
  if ! load_state; then
    echo "ROLLBACK_FAILED: cannot load durable deployment state; preserving state and backup" >&2
    return 2
  fi

  if ! git checkout --detach --force "$STATE_PREVIOUS_COMMIT"; then
    echo "ROLLBACK_FAILED: could not restore checkout $STATE_PREVIOUS_COMMIT" >&2
    rollback_failed=true
  elif [[ $(git rev-parse HEAD 2>/dev/null || true) != "$STATE_PREVIOUS_COMMIT" ]]; then
    echo "ROLLBACK_FAILED: checkout did not land on $STATE_PREVIOUS_COMMIT" >&2
    rollback_failed=true
  elif ! set_safe_site_modes; then
    echo "ROLLBACK_FAILED: could not restore safe served-site modes" >&2
    rollback_failed=true
  fi

  if cmp -s "$BACKUP_BINARY" "$BINARY"; then
    binary_restored=true
  else
    if run_systemctl is-active "$SERVICE" >/dev/null 2>&1; then
      if ! run_systemctl stop "$SERVICE"; then
        echo "ROLLBACK_FAILED: could not stop service before restoring binary" >&2
        rollback_failed=true
        can_restore_binary=false
      fi
    fi
    if [[ "$can_restore_binary" == true ]]; then
      if cp "$BACKUP_BINARY" "$ROLLBACK_BINARY" \
        && chmod 0755 "$ROLLBACK_BINARY" \
        && mv "$ROLLBACK_BINARY" "$BINARY" \
        && cmp -s "$BACKUP_BINARY" "$BINARY"; then
        binary_restored=true
      else
        echo "ROLLBACK_FAILED: could not atomically restore the known-good binary" >&2
        rollback_failed=true
      fi
    fi
  fi

  if run_systemctl is-active "$SERVICE" >/dev/null 2>&1; then
    service_active=true
  elif [[ "$binary_restored" == true ]] && run_systemctl start "$SERVICE"; then
    if wait_for_service_active; then
      service_active=true
    else
      rollback_failed=true
    fi
  else
    echo "ROLLBACK_FAILED: could not restart the known-good service" >&2
    rollback_failed=true
  fi

  if [[ "$binary_restored" != true || "$service_active" != true ]]; then
    rollback_failed=true
  fi
  if [[ "$rollback_failed" == false ]] \
    && ! verify_api_version "$STATE_PREVIOUS_VERSION" rollback; then
    rollback_failed=true
  fi
  if [[ "$rollback_failed" == false ]] \
    && ! verify_api_health rollback; then
    rollback_failed=true
  fi
  if [[ "$rollback_failed" == false ]] \
    && ! verify_exact_site_tree "$STATE_PREVIOUS_COMMIT" rollback; then
    rollback_failed=true
  fi
  if [[ "$rollback_failed" == false ]] \
    && ! verify_public_installer \
      "$STATE_PREVIOUS_COMMIT" "$STATE_PREVIOUS_VERSION" rollback; then
    rollback_failed=true
  fi

  if [[ "$rollback_failed" == true ]]; then
    echo "ROLLBACK_FAILED: recovery is incomplete; state=$STATE_FILE backup=$BACKUP_BINARY" >&2
    return 2
  fi
  rm -f "$STATE_FILE"
  echo "EVIDENCE rollback_complete commit=$STATE_PREVIOUS_COMMIT version=$STATE_PREVIOUS_VERSION"
}

handle_failure() {
  local original_status=$1
  local cause=$2
  local rollback_status=0
  if [[ "$HANDLING_FAILURE" == true ]]; then
    exit 2
  fi
  HANDLING_FAILURE=true
  trap - ERR INT TERM HUP
  set +e
  echo "ERROR: deployment interrupted by $cause (status=$original_status phase=${STATE_PHASE:-none})" >&2
  if [[ -f "$STATE_FILE" && "$DEPLOY_COMPLETE" == false ]]; then
    rollback_deployment "deployment interrupted by $cause"
    rollback_status=$?
  fi
  if ((rollback_status != 0)); then
    exit 2
  fi
  exit "$original_status"
}

trap 'handle_failure $? "ERR at line $LINENO"' ERR
trap 'handle_failure 130 INT' INT
trap 'handle_failure 143 TERM' TERM
trap 'handle_failure 129 HUP' HUP

inject_failure() {
  local point=$1
  [[ "$TEST_MODE" == true && "$FAILPOINT" == "$point" ]] || return 0
  echo "INJECTED_FAILURE point=$point mode=$FAIL_MODE" >&2
  case "$FAIL_MODE" in
    error) return 97 ;;
    INT|TERM|HUP|KILL) kill -s "$FAIL_MODE" "$$" ;;
  esac
}

install_tagged_canary_pair() {
  local installer_expected="$VERIFY_DIR/canary-installer-expected.sh"
  local installer_received="$VERIFY_DIR/canary-installer-received.sh"
  local installer_log="$VERIFY_DIR/canary-installer.log"
  local sums="$VERIFY_DIR/canary-SHA256SUMS"
  local supervisor_source="$VERIFY_DIR/canary-supervisor-source.sh"
  local supervisor_expected binary_actual supervisor_actual

  CANARY_INSTALL_DIR="$VERIFY_DIR/canary-bin"
  CANARY_BIN="$CANARY_INSTALL_DIR/octostore"
  CANARY_SUPERVISOR="$CANARY_INSTALL_DIR/octostore-supervisor"

  git show "$EXPECTED_COMMIT:install.sh" >"$installer_expected"
  git show "$EXPECTED_COMMIT:scripts/reference-supervisor.sh" >"$supervisor_source"
  curl -fsSL --max-time 5 "$INSTALLER_ORIGIN/$TAG/install.sh" -o "$installer_received"
  if ! cmp -s "$installer_expected" "$installer_received"; then
    echo "ERROR: supervised canary installer does not match commit $EXPECTED_COMMIT" >&2
    return 1
  fi

  OCTOSTORE_VERSION="$TAG" \
  OCTOSTORE_DOWNLOAD_BASE="$INSTALL_RELEASE_BASE" \
  OCTOSTORE_RELEASE_METADATA_URL="$RELEASE_METADATA_BASE/$TAG" \
  OCTOSTORE_INSTALL_DIR="$CANARY_INSTALL_DIR" \
    sh "$installer_received" >"$installer_log"

  [[ -x "$CANARY_BIN" && -x "$CANARY_SUPERVISOR" ]]
  [[ $("$CANARY_BIN" --version) == "octostore $EXPECTED_VERSION" ]]
  sh -n "$CANARY_SUPERVISOR"

  curl -fsSL --max-time 5 "$RELEASE_BASE/$TAG/SHA256SUMS" -o "$sums"
  supervisor_expected=$(awk '$2 == "octostore-reference-supervisor.sh" { print $1 }' "$sums")
  [[ "$supervisor_expected" =~ ^[0-9a-f]{64}$ ]]
  binary_actual=$(sha256sum "$CANARY_BIN" | awk '{print $1}')
  supervisor_actual=$(sha256sum "$CANARY_SUPERVISOR" | awk '{print $1}')
  [[ "$binary_actual" == "$EXPECTED_ASSET_SHA" ]]
  [[ "$supervisor_actual" == "$supervisor_expected" ]]
  if ! cmp -s "$supervisor_source" "$CANARY_SUPERVISOR"; then
    echo "ERROR: installed supervisor does not match commit $EXPECTED_COMMIT" >&2
    return 1
  fi
  echo "EVIDENCE live_installed_pair=version:$EXPECTED_VERSION,cli:$binary_actual,supervisor:$supervisor_actual,source_commit:$EXPECTED_COMMIT"
}

run_supervised_canary() {
  if [[ "$TEST_MODE" == true ]]; then
    "$CANARY_FIXTURE" "$CANARY_BIN" "$CANARY_SUPERVISOR" "$API_ORIGIN"
    echo "EVIDENCE fixture_supervised_canary=passed installed_pair=true"
    return 0
  fi

  local atlas_worker="$VERIFY_DIR/agent-atlas-worker"
  local comet_worker="$VERIFY_DIR/agent-comet-worker"
  local atlas_worker_log="$VERIFY_DIR/agent-atlas-worker.log"
  local comet_worker_log="$VERIFY_DIR/agent-comet-worker.log"
  local demo_dir="$VERIFY_DIR/supervised-demo"
  local demo_log="$VERIFY_DIR/supervised-demo.log"
  local worker_log starts stops worker_pid

  for path in "$VERIFY_DIR" "$DEPLOY_ROOT"; do
    if [[ ! "$path" =~ ^[A-Za-z0-9._/-]+$ ]]; then
      echo "ERROR: supervised canary path cannot be represented safely: $path" >&2
      return 1
    fi
  done
  [[ -x "$DEPLOY_ROOT/tests/fixtures/recording-worker.sh" ]]
  [[ -x "$DEPLOY_ROOT/scripts/two-agent-supervised-demo.sh" ]]

  printf '#!/bin/sh\nOCTOSTORE_SMOKE_WORKER_LOG=%s\nexport OCTOSTORE_SMOKE_WORKER_LOG\nexec %s\n' \
    "$atlas_worker_log" "$DEPLOY_ROOT/tests/fixtures/recording-worker.sh" >"$atlas_worker"
  printf '#!/bin/sh\nOCTOSTORE_SMOKE_WORKER_LOG=%s\nexport OCTOSTORE_SMOKE_WORKER_LOG\nexec %s\n' \
    "$comet_worker_log" "$DEPLOY_ROOT/tests/fixtures/recording-worker.sh" >"$comet_worker"
  chmod 0700 "$atlas_worker" "$comet_worker"

  OCTOSTORE_URL="$API_ORIGIN" \
  OCTOSTORE_BIN="$CANARY_BIN" \
  OCTOSTORE_SUPERVISOR="$CANARY_SUPERVISOR" \
  OCTOSTORE_ATLAS_WORKER="$atlas_worker" \
  OCTOSTORE_COMET_WORKER="$comet_worker" \
  OCTOSTORE_DEMO_DIR="$demo_dir" \
  OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY=1 \
    "$DEPLOY_ROOT/scripts/two-agent-supervised-demo.sh" >"$demo_log" 2>&1

  for worker_log in "$atlas_worker_log" "$comet_worker_log"; do
    starts=$(grep -Ec '^started [0-9]+ term [1-9][0-9]*$' "$worker_log" || true)
    stops=$(grep -Ec '^stopped [0-9]+$' "$worker_log" || true)
    [[ "$starts" -eq 1 && "$stops" -eq 1 ]]
    worker_pid=$(awk '$1 == "started" { print $2 }' "$worker_log")
    [[ "$worker_pid" =~ ^[1-9][0-9]*$ ]]
    if kill -0 "$worker_pid" 2>/dev/null; then
      echo "ERROR: supervised canary worker $worker_pid remained running" >&2
      return 1
    fi
  done
  if grep -R -E 'leader_token|lease_id|Authorization: Bearer' \
    "$demo_dir" "$demo_log" "$atlas_worker_log" "$comet_worker_log" >/dev/null; then
    echo "ERROR: secret-shaped value appeared in supervised canary evidence" >&2
    return 1
  fi
  grep -Fq 'leads;' "$demo_log"
  grep -Fq 'waits' "$demo_log"
  grep -Fq 'took over after cancellation' "$demo_log"
  grep -Fq 'no remaining supervisor processes' "$demo_log"
  echo "EVIDENCE live_supervised_canary=leader-waiter,cancellation,takeover,workers-stopped,processes-cleared,secret-free"
}

cd "$DEPLOY_ROOT"
acquire_deploy_lock

if [[ -f "$STATE_FILE" ]]; then
  echo "RECOVERY: found an incomplete deployment; rollback must pass before this attempt"
  set +e
  rollback_deployment "recovering an incomplete prior deployment"
  recovery_status=$?
  set -e
  if ((recovery_status != 0)); then
    echo "ERROR: prior deployment could not be recovered; refusing a new deployment" >&2
    exit 2
  fi
fi

echo "=== Deploy $TAG at immutable commit $EXPECTED_COMMIT ==="
git fetch --force origin main "refs/tags/$TAG:refs/tags/$TAG"
TAG_COMMIT=$(git rev-parse "${TAG}^{commit}")
if [[ "$TAG_COMMIT" != "$EXPECTED_COMMIT" ]]; then
  echo "ERROR: tag $TAG resolves to $TAG_COMMIT, expected $EXPECTED_COMMIT" >&2
  exit 1
fi
if ! git merge-base --is-ancestor "$EXPECTED_COMMIT" origin/main; then
  echo "ERROR: release commit $EXPECTED_COMMIT is not on origin/main" >&2
  exit 1
fi

curl -fsSL --max-time 30 "$RELEASE_BASE/$TAG/$ASSET" -o "$TMP_BINARY"
ACTUAL_ASSET_SHA=$(sha256sum "$TMP_BINARY" | awk '{print $1}')
if [[ "$EXPECTED_ASSET_SHA" != "$ACTUAL_ASSET_SHA" ]]; then
  echo "ERROR: asset digest does not match the exact-commit GitHub attestation for $ASSET" >&2
  exit 1
fi
chmod 0755 "$TMP_BINARY"
BINARY_VERSION=$("$TMP_BINARY" --version | awk '{print $2}')
if [[ "$BINARY_VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "ERROR: binary reports '$BINARY_VERSION', expected '$EXPECTED_VERSION'" >&2
  exit 1
fi
echo "EVIDENCE release_asset=$ASSET attested_checksum=$ACTUAL_ASSET_SHA version=$BINARY_VERSION"

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: current production binary is missing or not executable" >&2
  exit 1
fi
PREVIOUS_COMMIT=$(git rev-parse HEAD)
PREVIOUS_VERSION=$("$BINARY" --version | awk '{print $2}')
PREVIOUS_BINARY_SHA=$(sha256sum "$BINARY" | awk '{print $1}')
if [[ ! "$PREVIOUS_COMMIT" =~ ^[0-9a-f]{40}$ \
  || ! "$PREVIOUS_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
  || ! "$PREVIOUS_BINARY_SHA" =~ ^[0-9a-f]{64}$ ]]; then
  echo "ERROR: current deployment cannot be recorded as known-good" >&2
  exit 1
fi
if [[ "$PREVIOUS_BINARY_SHA" != "$EXPECTED_PREVIOUS_ASSET_SHA" ]]; then
  echo "ERROR: current production binary does not match independently attested previous release" >&2
  exit 1
fi
wait_for_service_active
verify_api_version "$PREVIOUS_VERSION" baseline
verify_api_health baseline
verify_exact_site_tree "$PREVIOUS_COMMIT" baseline
verify_public_installer "$PREVIOUS_COMMIT" "$PREVIOUS_VERSION" baseline

cp "$BINARY" "$BACKUP_STAGING"
chmod 0755 "$BACKUP_STAGING"
mv "$BACKUP_STAGING" "$BACKUP_BINARY"
cmp -s "$BINARY" "$BACKUP_BINARY"
[[ $(sha256sum "$BACKUP_BINARY" | awk '{print $1}') == "$PREVIOUS_BINARY_SHA" ]]
write_state prepared
echo "EVIDENCE known_good_saved commit=$PREVIOUS_COMMIT version=$PREVIOUS_VERSION checksum=$PREVIOUS_BINARY_SHA"

git checkout --detach --force "$EXPECTED_COMMIT"
[[ $(git rev-parse HEAD) == "$EXPECTED_COMMIT" ]]
set_safe_site_modes
write_state checkout_changed
inject_failure after_checkout

run_systemctl stop "$SERVICE"
write_state service_stopped
inject_failure after_service_stop
clear_service_listener

cp "$TMP_BINARY" "$CANDIDATE_BINARY"
chmod 0755 "$CANDIDATE_BINARY"
mv "$CANDIDATE_BINARY" "$BINARY"
cmp -s "$TMP_BINARY" "$BINARY"
write_state binary_swapped
inject_failure after_binary_swap

run_systemctl start "$SERVICE"
write_state service_started
wait_for_service_active
write_state proving
verify_api_version "$EXPECTED_VERSION" live
verify_api_health live
verify_exact_site_tree "$EXPECTED_COMMIT" live
verify_public_installer "$EXPECTED_COMMIT" "$EXPECTED_VERSION" live

curl -fsSL --max-time 30 "$RELEASE_BASE/$TAG/$ASSET" -o "$LIVE_BINARY"
LIVE_ASSET_SHA=$(sha256sum "$LIVE_BINARY" | awk '{print $1}')
if [[ "$LIVE_ASSET_SHA" != "$EXPECTED_ASSET_SHA" ]]; then
  echo "ERROR: post-deploy release asset proof failed" >&2
  false
fi
echo "EVIDENCE live_release_asset=$ASSET checksum=$LIVE_ASSET_SHA"

if [[ "$SKIP_CANARY" == false ]]; then
  install_tagged_canary_pair
  run_supervised_canary
else
  echo "EVIDENCE fixture_canary=explicitly_skipped"
fi

rm -f "$STATE_FILE"
DEPLOY_COMPLETE=true
echo "EVIDENCE known_good_retained path=$BACKUP_BINARY version=$PREVIOUS_VERSION checksum=$PREVIOUS_BINARY_SHA"
echo "=== Deploy complete: tag=$TAG commit=$EXPECTED_COMMIT live_version=$EXPECTED_VERSION ==="
