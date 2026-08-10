#!/usr/bin/env bash
# shellcheck disable=SC2016 # Single-quoted fixture scripts expand only when executed.
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

VERSION=$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)
[[ -n "$VERSION" ]] || { echo "could not read Cargo package version" >&2; exit 70; }
if [[ -n ${OCTOSTORE_SMOKE_BINARY:-} ]]; then
  SOURCE_BINARY=$OCTOSTORE_SMOKE_BINARY
  [[ -x "$SOURCE_BINARY" ]] || {
    echo "explicit smoke binary is not executable: $SOURCE_BINARY" >&2
    exit 64
  }
else
  SOURCE_BINARY="$ROOT/target/release/octostore"
  cargo build --release --locked --bin octostore
fi

case "$(uname -s | tr '[:upper:]' '[:lower:]')" in
  linux) PLATFORM=linux ;;
  darwin) PLATFORM=macos ;;
  *) echo "unsupported smoke-test OS" >&2; exit 64 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=amd64 ;;
  arm64|aarch64) ARCH=arm64 ;;
  *) echo "unsupported smoke-test architecture" >&2; exit 64 ;;
esac

ASSET="octostore-${PLATFORM}-${ARCH}"
SMOKE_DIR=$(mktemp -d "$ROOT/.smoke-release.XXXXXX")
FIXTURE_DIR="$SMOKE_DIR/releases/download/v$VERSION"
INSTALL_DIR="$SMOKE_DIR/bin with spaces"
INSTALL_TMP_DIR="$SMOKE_DIR/installer-tmp"
mkdir -p "$FIXTURE_DIR" "$INSTALL_TMP_DIR"
[[ ! -e "$INSTALL_DIR" ]]

cleanup() {
  chmod -R u+w "$SMOKE_DIR" 2>/dev/null || true
  rm -rf "$SMOKE_DIR"
}
trap cleanup EXIT INT TERM

cp "$SOURCE_BINARY" "$FIXTURE_DIR/$ASSET"
cp "$ROOT/scripts/reference-supervisor.sh" \
  "$FIXTURE_DIR/octostore-reference-supervisor.sh"
if command -v sha256sum >/dev/null 2>&1; then
  SHA=$(sha256sum "$FIXTURE_DIR/$ASSET" | awk '{print $1}')
  SUPERVISOR_SHA=$(sha256sum "$FIXTURE_DIR/octostore-reference-supervisor.sh" | awk '{print $1}')
else
  SHA=$(shasum -a 256 "$FIXTURE_DIR/$ASSET" | awk '{print $1}')
  SUPERVISOR_SHA=$(shasum -a 256 "$FIXTURE_DIR/octostore-reference-supervisor.sh" | awk '{print $1}')
fi
printf '%s  %s\n%s  %s\n' \
  "$SHA" "$ASSET" \
  "$SUPERVISOR_SHA" octostore-reference-supervisor.sh \
  >"$FIXTURE_DIR/SHA256SUMS"

RELEASE_METADATA="$SMOKE_DIR/release-metadata.json"
jq -cn \
  --arg tag "v$VERSION" \
  --arg binary "$ASSET" \
  --arg binary_digest "sha256:$SHA" \
  --arg supervisor octostore-reference-supervisor.sh \
  --arg supervisor_digest "sha256:$SUPERVISOR_SHA" \
  '{tag_name:$tag, immutable:true, assets:[
    {name:$binary, digest:$binary_digest},
    {name:$supervisor, digest:$supervisor_digest}
  ]}' >"$RELEASE_METADATA"
export OCTOSTORE_RELEASE_METADATA_URL="file://$RELEASE_METADATA"

# A substituted executable and matching co-located checksum must still fail
# against the independently supplied immutable release metadata. The malicious
# supervisor must not execute even its dependency-check branch.
TAMPERED_INSTALL_DIR="$SMOKE_DIR/tampered-bin"
TAMPERED_MARKER="$SMOKE_DIR/tampered-supervisor-executed"
ORIGINAL_SUPERVISOR="$SMOKE_DIR/original-reference-supervisor.sh"
ORIGINAL_SUMS="$SMOKE_DIR/original-SHA256SUMS"
cp "$FIXTURE_DIR/octostore-reference-supervisor.sh" "$ORIGINAL_SUPERVISOR"
cp "$FIXTURE_DIR/SHA256SUMS" "$ORIGINAL_SUMS"
printf '%s\n' \
  '#!/bin/sh' \
  ': > "$OCTOSTORE_TAMPERED_SUPERVISOR_MARKER"' \
  'exit 0' >"$FIXTURE_DIR/octostore-reference-supervisor.sh"
chmod 0755 "$FIXTURE_DIR/octostore-reference-supervisor.sh"
if command -v sha256sum >/dev/null 2>&1; then
  TAMPERED_SUPERVISOR_SHA=$(sha256sum \
    "$FIXTURE_DIR/octostore-reference-supervisor.sh" | awk '{print $1}')
else
  TAMPERED_SUPERVISOR_SHA=$(shasum -a 256 \
    "$FIXTURE_DIR/octostore-reference-supervisor.sh" | awk '{print $1}')
fi
printf '%s  %s\n%s  %s\n' \
  "$SHA" "$ASSET" \
  "$TAMPERED_SUPERVISOR_SHA" octostore-reference-supervisor.sh \
  >"$FIXTURE_DIR/SHA256SUMS"
if OCTOSTORE_TAMPERED_SUPERVISOR_MARKER="$TAMPERED_MARKER" \
  OCTOSTORE_VERSION="v$VERSION" \
  OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
  OCTOSTORE_INSTALL_DIR="$TAMPERED_INSTALL_DIR" \
  TMPDIR="$INSTALL_TMP_DIR" \
    sh "$ROOT/install.sh" >"$SMOKE_DIR/tampered-install.log" 2>&1; then
  echo "installer accepted a substituted supervisor and matching SHA256SUMS" >&2
  exit 1
fi
grep -F "Independent release metadata digest mismatch for octostore-reference-supervisor.sh" \
  "$SMOKE_DIR/tampered-install.log" >/dev/null
[[ ! -e "$TAMPERED_MARKER" ]]
[[ ! -e "$TAMPERED_INSTALL_DIR" ]]
mv "$ORIGINAL_SUPERVISOR" "$FIXTURE_DIR/octostore-reference-supervisor.sh"
mv "$ORIGINAL_SUMS" "$FIXTURE_DIR/SHA256SUMS"

# Recreate the documented clean-machine route before the primary installer
# invocation. This PATH deliberately contains only the route's prerequisites
# plus the installer's POSIX utilities; it selects the documented checksum
# command and validates the required suspend-inclusive clock first.
CLEAN_PATH_DIR="$SMOKE_DIR/clean-path"
mkdir -p "$CLEAN_PATH_DIR"
case "$PLATFORM" in
  linux)
    CLEAN_CHECKSUM=sha256sum
    CLEAN_CLOCK='clock_gettime,CLOCK_BOOTTIME'
    CLEAN_CLOCK_TEST='exit(clock_gettime(CLOCK_BOOTTIME) > 0 ? 0 : 1)'
    ;;
  macos)
    CLEAN_CHECKSUM=shasum
    CLEAN_CLOCK='clock_gettime,CLOCK_MONOTONIC_RAW'
    CLEAN_CLOCK_TEST='exit(clock_gettime(CLOCK_MONOTONIC_RAW) > 0 ? 0 : 1)'
    ;;
esac
for utility in awk basename chmod cp curl dirname env grep id jq mkdir mktemp mv \
  perl rm sed sh tr uname bash seq "$CLEAN_CHECKSUM"; do
  utility_path=$(command -v "$utility")
  [[ -n $utility_path ]] || {
    echo "clean-path prerequisite is unavailable: $utility" >&2
    exit 70
  }
  ln -s "$utility_path" "$CLEAN_PATH_DIR/$utility"
done
SH_BIN=$(command -v sh)
env -i PATH="$CLEAN_PATH_DIR" "$SH_BIN" -eu -c '
  command -v sh curl jq perl bash seq
  command -v "$1"
' sh "$CLEAN_CHECKSUM" >/dev/null
"$CLEAN_PATH_DIR/perl" -MTime::HiRes="$CLEAN_CLOCK" \
  -e "$CLEAN_CLOCK_TEST"
env -i \
  PATH="$CLEAN_PATH_DIR" \
  OCTOSTORE_RELEASE_METADATA_URL="$OCTOSTORE_RELEASE_METADATA_URL" \
  OCTOSTORE_VERSION="v$VERSION" \
  OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
  OCTOSTORE_INSTALL_DIR="$INSTALL_DIR" \
  TMPDIR="$INSTALL_TMP_DIR" \
  "$SH_BIN" "$ROOT/install.sh" >"$SMOKE_DIR/install.log"
[[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]

INSTALLED="$INSTALL_DIR/octostore"
INSTALLED_SUPERVISOR="$INSTALL_DIR/octostore-supervisor"
[[ -d "$INSTALL_DIR" ]]
[[ -x "$INSTALLED" ]]
[[ -x "$INSTALLED_SUPERVISOR" ]]
[[ $("$INSTALLED" --version) == "octostore $VERSION" ]]
sh -n "$INSTALLED_SUPERVISOR"
case "$PLATFORM" in
  linux)
    INSTALLED_MODE=$(stat -c '%a' "$INSTALLED")
    INSTALLED_OWNER=$(stat -c '%u' "$INSTALLED")
    SUPERVISOR_MODE=$(stat -c '%a' "$INSTALLED_SUPERVISOR")
    SUPERVISOR_OWNER=$(stat -c '%u' "$INSTALLED_SUPERVISOR")
    ;;
  macos)
    INSTALLED_MODE=$(stat -f '%Lp' "$INSTALLED")
    INSTALLED_OWNER=$(stat -f '%u' "$INSTALLED")
    SUPERVISOR_MODE=$(stat -f '%Lp' "$INSTALLED_SUPERVISOR")
    SUPERVISOR_OWNER=$(stat -f '%u' "$INSTALLED_SUPERVISOR")
    ;;
esac
[[ $INSTALLED_MODE == 755 ]]
[[ $INSTALLED_OWNER == "$(id -u)" ]]
[[ $SUPERVISOR_MODE == 755 ]]
[[ $SUPERVISOR_OWNER == "$(id -u)" ]]
"$INSTALLED" --help >/dev/null
grep -F "Created install directory $INSTALL_DIR" "$SMOKE_DIR/install.log" >/dev/null
grep -F 'umask 077' "$SMOKE_DIR/install.log" >/dev/null
grep -F 'od -An -N32 -tx1 /dev/urandom' "$SMOKE_DIR/install.log" >/dev/null
grep -F "chmod 600 \"\$TOKEN_FILE\"" "$SMOKE_DIR/install.log" >/dev/null
grep -F "OCTOSTORE_BIN='$INSTALLED'" "$SMOKE_DIR/install.log" >/dev/null
# Literal lines expected in the printed recipe.
# shellcheck disable=SC2016
grep -F 'BIND_ADDR="127.0.0.1:$OCTOSTORE_PORT" STATIC_TOKENS_FILE="$TOKEN_FILE" "$OCTOSTORE_BIN" serve &' \
  "$SMOKE_DIR/install.log" >/dev/null
# Literal lines expected in the printed recipe.
# shellcheck disable=SC2016
grep -F 'curl --fail --silent --show-error --retry 20 --retry-connrefused --retry-delay 1 "http://127.0.0.1:$OCTOSTORE_PORT/health"' \
  "$SMOKE_DIR/install.log" >/dev/null
if grep -F 'local:change-me' "$SMOKE_DIR/install.log" >/dev/null; then
  echo "installer printed an insecure fixed-token example" >&2
  exit 1
fi

GUIDANCE_SCRIPT="$SMOKE_DIR/secure-first-run.sh"
GUIDANCE_CONFIG="$SMOKE_DIR/guidance-config"
GUIDANCE_HOME="$SMOKE_DIR/guidance-home"
GUIDANCE_PATH_DIR="$SMOKE_DIR/guidance-path"
GUIDANCE_OUTPUT="$SMOKE_DIR/guidance-output.log"
awk '
  /^  OCTOSTORE_BIN=/ { capture = 1 }
  capture {
    line = $0
    sub(/^  /, "", line)
    print line
    if (line ~ /^curl --fail /) exit
  }
' "$SMOKE_DIR/install.log" >"$GUIDANCE_SCRIPT"
mkdir -p "$GUIDANCE_HOME" "$GUIDANCE_PATH_DIR"
for utility in dirname mkdir od tr chmod curl; do
  utility_path=$(command -v "$utility")
  [[ -n $utility_path ]]
  ln -s "$utility_path" "$GUIDANCE_PATH_DIR/$utility"
done
SH_BIN=$(command -v sh)
GUIDANCE_PORT=$((48000 + ($$ % 1000)))
(
  cd "$GUIDANCE_HOME"
  # The child shell, not this fixture, expands these variables.
  # shellcheck disable=SC2016
  PATH="$GUIDANCE_PATH_DIR" \
  XDG_CONFIG_HOME="$GUIDANCE_CONFIG" \
  HOME="$GUIDANCE_HOME" \
  OCTOSTORE_PORT="$GUIDANCE_PORT" \
    "$SH_BIN" -eu -c '
      if command -v octostore >/dev/null 2>&1; then
        echo "fixture unexpectedly exposed a bare octostore command" >&2
        exit 1
      fi
      OCTOSTORE_PID=""
      cleanup_recipe() {
        if [ -n "$OCTOSTORE_PID" ]; then
          kill -TERM "$OCTOSTORE_PID" 2>/dev/null || true
          wait "$OCTOSTORE_PID" 2>/dev/null || true
        fi
      }
      trap cleanup_recipe EXIT INT TERM
      . "$1"
    ' sh "$GUIDANCE_SCRIPT"
) >"$GUIDANCE_OUTPUT"
grep -F '"status":"ok"' "$GUIDANCE_OUTPUT" >/dev/null
GUIDANCE_TOKEN_FILE="$GUIDANCE_CONFIG/octostore/static-tokens"
[[ -f "$GUIDANCE_TOKEN_FILE" ]]
grep -Eq '^local:[0-9a-f]{64}$' "$GUIDANCE_TOKEN_FILE"
case "$PLATFORM" in
  linux) TOKEN_MODE=$(stat -c '%a' "$GUIDANCE_TOKEN_FILE") ;;
  macos) TOKEN_MODE=$(stat -f '%Lp' "$GUIDANCE_TOKEN_FILE") ;;
esac
[[ $TOKEN_MODE == 600 ]]

INVALID_DESTINATION="$SMOKE_DIR/not-a-directory"
: >"$INVALID_DESTINATION"
if OCTOSTORE_VERSION="v$VERSION" \
  OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
  OCTOSTORE_INSTALL_DIR="$INVALID_DESTINATION" \
    sh "$ROOT/install.sh" >"$SMOKE_DIR/invalid-destination.log" 2>&1; then
  echo "installer accepted a non-directory destination" >&2
  exit 1
fi
grep -F "exists but is not a directory: $INVALID_DESTINATION" \
  "$SMOKE_DIR/invalid-destination.log" >/dev/null

EXISTING_TARGET_INSTALL_DIR="$SMOKE_DIR/existing-target-bin"
mkdir -p "$EXISTING_TARGET_INSTALL_DIR/octostore"
if OCTOSTORE_VERSION="v$VERSION" \
  OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
  OCTOSTORE_INSTALL_DIR="$EXISTING_TARGET_INSTALL_DIR" \
  TMPDIR="$INSTALL_TMP_DIR" \
    sh "$ROOT/install.sh" >"$SMOKE_DIR/existing-target.log" 2>&1; then
  echo "installer accepted an existing octostore directory as its target" >&2
  exit 1
fi
grep -F "Install target exists but is not a regular file: $EXISTING_TARGET_INSTALL_DIR/octostore" \
  "$SMOKE_DIR/existing-target.log" >/dev/null
[[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]

# A failed final rename must leave existing executables untouched and remove
# both same-filesystem staging files.
INTERRUPTED_INSTALL_DIR="$SMOKE_DIR/interrupted-bin"
INTERRUPTED_FAKE_BIN="$SMOKE_DIR/interrupted-fake-bin"
INTERRUPTED_MOVE_LOG="$SMOKE_DIR/interrupted-move.log"
INTERRUPTED_REAL_MV=$(command -v mv)
mkdir -p "$INTERRUPTED_INSTALL_DIR" "$INTERRUPTED_FAKE_BIN"
printf '%s\n' old-octostore >"$INTERRUPTED_INSTALL_DIR/octostore"
printf '%s\n' old-supervisor >"$INTERRUPTED_INSTALL_DIR/octostore-supervisor"
chmod 0755 \
  "$INTERRUPTED_INSTALL_DIR/octostore" \
  "$INTERRUPTED_INSTALL_DIR/octostore-supervisor"
printf '%s\n' \
  '#!/bin/sh' \
  'printf "%s\n" "$*" >> "$OCTOSTORE_TEST_MOVE_LOG"' \
  'source_path=' \
  'target_path=' \
  'for arg do' \
  '  case "$arg" in -*) continue ;; esac' \
  '  if [ -z "$source_path" ]; then source_path=$arg; else target_path=$arg; fi' \
  'done' \
  'case "$source_path" in *.backup.*) exec "$OCTOSTORE_TEST_REAL_MV" "$@" ;; esac' \
  '[ "$target_path" != "$OCTOSTORE_TEST_BINARY_TARGET" ] || exit 99' \
  'exec "$OCTOSTORE_TEST_REAL_MV" "$@"' >"$INTERRUPTED_FAKE_BIN/mv"
chmod +x "$INTERRUPTED_FAKE_BIN/mv"
if PATH="$INTERRUPTED_FAKE_BIN:$PATH" \
  OCTOSTORE_TEST_MOVE_LOG="$INTERRUPTED_MOVE_LOG" \
  OCTOSTORE_TEST_REAL_MV="$INTERRUPTED_REAL_MV" \
  OCTOSTORE_TEST_BINARY_TARGET="$INTERRUPTED_INSTALL_DIR/octostore" \
  OCTOSTORE_VERSION="v$VERSION" \
  OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
  OCTOSTORE_INSTALL_DIR="$INTERRUPTED_INSTALL_DIR" \
  TMPDIR="$INSTALL_TMP_DIR" \
    sh "$ROOT/install.sh" >"$SMOKE_DIR/interrupted-install.log" 2>&1; then
  echo "installer ignored an interrupted final rename" >&2
  exit 1
fi
[[ $(cat "$INTERRUPTED_INSTALL_DIR/octostore") == old-octostore ]]
[[ $(cat "$INTERRUPTED_INSTALL_DIR/octostore-supervisor") == old-supervisor ]]
[[ -s "$INTERRUPTED_MOVE_LOG" ]]
grep -E "octostore-supervisor\.tmp\..+ $INTERRUPTED_INSTALL_DIR/octostore-supervisor$" \
  "$INTERRUPTED_MOVE_LOG" >/dev/null
grep -E "octostore\.tmp\..+ $INTERRUPTED_INSTALL_DIR/octostore$" \
  "$INTERRUPTED_MOVE_LOG" >/dev/null
grep -E "octostore-supervisor\.backup\..+ $INTERRUPTED_INSTALL_DIR/octostore-supervisor$" \
  "$INTERRUPTED_MOVE_LOG" >/dev/null
[[ -z $(find "$INTERRUPTED_INSTALL_DIR" -name '*.tmp.*' -print -quit) ]]
[[ -z $(find "$INTERRUPTED_INSTALL_DIR" -name '*.backup.*' -print -quit) ]]
[[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]

# A signal after either successful replacement must roll back the complete
# pair, exit with the conventional signal status, and never reach success
# output. The installer seam sends the requested real signal immediately after
# the named rename while rollback is armed.
assert_signal_rollback() {
  local phase=$1
  local signal_name=$2
  local expected_status
  local case_dir="$SMOKE_DIR/signal-${phase}-${signal_name}"
  local case_install_dir="$case_dir/bin"
  local old_binary="$case_dir/old-octostore"
  local old_supervisor="$case_dir/old-octostore-supervisor"
  local log_file="$case_dir/install.log"
  local status

  case "$signal_name" in
    HUP) expected_status=129 ;;
    INT) expected_status=130 ;;
    TERM) expected_status=143 ;;
    *) echo "unknown test signal: $signal_name" >&2; return 64 ;;
  esac

  mkdir -p "$case_install_dir"
  printf '%s\n' "old-octostore-${phase}-${signal_name}" >"$old_binary"
  printf '%s\n' "old-supervisor-${phase}-${signal_name}" >"$old_supervisor"
  cp "$old_binary" "$case_install_dir/octostore"
  cp "$old_supervisor" "$case_install_dir/octostore-supervisor"
  chmod 0755 "$case_install_dir/octostore" "$case_install_dir/octostore-supervisor"

  if OCTOSTORE_TEST_INTERRUPT_AFTER_RENAME="$phase" \
    OCTOSTORE_TEST_INTERRUPT_SIGNAL="$signal_name" \
    OCTOSTORE_VERSION="v$VERSION" \
    OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
    OCTOSTORE_INSTALL_DIR="$case_install_dir" \
    TMPDIR="$INSTALL_TMP_DIR" \
      sh "$ROOT/install.sh" >"$log_file" 2>&1; then
    echo "installer resumed after $signal_name at $phase rename" >&2
    return 1
  else
    status=$?
  fi

  [[ $status -eq $expected_status ]] || {
    echo "installer exited $status after $signal_name at $phase rename; expected $expected_status" >&2
    return 1
  }
  cmp -s "$old_binary" "$case_install_dir/octostore"
  cmp -s "$old_supervisor" "$case_install_dir/octostore-supervisor"
  if grep -F '✅ Installed' "$log_file" >/dev/null; then
    echo "installer printed success after $signal_name at $phase rename" >&2
    return 1
  fi
  [[ -z $(find "$case_install_dir" \( -name '*.tmp.*' -o -name '*.backup.*' \) -print -quit) ]]
  [[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]
}

for signal_name in HUP INT TERM; do
  assert_signal_rollback supervisor "$signal_name"
  assert_signal_rollback binary "$signal_name"
done

# Reject any attempt to rename a download temporary directly into place. This
# models a cross-filesystem TMPDIR while allowing sibling staging renames.
CROSS_INSTALL_DIR="$SMOKE_DIR/cross-filesystem-bin"
CROSS_FAKE_BIN="$SMOKE_DIR/cross-filesystem-fake-bin"
REAL_MV=$(command -v mv)
mkdir -p "$CROSS_FAKE_BIN"
printf '%s\n' \
  '#!/bin/sh' \
  'source_path=' \
  'for arg do case "$arg" in -*) ;; *) source_path=$arg; break ;; esac; done' \
  'case "$source_path" in "$OCTOSTORE_TEST_DOWNLOAD_TMP"/*) exit 90 ;; esac' \
  'exec "$OCTOSTORE_TEST_REAL_MV" "$@"' >"$CROSS_FAKE_BIN/mv"
chmod +x "$CROSS_FAKE_BIN/mv"
PATH="$CROSS_FAKE_BIN:$PATH" \
OCTOSTORE_TEST_DOWNLOAD_TMP="$INSTALL_TMP_DIR" \
OCTOSTORE_TEST_REAL_MV="$REAL_MV" \
OCTOSTORE_VERSION="v$VERSION" \
OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
OCTOSTORE_INSTALL_DIR="$CROSS_INSTALL_DIR" \
TMPDIR="$INSTALL_TMP_DIR" \
  sh "$ROOT/install.sh" >"$SMOKE_DIR/cross-filesystem-install.log"
[[ -x "$CROSS_INSTALL_DIR/octostore" ]]
[[ -x "$CROSS_INSTALL_DIR/octostore-supervisor" ]]
[[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]

# Exercise both elevated directory creation and elevated binary placement without
# requiring real privileges. Root already bypasses mode-based permission checks.
if [[ $(id -u) -ne 0 ]]; then
  PRIV_PARENT="$SMOKE_DIR/restricted"
  PRIV_INSTALL_DIR="$PRIV_PARENT/bin"
  PRIV_EXISTING_INSTALL_DIR="$PRIV_PARENT/existing-bin"
  FAKE_BIN="$SMOKE_DIR/fake-bin"
  PRIVILEGE_LOG="$SMOKE_DIR/privilege.log"
  EXISTING_TARGET_PRIVILEGE_LOG="$SMOKE_DIR/existing-target-privilege.log"
  PRIVILEGE_ROOT_MARKER="$SMOKE_DIR/privileged-owner-root"
  REAL_STAT=$(command -v stat)
  mkdir -p "$PRIV_PARENT" "$FAKE_BIN" "$PRIV_EXISTING_INSTALL_DIR/octostore"
  chmod 500 "$PRIV_EXISTING_INSTALL_DIR"
  chmod 500 "$PRIV_PARENT"
  # These variables must expand when the generated helper runs, not while the fixture writes it.
  # shellcheck disable=SC2016
  printf '%s\n' \
    '#!/bin/sh' \
    'printf "%s\n" "$*" >> "$OCTOSTORE_TEST_PRIVILEGE_LOG"' \
    'operation=$1' \
    'shift' \
    'case "$operation" in' \
    '  install)' \
    '    case "$1" in' \
    '      -d)' \
    '        [ "$2" = -o ] && [ "$3" = root ] && [ "$4" = -m ] && [ "$5" = 0755 ] || exit 64' \
    '        chmod u+w "$OCTOSTORE_TEST_PRIV_PARENT"' \
    '        command install -d -m 0755 "$6"' \
    '        status=$?' \
    '        chmod a-w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '        exit "$status"' \
    '        ;;' \
    '      -o)' \
    '        [ "$2" = root ] && [ "$3" = -m ] && [ "$4" = 0755 ] || exit 64' \
    '        chmod u+w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '        command install -m 0755 "$5" "$6"' \
    '        status=$?' \
    '        chmod a-w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '        exit "$status"' \
    '        ;;' \
    '    esac' \
    '    ;;' \
    '  mktemp)' \
    '    chmod u+w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    command mktemp "$1"' \
    '    status=$?' \
    '    chmod a-w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    exit "$status"' \
    '    ;;' \
    '  mv)' \
    '    chmod u+w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    command mv "$@"' \
    '    status=$?' \
    '    chmod a-w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    [ "$status" -eq 0 ] && : > "$OCTOSTORE_TEST_PRIV_ROOT_MARKER"' \
    '    exit "$status"' \
    '    ;;' \
    '  cp)' \
    '    chmod u+w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    command cp "$@"' \
    '    status=$?' \
    '    chmod a-w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    exit "$status"' \
    '    ;;' \
    '  rm)' \
    '    chmod u+w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    command rm "$@"' \
    '    status=$?' \
    '    chmod a-w "$OCTOSTORE_TEST_PRIV_INSTALL_DIR"' \
    '    exit "$status"' \
    '    ;;' \
    'esac' \
    'exit 64' >"$FAKE_BIN/sudo"
  chmod +x "$FAKE_BIN/sudo"

  # Simulate the ownership metadata that a real privileged install creates.
  # The marker is written only after the fake sudo validates `-o root`.
  # shellcheck disable=SC2016
  printf '%s\n' \
    '#!/bin/sh' \
    'last=' \
    'for arg do last=$arg; done' \
    'case "$last" in' \
    '  "$OCTOSTORE_TEST_PRIV_TARGET"|"$OCTOSTORE_TEST_PRIV_SUPERVISOR_TARGET")' \
    '    if [ -f "$OCTOSTORE_TEST_PRIV_ROOT_MARKER" ]; then printf "0 755\n"; exit 0; fi' \
    '    ;;' \
    'esac' \
    'exec "$OCTOSTORE_TEST_REAL_STAT" "$@"' >"$FAKE_BIN/stat"
  chmod +x "$FAKE_BIN/stat"

  if PATH="$FAKE_BIN:$PATH" \
    OCTOSTORE_TEST_PRIVILEGE_LOG="$EXISTING_TARGET_PRIVILEGE_LOG" \
    OCTOSTORE_TEST_PRIV_TARGET="$PRIV_EXISTING_INSTALL_DIR/octostore" \
    OCTOSTORE_TEST_PRIV_SUPERVISOR_TARGET="$PRIV_EXISTING_INSTALL_DIR/octostore-supervisor" \
    OCTOSTORE_TEST_REAL_STAT="$REAL_STAT" \
    OCTOSTORE_VERSION="v$VERSION" \
    OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
    OCTOSTORE_INSTALL_DIR="$PRIV_EXISTING_INSTALL_DIR" \
    TMPDIR="$INSTALL_TMP_DIR" \
      sh "$ROOT/install.sh" >"$SMOKE_DIR/privileged-existing-target.log" 2>&1; then
    echo "installer accepted an existing octostore directory in a privileged destination" >&2
    exit 1
  fi
  grep -F "Install target exists but is not a regular file: $PRIV_EXISTING_INSTALL_DIR/octostore" \
    "$SMOKE_DIR/privileged-existing-target.log" >/dev/null
  [[ ! -s "$EXISTING_TARGET_PRIVILEGE_LOG" ]]
  [[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]

  PATH="$FAKE_BIN:$PATH" \
  OCTOSTORE_TEST_PRIVILEGE_LOG="$PRIVILEGE_LOG" \
  OCTOSTORE_TEST_PRIV_PARENT="$PRIV_PARENT" \
  OCTOSTORE_TEST_PRIV_INSTALL_DIR="$PRIV_INSTALL_DIR" \
  OCTOSTORE_TEST_PRIV_ROOT_MARKER="$PRIVILEGE_ROOT_MARKER" \
  OCTOSTORE_TEST_PRIV_TARGET="$PRIV_INSTALL_DIR/octostore" \
  OCTOSTORE_TEST_PRIV_SUPERVISOR_TARGET="$PRIV_INSTALL_DIR/octostore-supervisor" \
  OCTOSTORE_TEST_REAL_STAT="$REAL_STAT" \
  OCTOSTORE_VERSION="v$VERSION" \
  OCTOSTORE_DOWNLOAD_BASE="file://$SMOKE_DIR/releases" \
  OCTOSTORE_INSTALL_DIR="$PRIV_INSTALL_DIR" \
  TMPDIR="$INSTALL_TMP_DIR" \
    sh "$ROOT/install.sh" >"$SMOKE_DIR/privileged-install.log"
  [[ -z $(find "$INSTALL_TMP_DIR" -type f -print -quit) ]]

  [[ -x "$PRIV_INSTALL_DIR/octostore" ]]
  [[ -x "$PRIV_INSTALL_DIR/octostore-supervisor" ]]
  [[ -f "$PRIVILEGE_ROOT_MARKER" ]]
  [[ $(sed -n '1p' "$PRIVILEGE_LOG") == "install -d -o root -m 0755 $PRIV_INSTALL_DIR" ]]
  grep -E "^mktemp $PRIV_INSTALL_DIR/octostore\.tmp\.XXXXXX$" "$PRIVILEGE_LOG" >/dev/null
  grep -E "^mktemp $PRIV_INSTALL_DIR/octostore-supervisor\.tmp\.XXXXXX$" "$PRIVILEGE_LOG" >/dev/null
  grep -E "^install -o root -m 0755 .+ $PRIV_INSTALL_DIR/octostore\.tmp\." "$PRIVILEGE_LOG" >/dev/null
  grep -E "^install -o root -m 0755 .+ $PRIV_INSTALL_DIR/octostore-supervisor\.tmp\." "$PRIVILEGE_LOG" >/dev/null
  grep -E "^mv -f $PRIV_INSTALL_DIR/octostore\.tmp\..+ $PRIV_INSTALL_DIR/octostore$" "$PRIVILEGE_LOG" >/dev/null
  grep -E "^mv -f $PRIV_INSTALL_DIR/octostore-supervisor\.tmp\..+ $PRIV_INSTALL_DIR/octostore-supervisor$" "$PRIVILEGE_LOG" >/dev/null
  case "$PLATFORM" in
    linux) PRIVILEGED_MODE=$(stat -c '%a' "$PRIV_INSTALL_DIR/octostore") ;;
    macos) PRIVILEGED_MODE=$(stat -f '%Lp' "$PRIV_INSTALL_DIR/octostore") ;;
  esac
  [[ $PRIVILEGED_MODE == 755 ]]
fi

OCTOSTORE_SMOKE_PORT=$((44000 + ($$ % 2000))) \
OCTOSTORE_SUPERVISOR="$INSTALLED_SUPERVISOR" \
  "$ROOT/scripts/smoke-two-agents.sh" "$INSTALLED"
OCTOSTORE_SMOKE_PORT=$((46000 + ($$ % 2000))) \
  "$ROOT/scripts/smoke-supervisor.sh" "$INSTALLED" "$INSTALLED_SUPERVISOR"

echo "release fixture smoke passed: immutable metadata rejected substituted payload/checksums; transactional installed pair ran server, two-agent, and supervisor paths"
