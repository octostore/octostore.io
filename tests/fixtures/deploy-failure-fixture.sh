#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
FIXTURE_ROOT=$(mktemp -d "$ROOT/.deploy-failure-fixture.XXXXXX")
SOURCE_REPO="$FIXTURE_ROOT/source"
REMOTE_REPO="$FIXTURE_ROOT/remote.git"
DEPLOY_ROOT="$FIXTURE_ROOT/deploy"
VERIFY_PARENT="$FIXTURE_ROOT/verify"
MOCK_BIN="$FIXTURE_ROOT/mock-bin"
RELEASE_ROOT="$FIXTURE_ROOT/releases"
SERVICE_STATE="$FIXTURE_ROOT/service-state"
SYSTEMCTL_LOG="$FIXTURE_ROOT/systemctl.log"
CURL_LOG="$FIXTURE_ROOT/curl.log"
DEPLOY_BINARY="$DEPLOY_ROOT/runtime/octostore"
RELEASE_ORIGIN=https://release.fixture.invalid/releases
API_ORIGIN=https://api.fixture.invalid
SITE_ORIGIN=https://site.fixture.invalid
INSTALLER_ORIGIN=https://installer.fixture.invalid
RELEASE_METADATA_ORIGIN=https://metadata.fixture.invalid
TAG=v0.14.0
OFF_MAIN_TAG=v0.14.1

cleanup() {
  rm -rf "$FIXTURE_ROOT"
}
trap cleanup EXIT INT TERM HUP

fail() {
  echo "deployment fixture failed: $*" >&2
  exit 1
}

file_mode() {
  if stat -c '%a' "$1" >/dev/null 2>&1; then
    stat -c '%a' "$1"
  else
    stat -f '%Lp' "$1"
  fi
}

assert_safe_site_modes() {
  local path
  while IFS= read -r -d '' path; do
    [[ $(file_mode "$path") == 755 ]] || fail "site directory mode is not 0755: $path"
  done < <(find "$DEPLOY_ROOT/site" -type d -print0)
  while IFS= read -r -d '' path; do
    [[ $(file_mode "$path") == 644 ]] || fail "site file mode is not 0644: $path"
  done < <(find "$DEPLOY_ROOT/site" -type f -print0)
}

make_binary() {
  local path=$1
  local version=$2
  mkdir -p "$(dirname -- "$path")"
  {
    printf '%s\n' '#!/usr/bin/env bash'
    printf '%s\n' 'set -euo pipefail'
    printf 'printf '\''octostore %s\\n'\''\n' "$version"
  } >"$path"
  chmod 0755 "$path"
}

mkdir -p "$SOURCE_REPO" "$MOCK_BIN" "$RELEASE_ROOT/$TAG" "$VERIFY_PARENT"
git init -q -b main "$SOURCE_REPO"
git -C "$SOURCE_REPO" config user.name "OctoStore deploy fixture"
git -C "$SOURCE_REPO" config user.email "fixture@invalid.example"
mkdir -p "$FIXTURE_ROOT/no-hooks"
git -C "$SOURCE_REPO" config core.hooksPath "$FIXTURE_ROOT/no-hooks"
git -C "$SOURCE_REPO" config commit.gpgsign false
git -C "$SOURCE_REPO" config tag.gpgsign false
mkdir -p "$SOURCE_REPO/site/agents" "$SOURCE_REPO/site/assets" "$SOURCE_REPO/site/docs" \
  "$SOURCE_REPO/scripts"
printf '%s\n' 'old home' >"$SOURCE_REPO/site/index.html"
printf '%s\n' 'old skill' >"$SOURCE_REPO/site/agents/SKILL.md"
printf '%s\n' 'old asset' >"$SOURCE_REPO/site/assets/site.css"
printf '%s\n' 'old docs' >"$SOURCE_REPO/site/docs/index.html"
printf '%s\n' 'old admin' >"$SOURCE_REPO/site/admin.html"
cp "$ROOT/install.sh" "$SOURCE_REPO/install.sh"
cp "$ROOT/scripts/reference-supervisor.sh" "$SOURCE_REPO/scripts/reference-supervisor.sh"
git -C "$SOURCE_REPO" add site install.sh scripts/reference-supervisor.sh
git -C "$SOURCE_REPO" commit -q -m "old deployment"
OLD_COMMIT=$(git -C "$SOURCE_REPO" rev-parse HEAD)

mkdir -p "$SOURCE_REPO/site/blog"
printf '%s\n' 'new home' >"$SOURCE_REPO/site/index.html"
printf '%s\n' 'new skill' >"$SOURCE_REPO/site/agents/SKILL.md"
printf '%s\n' 'new asset' >"$SOURCE_REPO/site/assets/site.css"
printf '%s\n' 'new docs' >"$SOURCE_REPO/site/docs/index.html"
printf '%s\n' 'new admin' >"$SOURCE_REPO/site/admin.html"
printf '%s\n' 'new blog' >"$SOURCE_REPO/site/blog/index.html"
cp "$ROOT/install.sh" "$SOURCE_REPO/install.sh"
cp "$ROOT/scripts/reference-supervisor.sh" "$SOURCE_REPO/scripts/reference-supervisor.sh"
git -C "$SOURCE_REPO" add site install.sh scripts/reference-supervisor.sh
git -C "$SOURCE_REPO" commit -q -m "target deployment"
TARGET_COMMIT=$(git -C "$SOURCE_REPO" rev-parse HEAD)
git -C "$SOURCE_REPO" tag "$TAG"

git -C "$SOURCE_REPO" checkout -q -b off-main-release
printf '%s\n' 'tagged outside main' >"$SOURCE_REPO/off-main-release.txt"
git -C "$SOURCE_REPO" add off-main-release.txt
git -C "$SOURCE_REPO" commit -q -m "off-main release"
OFF_MAIN_COMMIT=$(git -C "$SOURCE_REPO" rev-parse HEAD)
git -C "$SOURCE_REPO" tag "$OFF_MAIN_TAG"
git -C "$SOURCE_REPO" checkout -q main

git clone -q --bare "$SOURCE_REPO" "$REMOTE_REPO"
git clone -q "$REMOTE_REPO" "$DEPLOY_ROOT"
git -C "$DEPLOY_ROOT" checkout -q --detach "$OLD_COMMIT"

make_binary "$DEPLOY_BINARY" 0.13.0
PREVIOUS_ATTESTED_ASSET_SHA=$(sha256sum "$DEPLOY_BINARY" | awk '{print $1}')
make_binary "$RELEASE_ROOT/$TAG/octostore-linux-amd64" 0.14.0
ATTESTED_ASSET_SHA=$(sha256sum "$RELEASE_ROOT/$TAG/octostore-linux-amd64" | awk '{print $1}')
for asset in octostore-linux-arm64 octostore-macos-amd64 octostore-macos-arm64; do
  cp "$RELEASE_ROOT/$TAG/octostore-linux-amd64" "$RELEASE_ROOT/$TAG/$asset"
done
cp "$ROOT/scripts/reference-supervisor.sh" \
  "$RELEASE_ROOT/$TAG/octostore-reference-supervisor.sh"
SUPERVISOR_ASSET_SHA=$(sha256sum \
  "$RELEASE_ROOT/$TAG/octostore-reference-supervisor.sh" | awk '{print $1}')
{
  for asset in \
    octostore-linux-amd64 octostore-linux-arm64 \
    octostore-macos-amd64 octostore-macos-arm64; do
    printf '%s  %s\n' "$ATTESTED_ASSET_SHA" "$asset"
  done
  printf '%s  %s\n' "$SUPERVISOR_ASSET_SHA" octostore-reference-supervisor.sh
} >"$RELEASE_ROOT/$TAG/SHA256SUMS"
printf '%s\n' active >"$SERVICE_STATE"
: >"$SYSTEMCTL_LOG"
: >"$CURL_LOG"

# shellcheck disable=SC2016 # The generated mock expands these variables when it runs.
{
  printf '%s\n' '#!/usr/bin/env bash'
  printf '%s\n' 'set -euo pipefail'
  printf '%s\n' 'printf '\''%s\n'\'' "$*" >>"$MOCK_SYSTEMCTL_LOG"'
  printf '%s\n' 'case "${1:-}" in'
  printf '%s\n' '  stop) printf '\''inactive\n'\'' >"$MOCK_SERVICE_STATE" ;;'
  printf '%s\n' '  start)'
  printf '%s\n' '    if [[ ${MOCK_FAIL_START:-false} == true ]]; then exit 1; fi'
  printf '%s\n' '    [[ -x "$MOCK_DEPLOY_BINARY" ]] || exit 1'
  printf '%s\n' '    printf '\''active\n'\'' >"$MOCK_SERVICE_STATE"'
  printf '%s\n' '    ;;'
  printf '%s\n' '  is-active)'
  printf '%s\n' '    state=$(sed -n '\''1p'\'' "$MOCK_SERVICE_STATE")'
  printf '%s\n' '    printf '\''%s\n'\'' "$state"'
  printf '%s\n' '    [[ "$state" == active ]]'
  printf '%s\n' '    ;;'
  printf '%s\n' '  *) exit 64 ;;'
  printf '%s\n' 'esac'
} >"$MOCK_BIN/systemctl"

{
  printf '%s\n' '#!/usr/bin/env bash'
  printf '%s\n' 'set -euo pipefail'
  printf '%s\n' 'exit 0'
} >"$MOCK_BIN/fuser"
cp "$MOCK_BIN/fuser" "$MOCK_BIN/journalctl"

# shellcheck disable=SC2016 # The generated mock expands these variables when it runs.
{
  printf '%s\n' '#!/usr/bin/env bash'
  printf '%s\n' 'set -euo pipefail'
  printf '%s\n' 'output=""'
  printf '%s\n' 'url=""'
  printf '%s\n' 'while (($# > 0)); do'
  printf '%s\n' '  case "$1" in'
  printf '%s\n' '    -o) output=$2; shift 2 ;;'
  printf '%s\n' '    --max-time|-H|--data|-X) shift 2 ;;'
  printf '%s\n' '    -*) shift ;;'
  printf '%s\n' '    *) url=$1; shift ;;'
  printf '%s\n' '  esac'
  printf '%s\n' 'done'
  printf '%s\n' '[[ -n "$url" ]] || exit 64'
  printf '%s\n' 'printf '\''%s\n'\'' "$url" >>"$MOCK_CURL_LOG"'
  printf '%s\n' 'source_file=""'
  printf '%s\n' 'generated=""'
  printf '%s\n' 'case "$url" in'
  printf '%s\n' '  "$MOCK_RELEASE_ORIGIN"/download/*)'
  printf '%s\n' '    source_file="$MOCK_RELEASE_ROOT/${url#"$MOCK_RELEASE_ORIGIN"/download/}"'
  printf '%s\n' '    ;;'
  printf '%s\n' '  "$MOCK_RELEASE_ORIGIN"/*)'
  printf '%s\n' '    source_file="$MOCK_RELEASE_ROOT/${url#"$MOCK_RELEASE_ORIGIN"/}"'
  printf '%s\n' '    ;;'
  printf '%s\n' '  "$MOCK_API_ORIGIN/health")'
  printf '%s\n' '    [[ $(sed -n '\''1p'\'' "$MOCK_SERVICE_STATE") == active ]] || exit 22'
  printf '%s\n' '    version=$($MOCK_DEPLOY_BINARY --version | awk '\''{ print $2 }'\'')'
  printf '%s\n' '    if [[ "$version" == "${MOCK_BAD_HEALTH_VERSION:-}" \
      || ("${MOCK_BAD_ROLLBACK_HEALTH:-false}" == true \
        && -f "$MOCK_DEPLOY_ROOT/.octostore-deploy-state") ]]; then'
  printf '%s\n' '      generated='\''{"status":"degraded","storage":"wal","db_size_bytes":123}'\'''
  printf '%s\n' '    else'
  printf '%s\n' '      generated='\''{"status":"ok","storage":"wal","db_size_bytes":123}'\'''
  printf '%s\n' '    fi'
  printf '%s\n' '    ;;'
  printf '%s\n' '  "$MOCK_API_ORIGIN/openapi.yaml")'
  printf '%s\n' '    version=$($MOCK_DEPLOY_BINARY --version | awk '\''{ print $2 }'\'')'
  printf '%s\n' '    generated=$(printf '\''info:\n  version: %s\n'\'' "$version")'
  printf '%s\n' '    ;;'
  printf '%s\n' '  "$MOCK_INSTALLER_ORIGIN"/*/install.sh)'
  printf '%s\n' '    installer_tag=${url#"$MOCK_INSTALLER_ORIGIN"/}'
  printf '%s\n' '    installer_tag=${installer_tag%/install.sh}'
  printf '%s\n' '    if [[ "$installer_tag" == "${MOCK_BAD_INSTALLER_TAG:-}" ]]; then'
  printf '%s\n' '      generated='\''# corrupted public installer'\'''
  printf '%s\n' '    else'
  printf '%s\n' '      source_file="$MOCK_DEPLOY_ROOT/install.sh"'
  printf '%s\n' '    fi'
  printf '%s\n' '    ;;'
  printf '%s\n' '  "$MOCK_RELEASE_METADATA_ORIGIN"/*)'
  printf '%s\n' '    metadata_tag=${url#"$MOCK_RELEASE_METADATA_ORIGIN"/}'
  printf '%s\n' '    generated=$(jq -cn --arg tag "$metadata_tag" --arg binary_sha "$MOCK_ATTESTED_ASSET_SHA" --arg supervisor_sha "$MOCK_SUPERVISOR_ASSET_SHA" '\''{tag_name:$tag,immutable:true,assets:([["octostore-linux-amd64","octostore-linux-arm64","octostore-macos-amd64","octostore-macos-arm64"][] | {name:.,digest:("sha256:"+$binary_sha)}]+[{name:"octostore-reference-supervisor.sh",digest:("sha256:"+$supervisor_sha)}])}'\'')'
  printf '%s\n' '    ;;'
  printf '%s\n' '  "$MOCK_SITE_ORIGIN"/*)'
  printf '%s\n' '    relative=${url#"$MOCK_SITE_ORIGIN"/}'
  printf '%s\n' '    if [[ -z "$relative" ]]; then'
  printf '%s\n' '      relative=index.html'
  printf '%s\n' '    elif [[ "$relative" == */ ]]; then'
  printf '%s\n' '      relative="${relative}index.html"'
  printf '%s\n' '    fi'
  printf '%s\n' '    source_file="$MOCK_DEPLOY_ROOT/site/$relative"'
  printf '%s\n' '    ;;'
  printf '%s\n' '  *) exit 22 ;;'
  printf '%s\n' 'esac'
  printf '%s\n' 'if [[ -n "$source_file" ]]; then'
  printf '%s\n' '  [[ -f "$source_file" ]] || exit 22'
  printf '%s\n' '  if [[ -n "$output" ]]; then cp "$source_file" "$output"; else command cat "$source_file"; fi'
  printf '%s\n' 'else'
  printf '%s\n' '  if [[ -n "$output" ]]; then printf '\''%s\n'\'' "$generated" >"$output"; else printf '\''%s\n'\'' "$generated"; fi'
  printf '%s\n' 'fi'
} >"$MOCK_BIN/curl"
chmod 0755 "$MOCK_BIN/systemctl" "$MOCK_BIN/fuser" "$MOCK_BIN/journalctl" "$MOCK_BIN/curl"

CANARY_LOG="$FIXTURE_ROOT/canary.log"
CANARY_SUCCESS="$FIXTURE_ROOT/canary-success"
CANARY_FAILURE="$FIXTURE_ROOT/canary-failure"
# shellcheck disable=SC2016 # Generated canary scripts expand these values when executed.
{
  printf '%s\n' '#!/usr/bin/env bash'
  printf '%s\n' 'set -euo pipefail'
  printf '%s\n' 'cli=$1 supervisor=$2 api=$3'
  printf '%s\n' '[[ $($cli --version) == "octostore 0.14.0" ]]'
  printf '%s\n' 'sh -n "$supervisor"'
  printf '%s\n' '[[ "$api" == https://api.fixture.invalid ]]'
  printf '%s\n' 'printf "success %s %s %s\n" "$cli" "$supervisor" "$api" >>"$MOCK_CANARY_LOG"'
} >"$CANARY_SUCCESS"
# shellcheck disable=SC2016 # Generated canary script expands this value when executed.
{
  printf '%s\n' '#!/usr/bin/env bash'
  printf '%s\n' 'set -euo pipefail'
  printf '%s\n' 'printf "failure\n" >>"$MOCK_CANARY_LOG"'
  printf '%s\n' 'echo "fixture supervised canary rejected the candidate" >&2'
  printf '%s\n' 'exit 91'
} >"$CANARY_FAILURE"
chmod 0755 "$CANARY_SUCCESS" "$CANARY_FAILURE"

deploy_environment() {
  local deploy_tag=${DEPLOY_TAG:-$TAG}
  local deploy_commit=${DEPLOY_EXPECTED_COMMIT:-$TARGET_COMMIT}
  local deploy_asset_sha=${DEPLOY_EXPECTED_ASSET_SHA:-$ATTESTED_ASSET_SHA}
  local metadata_supervisor_sha=${DEPLOY_METADATA_SUPERVISOR_SHA:-$SUPERVISOR_ASSET_SHA}
  env \
    PATH="$MOCK_BIN:$PATH" \
    MOCK_SERVICE_STATE="$SERVICE_STATE" \
    MOCK_SYSTEMCTL_LOG="$SYSTEMCTL_LOG" \
    MOCK_DEPLOY_BINARY="$DEPLOY_BINARY" \
    MOCK_DEPLOY_ROOT="$DEPLOY_ROOT" \
    MOCK_RELEASE_ORIGIN="$RELEASE_ORIGIN" \
    MOCK_RELEASE_ROOT="$RELEASE_ROOT" \
    MOCK_API_ORIGIN="$API_ORIGIN" \
    MOCK_SITE_ORIGIN="$SITE_ORIGIN" \
    MOCK_INSTALLER_ORIGIN="$INSTALLER_ORIGIN" \
    MOCK_RELEASE_METADATA_ORIGIN="$RELEASE_METADATA_ORIGIN" \
    MOCK_ATTESTED_ASSET_SHA="$ATTESTED_ASSET_SHA" \
    MOCK_SUPERVISOR_ASSET_SHA="$metadata_supervisor_sha" \
    MOCK_CURL_LOG="$CURL_LOG" \
    MOCK_CANARY_LOG="$CANARY_LOG" \
    MOCK_FAIL_START="${MOCK_FAIL_START:-false}" \
    MOCK_BAD_HEALTH_VERSION="${MOCK_BAD_HEALTH_VERSION:-}" \
    MOCK_BAD_ROLLBACK_HEALTH="${MOCK_BAD_ROLLBACK_HEALTH:-false}" \
    MOCK_BAD_INSTALLER_TAG="${MOCK_BAD_INSTALLER_TAG:-}" \
    OCTOSTORE_DEPLOY_TEST_MODE=fixture-only \
    OCTOSTORE_DEPLOY_TEST_ROOT="$FIXTURE_ROOT" \
    OCTOSTORE_DEPLOY_ROOT="$DEPLOY_ROOT" \
    OCTOSTORE_DEPLOY_BINARY="$DEPLOY_BINARY" \
    OCTOSTORE_DEPLOY_API_ORIGIN="$API_ORIGIN" \
    OCTOSTORE_DEPLOY_SITE_ORIGIN="$SITE_ORIGIN" \
    OCTOSTORE_DEPLOY_INSTALLER_ORIGIN="$INSTALLER_ORIGIN" \
    OCTOSTORE_DEPLOY_RELEASE_BASE="$RELEASE_ORIGIN" \
    OCTOSTORE_DEPLOY_INSTALL_RELEASE_BASE="$RELEASE_ORIGIN" \
    OCTOSTORE_DEPLOY_RELEASE_METADATA_BASE="$RELEASE_METADATA_ORIGIN" \
    OCTOSTORE_DEPLOY_VERIFY_PARENT="$VERIFY_PARENT" \
    OCTOSTORE_DEPLOY_VERIFY_ATTEMPTS=1 \
    OCTOSTORE_DEPLOY_RETRY_DELAY=0 \
    OCTOSTORE_DEPLOY_SKIP_CANARY=false \
    OCTOSTORE_DEPLOY_CANARY_FIXTURE="${DEPLOY_CANARY_FIXTURE:-$CANARY_SUCCESS}" \
    OCTOSTORE_DEPLOY_FAILPOINT="${DEPLOY_FAILPOINT:-}" \
    OCTOSTORE_DEPLOY_FAIL_MODE="${DEPLOY_FAIL_MODE:-error}" \
    bash "$ROOT/deploy/deploy.sh" "$deploy_tag" "$deploy_commit" "$deploy_asset_sha" \
      "$PREVIOUS_ATTESTED_ASSET_SHA"
}

assert_old_deployment() {
  local log=$1
  [[ $(git -C "$DEPLOY_ROOT" rev-parse HEAD) == "$OLD_COMMIT" ]] || fail "checkout was not rolled back"
  [[ $($DEPLOY_BINARY --version) == "octostore 0.13.0" ]] || fail "binary was not rolled back"
  [[ $(sed -n '1p' "$SERVICE_STATE") == active ]] || fail "service was not restored"
  [[ ! -e "$DEPLOY_ROOT/.octostore-deploy-state" ]] || fail "successful rollback left deployment state"
  [[ -x "${DEPLOY_BINARY}.known-good" ]] || fail "known-good binary was not retained"
  assert_safe_site_modes
  grep -Fq "EVIDENCE rollback_complete commit=$OLD_COMMIT version=0.13.0" "$log" \
    || fail "rollback completion evidence missing"
  grep -Eq "EVIDENCE rollback_site_commit=$OLD_COMMIT files=[1-9][0-9]*" "$log" \
    || fail "exact rollback site evidence missing"
  grep -Fq 'EVIDENCE rollback_api_health=status:ok,storage:wal' "$log" \
    || fail "rollback health evidence missing"
  grep -Fq "EVIDENCE rollback_installer_commit=$OLD_COMMIT version=0.13.0" "$log" \
    || fail "exact rollback installer evidence missing"
}

run_provenance_failure_case() {
  local label=$1
  local release_tag=$2
  local expected_commit=$3
  local expected_error=$4
  local log="$FIXTURE_ROOT/provenance-$label.log"
  local binary_sha status
  binary_sha=$(sha256sum "$DEPLOY_BINARY" | awk '{print $1}')
  : >"$SYSTEMCTL_LOG"
  : >"$CURL_LOG"

  set +e
  DEPLOY_TAG=$release_tag DEPLOY_EXPECTED_COMMIT=$expected_commit \
    deploy_environment >"$log" 2>&1
  status=$?
  set -e

  ((status != 0)) || fail "$label provenance case unexpectedly succeeded"
  grep -Fq "$expected_error" "$log" || fail "$label provenance rejection was not explicit"
  [[ $(git -C "$DEPLOY_ROOT" rev-parse HEAD) == "$OLD_COMMIT" ]] \
    || fail "$label provenance failure changed the checkout"
  [[ $(sha256sum "$DEPLOY_BINARY" | awk '{print $1}') == "$binary_sha" ]] \
    || fail "$label provenance failure changed the binary"
  [[ $(sed -n '1p' "$SERVICE_STATE") == active ]] \
    || fail "$label provenance failure changed service state"
  [[ ! -e "$DEPLOY_ROOT/.octostore-deploy-state" ]] \
    || fail "$label provenance failure created deployment state"
  [[ ! -e "${DEPLOY_BINARY}.known-good" ]] \
    || fail "$label provenance failure created a binary backup"
  [[ ! -s "$SYSTEMCTL_LOG" ]] \
    || fail "$label provenance failure invoked systemctl"
  [[ ! -s "$CURL_LOG" ]] \
    || fail "$label provenance failure fetched release or production content"
  echo "EVIDENCE provenance_rejected case=$label before_mutation=true"
}

run_failure_case() {
  local point=$1
  local mode=$2
  local expected_status=$3
  local log="$FIXTURE_ROOT/failure-$point-$mode.log"
  local status
  : >"$CURL_LOG"
  set +e
  DEPLOY_FAILPOINT=$point DEPLOY_FAIL_MODE=$mode deploy_environment >"$log" 2>&1
  status=$?
  set -e
  if [[ "$expected_status" == nonzero ]]; then
    ((status != 0)) || fail "$point/$mode unexpectedly succeeded"
  elif ((status != expected_status)); then
    fail "$point/$mode exited $status, expected $expected_status"
  fi
  grep -Fq "INJECTED_FAILURE point=$point mode=$mode" "$log" \
    || fail "$point/$mode did not reach its injection point"
  assert_old_deployment "$log"
}

run_asset_digest_failure_case() {
  local wrong_digest=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
  local log="$FIXTURE_ROOT/provenance-asset-digest.log"
  local binary_sha status
  binary_sha=$(sha256sum "$DEPLOY_BINARY" | awk '{print $1}')
  : >"$SYSTEMCTL_LOG"
  : >"$CURL_LOG"

  set +e
  DEPLOY_EXPECTED_ASSET_SHA=$wrong_digest deploy_environment >"$log" 2>&1
  status=$?
  set -e

  ((status != 0)) || fail "wrong attested asset digest unexpectedly succeeded"
  grep -Fq 'asset digest does not match the exact-commit GitHub attestation' "$log" \
    || fail "wrong attested asset digest rejection was not explicit"
  [[ $(git -C "$DEPLOY_ROOT" rev-parse HEAD) == "$OLD_COMMIT" ]] \
    || fail "asset digest failure changed the checkout"
  [[ $(sha256sum "$DEPLOY_BINARY" | awk '{print $1}') == "$binary_sha" ]] \
    || fail "asset digest failure changed the production binary"
  [[ $(sed -n '1p' "$SERVICE_STATE") == active ]] \
    || fail "asset digest failure changed service state"
  [[ ! -e "$DEPLOY_ROOT/.octostore-deploy-state" ]] \
    || fail "asset digest failure created deployment state"
  [[ ! -e "${DEPLOY_BINARY}.known-good" ]] \
    || fail "asset digest failure created a binary backup"
  [[ ! -s "$SYSTEMCTL_LOG" ]] \
    || fail "asset digest failure invoked systemctl"
  [[ $(grep -Fxc "$RELEASE_ORIGIN/$TAG/octostore-linux-amd64" "$CURL_LOG") == 1 ]] \
    || fail "asset digest failure did not fetch exactly one candidate asset"
  echo 'EVIDENCE provenance_rejected case=asset-digest before_mutation=true'
}

run_previous_digest_failure_case() {
  local original="$FIXTURE_ROOT/original-production-binary"
  local log="$FIXTURE_ROOT/provenance-previous-asset-digest.log"
  local status
  cp "$DEPLOY_BINARY" "$original"
  make_binary "$DEPLOY_BINARY" 0.13.0
  printf '%s\n' '# substituted after the running service started' >>"$DEPLOY_BINARY"
  : >"$SYSTEMCTL_LOG"
  : >"$CURL_LOG"

  set +e
  deploy_environment >"$log" 2>&1
  status=$?
  set -e
  cp "$original" "$DEPLOY_BINARY"
  chmod 0755 "$DEPLOY_BINARY"

  ((status != 0)) || fail "substituted previous binary unexpectedly became rollback authority"
  grep -Fq 'current production binary does not match independently attested previous release' "$log" \
    || fail "previous binary provenance rejection was not explicit"
  [[ $(git -C "$DEPLOY_ROOT" rev-parse HEAD) == "$OLD_COMMIT" ]] \
    || fail "previous provenance failure changed checkout"
  [[ $(sed -n '1p' "$SERVICE_STATE") == active ]] \
    || fail "previous provenance failure changed service state"
  [[ ! -e "$DEPLOY_ROOT/.octostore-deploy-state" ]] \
    || fail "previous provenance failure created deployment state"
  [[ ! -e "${DEPLOY_BINARY}.known-good" ]] \
    || fail "previous provenance failure created a backup"
  [[ ! -s "$SYSTEMCTL_LOG" ]] || fail "previous provenance failure invoked systemctl"
  echo 'EVIDENCE provenance_rejected case=previous-asset-digest before_mutation=true'
}

run_proof_failure_case() {
  local proof=$1
  local expected_error=$2
  local log="$FIXTURE_ROOT/proof-$proof.log"
  local status
  : >"$CURL_LOG"
  set +e
  case "$proof" in
    health)
      MOCK_BAD_HEALTH_VERSION=0.14.0 deploy_environment >"$log" 2>&1
      ;;
    installer)
      MOCK_BAD_INSTALLER_TAG=$TAG deploy_environment >"$log" 2>&1
      ;;
    *)
      fail "unknown proof failure case: $proof"
      ;;
  esac
  status=$?
  set -e
  ((status != 0)) || fail "$proof proof failure unexpectedly succeeded"
  grep -Fq "$expected_error" "$log" || fail "$proof proof failure was not reported"
  assert_old_deployment "$log"
  echo "EVIDENCE proof_failure case=$proof rollback=verified"
}

run_canary_failure_case() {
  local log="$FIXTURE_ROOT/proof-supervised-canary.log"
  local status
  : >"$CANARY_LOG"
  set +e
  DEPLOY_CANARY_FIXTURE=$CANARY_FAILURE deploy_environment >"$log" 2>&1
  status=$?
  set -e
  ((status != 0)) || fail "supervised canary failure unexpectedly succeeded"
  grep -Fq 'fixture supervised canary rejected the candidate' "$log" \
    || fail "supervised canary failure was not reported"
  grep -Fxq failure "$CANARY_LOG" || fail "failing supervised canary was not executed"
  assert_old_deployment "$log"
  echo 'EVIDENCE proof_failure case=supervised-canary rollback=verified'
}

run_supervisor_provenance_failure_case() {
  local asset="$RELEASE_ROOT/$TAG/octostore-reference-supervisor.sh"
  local sums="$RELEASE_ROOT/$TAG/SHA256SUMS"
  local original_asset="$FIXTURE_ROOT/original-reference-supervisor.sh"
  local original_sums="$FIXTURE_ROOT/original-SHA256SUMS"
  local log="$FIXTURE_ROOT/proof-supervisor-provenance.log"
  local release_asset tampered_sha status

  cp "$asset" "$original_asset"
  cp "$sums" "$original_sums"
  printf '%s\n' '#!/bin/sh' 'exit 0' >"$asset"
  chmod 0755 "$asset"
  tampered_sha=$(sha256sum "$asset" | awk '{print $1}')
  {
    for release_asset in \
      octostore-linux-amd64 octostore-linux-arm64 \
      octostore-macos-amd64 octostore-macos-arm64; do
      printf '%s  %s\n' "$ATTESTED_ASSET_SHA" "$release_asset"
    done
    printf '%s  %s\n' "$tampered_sha" octostore-reference-supervisor.sh
  } >"$sums"

  set +e
  DEPLOY_METADATA_SUPERVISOR_SHA="$tampered_sha" deploy_environment >"$log" 2>&1
  status=$?
  set -e
  cp "$original_asset" "$asset"
  cp "$original_sums" "$sums"

  ((status != 0)) || fail "jointly tampered supervisor and checksum unexpectedly succeeded"
  grep -Fq "installed supervisor does not match commit $TARGET_COMMIT" "$log" || {
    sed -n '1,160p' "$log" >&2
    fail "supervisor source-provenance rejection was not explicit"
  }
  assert_old_deployment "$log"
  echo 'EVIDENCE proof_failure case=supervisor-provenance rollback=verified'
}

run_provenance_failure_case \
  tag-commit-mismatch "$TAG" "$OLD_COMMIT" \
  "tag $TAG resolves to $TARGET_COMMIT, expected $OLD_COMMIT"
run_provenance_failure_case \
  off-main-tag "$OFF_MAIN_TAG" "$OFF_MAIN_COMMIT" \
  "release commit $OFF_MAIN_COMMIT is not on origin/main"
run_asset_digest_failure_case
run_previous_digest_failure_case

for point in after_checkout after_service_stop after_binary_swap; do
  run_failure_case "$point" error nonzero
done
run_failure_case after_service_stop INT 130
run_failure_case after_service_stop TERM 143
run_failure_case after_service_stop HUP 129
run_proof_failure_case health 'live API health response does not match the documented contract'
run_proof_failure_case installer 'public installer at'
run_supervisor_provenance_failure_case
run_canary_failure_case

rollback_health_failure_log="$FIXTURE_ROOT/rollback-health-failure.log"
set +e
MOCK_BAD_ROLLBACK_HEALTH=true DEPLOY_FAILPOINT=after_service_stop DEPLOY_FAIL_MODE=error \
  deploy_environment >"$rollback_health_failure_log" 2>&1
rollback_health_failure_status=$?
set -e
((rollback_health_failure_status == 2)) || fail "bad rollback health did not exit 2"
grep -Fq 'live API health response does not match the documented contract' \
  "$rollback_health_failure_log" || fail "bad rollback health was not detected"
grep -Fq 'ROLLBACK_FAILED:' "$rollback_health_failure_log" \
  || fail "bad rollback health was not reported as rollback failure"
[[ -f "$DEPLOY_ROOT/.octostore-deploy-state" ]] \
  || fail "bad rollback health did not preserve recovery state"
[[ -x "${DEPLOY_BINARY}.known-good" ]] \
  || fail "bad rollback health did not preserve the known-good binary"
echo 'EVIDENCE proof_failure case=rollback-health rollback_failure=reported'

rollback_health_recovery_log="$FIXTURE_ROOT/rollback-health-recovery.log"
set +e
DEPLOY_FAILPOINT=after_checkout DEPLOY_FAIL_MODE=error \
  deploy_environment >"$rollback_health_recovery_log" 2>&1
rollback_health_recovery_status=$?
set -e
((rollback_health_recovery_status != 0)) \
  || fail "rollback-health recovery case unexpectedly succeeded"
grep -Fq 'RECOVERY: found an incomplete deployment' "$rollback_health_recovery_log" \
  || fail "rerun did not recover the rollback-health failure first"
assert_old_deployment "$rollback_health_recovery_log"

rollback_failure_log="$FIXTURE_ROOT/rollback-failure.log"
set +e
MOCK_FAIL_START=true DEPLOY_FAILPOINT=after_service_stop DEPLOY_FAIL_MODE=error \
  deploy_environment >"$rollback_failure_log" 2>&1
rollback_failure_status=$?
set -e
((rollback_failure_status == 2)) || fail "failed rollback did not exit 2"
grep -Fq 'ROLLBACK_FAILED:' "$rollback_failure_log" || fail "failed rollback was not reported"
[[ -f "$DEPLOY_ROOT/.octostore-deploy-state" ]] || fail "failed rollback did not preserve state"
[[ -x "${DEPLOY_BINARY}.known-good" ]] || fail "failed rollback did not preserve known-good binary"

recovery_failure_log="$FIXTURE_ROOT/recover-failed-rollback.log"
set +e
DEPLOY_FAILPOINT=after_checkout DEPLOY_FAIL_MODE=error \
  deploy_environment >"$recovery_failure_log" 2>&1
recovery_failure_status=$?
set -e
((recovery_failure_status != 0)) || fail "recovered deployment failure unexpectedly succeeded"
grep -Fq 'RECOVERY: found an incomplete deployment' "$recovery_failure_log" \
  || fail "rerun did not recover durable state first"
assert_old_deployment "$recovery_failure_log"

crash_log="$FIXTURE_ROOT/crash.log"
set +e
DEPLOY_FAILPOINT=after_binary_swap DEPLOY_FAIL_MODE=KILL \
  deploy_environment >"$crash_log" 2>&1
crash_status=$?
set -e
((crash_status == 137)) || fail "hard interruption exited $crash_status, expected 137"
[[ -f "$DEPLOY_ROOT/.octostore-deploy-state" ]] || fail "hard interruption lost durable state"
[[ -x "${DEPLOY_BINARY}.known-good" ]] || fail "hard interruption lost known-good binary"

success_log="$FIXTURE_ROOT/recovery-success.log"
: >"$CURL_LOG"
deploy_environment >"$success_log" 2>&1
grep -Fq 'RECOVERY: found an incomplete deployment' "$success_log" \
  || fail "successful rerun did not recover the interrupted attempt"
grep -Fq "EVIDENCE rollback_complete commit=$OLD_COMMIT version=0.13.0" "$success_log" \
  || fail "successful rerun did not verify rollback before retrying"
[[ $(git -C "$DEPLOY_ROOT" rev-parse HEAD) == "$TARGET_COMMIT" ]] || fail "target checkout was not deployed"
[[ $($DEPLOY_BINARY --version) == "octostore 0.14.0" ]] || fail "target binary was not deployed"
[[ $(sed -n '1p' "$SERVICE_STATE") == active ]] || fail "target service is not active"
[[ ! -e "$DEPLOY_ROOT/.octostore-deploy-state" ]] || fail "successful deploy left state behind"
assert_safe_site_modes
[[ $("${DEPLOY_BINARY}.known-good" --version) == "octostore 0.13.0" ]] \
  || fail "successful deploy did not retain the prior known-good binary"

TARGET_SITE_COUNT=$(git -C "$DEPLOY_ROOT" ls-tree -r --name-only "$TARGET_COMMIT" -- site | wc -l | tr -d ' ')
grep -Fq "EVIDENCE live_site_commit=$TARGET_COMMIT files=$TARGET_SITE_COUNT" "$success_log" \
  || fail "successful deploy did not prove the complete target site manifest"
grep -Fq 'EVIDENCE live_api_health=status:ok,storage:wal' "$success_log" \
  || fail "successful deploy did not prove the documented health response"
grep -Fq "EVIDENCE live_installer_commit=$TARGET_COMMIT version=0.14.0" "$success_log" \
  || fail "successful deploy did not prove the exact public installer"
grep -Fq 'EVIDENCE live_installed_pair=version:0.14.0' "$success_log" \
  || fail "successful deploy did not install and verify the tagged CLI/supervisor pair"
grep -Fq 'EVIDENCE fixture_supervised_canary=passed installed_pair=true' "$success_log" \
  || fail "successful deploy did not run the supervised canary gate"
grep -q '^success ' "$CANARY_LOG" || fail "successful supervised canary was not executed"
grep -Fxq "$API_ORIGIN/health" "$CURL_LOG" \
  || fail "successful deploy did not request the public health endpoint"
grep -Fxq "$INSTALLER_ORIGIN/$TAG/install.sh" "$CURL_LOG" \
  || fail "successful deploy did not request the versioned public installer"
echo "EVIDENCE public_proof health=validated installer_commit=$TARGET_COMMIT installer_version=0.14.0"
while IFS= read -r path; do
  relative=${path#site/}
  case "$relative" in
    index.html) url="$SITE_ORIGIN/" ;;
    */index.html) url="$SITE_ORIGIN/${relative%index.html}" ;;
    *) url="$SITE_ORIGIN/$relative" ;;
  esac
  grep -Fxq "$url" "$CURL_LOG" || fail "site manifest URL was not requested: $url"
done < <(git -C "$DEPLOY_ROOT" -c core.quotePath=false ls-tree -r --name-only "$TARGET_COMMIT" -- site)

override_log="$FIXTURE_ROOT/production-override.log"
set +e
OCTOSTORE_DEPLOY_ROOT="$DEPLOY_ROOT" bash "$ROOT/deploy/deploy.sh" \
  "$TAG" "$TARGET_COMMIT" "$ATTESTED_ASSET_SHA" \
  "$PREVIOUS_ATTESTED_ASSET_SHA" \
  >"$override_log" 2>&1
override_status=$?
set -e
((override_status == 64)) || fail "production override was not rejected with usage status"
grep -Fq 'fixture-only and cannot override production' "$override_log" \
  || fail "production override rejection was not explicit"

echo "deployment failure fixture passed: pre-mutation provenance rejection, ERR/INT/TERM/HUP rollback, health/installer proof failures, rerun recovery, and $TARGET_SITE_COUNT exact site files"
