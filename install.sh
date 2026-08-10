#!/bin/sh
set -e

REPO="${OCTOSTORE_REPO:-octostore/octostore.io}"
BINARY="octostore"
SUPERVISOR_BINARY="octostore-supervisor"
INSTALL_DIR="${OCTOSTORE_INSTALL_DIR:-/usr/local/bin}"
DOWNLOAD_BASE="${OCTOSTORE_DOWNLOAD_BASE:-https://github.com/${REPO}/releases}"
TMP=""
SUPERVISOR_TMP=""
SUMS_TMP=""
RELEASE_METADATA_TMP=""
BINARY_STAGE=""
SUPERVISOR_STAGE=""
BINARY_BACKUP=""
SUPERVISOR_BACKUP=""
BINARY_COMMIT_STARTED=0
SUPERVISOR_COMMIT_STARTED=0
INSTALL_COMMIT_ACTIVE=0
ELEVATED_INSTALL=0
CLEANUP_IN_PROGRESS=0

shell_quote() {
  printf "'"
  printf '%s' "$1" | sed "s/'/'\\\\''/g"
  printf "'"
}

case "$INSTALL_DIR" in
  "") echo "❌ OCTOSTORE_INSTALL_DIR cannot be empty"; exit 1 ;;
  -*) echo "❌ OCTOSTORE_INSTALL_DIR cannot begin with '-'"; exit 1 ;;
esac

install_mv() {
  if [ "$ELEVATED_INSTALL" -eq 1 ]; then
    sudo mv -f "$1" "$2"
  else
    mv -f "$1" "$2"
  fi
}

install_rm() {
  if [ "$ELEVATED_INSTALL" -eq 1 ]; then
    sudo rm -f "$1"
  else
    rm -f "$1"
  fi
}

rollback_install_commit() {
  [ "$INSTALL_COMMIT_ACTIVE" -eq 1 ] || return 0
  rollback_status=0
  echo "❌ Install commit failed; restoring the previous executable pair" >&2
  if [ "$BINARY_COMMIT_STARTED" -eq 1 ]; then
    if [ -n "$BINARY_BACKUP" ]; then
      if install_mv "$BINARY_BACKUP" "$TARGET"; then
        BINARY_BACKUP=""
      else
        echo "❌ Could not restore previous $BINARY" >&2
        rollback_status=1
      fi
    else
      if ! install_rm "$TARGET"; then
        echo "❌ Could not remove interrupted $BINARY" >&2
        rollback_status=1
      fi
    fi
  fi
  if [ "$SUPERVISOR_COMMIT_STARTED" -eq 1 ]; then
    if [ -n "$SUPERVISOR_BACKUP" ]; then
      if install_mv "$SUPERVISOR_BACKUP" "$SUPERVISOR_TARGET"; then
        SUPERVISOR_BACKUP=""
      else
        echo "❌ Could not restore previous $SUPERVISOR_BINARY" >&2
        rollback_status=1
      fi
    else
      if ! install_rm "$SUPERVISOR_TARGET"; then
        echo "❌ Could not remove interrupted $SUPERVISOR_BINARY" >&2
        rollback_status=1
      fi
    fi
  fi
  INSTALL_COMMIT_ACTIVE=0
  return "$rollback_status"
}

cleanup() {
  [ "$CLEANUP_IN_PROGRESS" -eq 0 ] || return 0
  CLEANUP_IN_PROGRESS=1
  rollback_install_commit || :
  if [ -n "$TMP" ]; then rm -f "$TMP" 2>/dev/null || :; fi
  if [ -n "$SUPERVISOR_TMP" ]; then rm -f "$SUPERVISOR_TMP" 2>/dev/null || :; fi
  if [ -n "$SUMS_TMP" ]; then rm -f "$SUMS_TMP" 2>/dev/null || :; fi
  if [ -n "$RELEASE_METADATA_TMP" ]; then rm -f "$RELEASE_METADATA_TMP" 2>/dev/null || :; fi
  if [ "$ELEVATED_INSTALL" -eq 1 ]; then
    [ -z "$BINARY_STAGE" ] || sudo rm -f "$BINARY_STAGE" 2>/dev/null || true
    [ -z "$SUPERVISOR_STAGE" ] || sudo rm -f "$SUPERVISOR_STAGE" 2>/dev/null || true
  else
    [ -z "$BINARY_STAGE" ] || rm -f "$BINARY_STAGE" 2>/dev/null || true
    [ -z "$SUPERVISOR_STAGE" ] || rm -f "$SUPERVISOR_STAGE" 2>/dev/null || true
  fi
  [ -z "$BINARY_BACKUP" ] || install_rm "$BINARY_BACKUP" 2>/dev/null || true
  [ -z "$SUPERVISOR_BACKUP" ] || install_rm "$SUPERVISOR_BACKUP" 2>/dev/null || true
}

signal_exit() {
  signal_status=$1
  # A trapped signal must not return to the interrupted command: that could
  # otherwise print success after cleanup has restored the previous pair.
  trap '' HUP INT TERM
  cleanup
  trap - EXIT
  exit "$signal_status"
}
trap cleanup EXIT
trap 'signal_exit 129' HUP
trap 'signal_exit 130' INT
trap 'signal_exit 143' TERM

# Detect OS and arch
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)  PLATFORM="linux" ;;
  darwin) PLATFORM="macos" ;;
  *)      echo "❌ Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64)  SUFFIX="${PLATFORM}-amd64" ;;
  arm64|aarch64) SUFFIX="${PLATFORM}-arm64" ;;
  *)             echo "❌ Unsupported architecture: $ARCH"; exit 1 ;;
esac

ASSET="octostore-${SUFFIX}"
SUPERVISOR_ASSET="octostore-reference-supervisor.sh"

# Resolve the requested release, or discover the latest stable release.
if [ -n "${OCTOSTORE_VERSION:-}" ]; then
  LATEST="$OCTOSTORE_VERSION"
  case "$LATEST" in
    v*) ;;
    *) LATEST="v${LATEST}" ;;
  esac
  echo "🐙 Installing requested release ${LATEST}..."
else
  echo "🐙 Detecting latest release..."
  LATEST=$(curl -sI "${DOWNLOAD_BASE}/latest" | grep -i "^location:" | sed 's/.*tag\///' | tr -d '\r\n')
fi

if [ -z "$LATEST" ]; then
  echo "❌ Could not find latest release"
  exit 1
fi

RELEASE_METADATA_URL="${OCTOSTORE_RELEASE_METADATA_URL:-https://api.github.com/repos/${REPO}/releases/tags/${LATEST}}"

command -v jq >/dev/null 2>&1 || {
  echo "❌ Release provenance verification requires jq"
  exit 1
}

RELEASE_METADATA_TMP=$(mktemp)
curl -fL --show-error --silent \
  -H 'Accept: application/vnd.github+json' \
  -H 'X-GitHub-Api-Version: 2022-11-28' \
  -o "$RELEASE_METADATA_TMP" \
  "$RELEASE_METADATA_URL" || {
  echo "❌ Could not download independent release metadata"
  exit 1
}

METADATA_TAG=$(jq -er '.tag_name | select(type == "string")' "$RELEASE_METADATA_TMP") || {
  echo "❌ Release metadata omitted a valid tag"
  exit 1
}
METADATA_IMMUTABLE=$(jq -er '.immutable | select(type == "boolean")' "$RELEASE_METADATA_TMP") || {
  echo "❌ Release metadata omitted immutable-release status"
  exit 1
}
if [ "$METADATA_TAG" != "$LATEST" ]; then
  echo "❌ Release metadata tag $METADATA_TAG does not match requested release $LATEST"
  exit 1
fi
if [ "$METADATA_IMMUTABLE" != true ]; then
  echo "❌ Release $LATEST is not immutable; refusing executable assets"
  exit 1
fi

URL="${DOWNLOAD_BASE}/download/${LATEST}/${ASSET}"
echo "📦 Downloading ${ASSET} (${LATEST})..."

TMP=$(mktemp)
SUPERVISOR_TMP=$(mktemp)
SUMS_TMP=$(mktemp)
curl -fL --show-error --silent -o "$TMP" "$URL" || {
  echo "❌ Download failed: $URL"
  exit 1
}
curl -fL --show-error --silent \
  -o "$SUPERVISOR_TMP" \
  "${DOWNLOAD_BASE}/download/${LATEST}/${SUPERVISOR_ASSET}" || {
  echo "❌ Could not download ${SUPERVISOR_ASSET}"
  exit 1
}
curl -fL --show-error --silent \
  -o "$SUMS_TMP" \
  "${DOWNLOAD_BASE}/download/${LATEST}/SHA256SUMS" || {
  echo "❌ Could not download release checksums"
  exit 1
}

EXPECTED=$(awk -v asset="$ASSET" '$2 == asset { print $1 }' "$SUMS_TMP")
SUPERVISOR_EXPECTED=$(awk -v asset="$SUPERVISOR_ASSET" '$2 == asset { print $1 }' "$SUMS_TMP")
if [ -z "$EXPECTED" ]; then
  echo "❌ SHA256SUMS does not contain $ASSET"
  exit 1
fi
if [ -z "$SUPERVISOR_EXPECTED" ]; then
  echo "❌ SHA256SUMS does not contain $SUPERVISOR_ASSET"
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "$TMP" | awk '{print $1}')
  SUPERVISOR_ACTUAL=$(sha256sum "$SUPERVISOR_TMP" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL=$(shasum -a 256 "$TMP" | awk '{print $1}')
  SUPERVISOR_ACTUAL=$(shasum -a 256 "$SUPERVISOR_TMP" | awk '{print $1}')
else
  echo "❌ Checksum verification requires sha256sum or shasum"
  exit 1
fi

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

if [ "$SUPERVISOR_EXPECTED" != "$SUPERVISOR_ACTUAL" ]; then
  echo "❌ Checksum mismatch for $SUPERVISOR_ASSET"
  exit 1
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "❌ Checksum mismatch for $ASSET"
  exit 1
fi

METADATA_BINARY_EXPECTED=$(jq -er --arg asset "$ASSET" '
  [.assets[] | select(.name == $asset) | .digest] |
  if length == 1 then .[0] else error("expected exactly one matching asset") end |
  select(type == "string" and test("^sha256:[0-9a-f]{64}$")) |
  sub("^sha256:"; "")
' "$RELEASE_METADATA_TMP") || {
  echo "❌ Immutable release metadata omitted a unique SHA-256 digest for $ASSET"
  exit 1
}
METADATA_SUPERVISOR_EXPECTED=$(jq -er --arg asset "$SUPERVISOR_ASSET" '
  [.assets[] | select(.name == $asset) | .digest] |
  if length == 1 then .[0] else error("expected exactly one matching asset") end |
  select(type == "string" and test("^sha256:[0-9a-f]{64}$")) |
  sub("^sha256:"; "")
' "$RELEASE_METADATA_TMP") || {
  echo "❌ Immutable release metadata omitted a unique SHA-256 digest for $SUPERVISOR_ASSET"
  exit 1
}
if [ "$METADATA_BINARY_EXPECTED" != "$ACTUAL" ]; then
  echo "❌ Independent release metadata digest mismatch for $ASSET"
  exit 1
fi
if [ "$METADATA_SUPERVISOR_EXPECTED" != "$SUPERVISOR_ACTUAL" ]; then
  echo "❌ Independent release metadata digest mismatch for $SUPERVISOR_ASSET"
  exit 1
fi

chmod 755 "$TMP"
chmod 755 "$SUPERVISOR_TMP"
VERSION=$("$TMP" --version | awk '{print $2}')
EXPECTED_VERSION=${LATEST#v}
if [ "$VERSION" != "$EXPECTED_VERSION" ]; then
  echo "❌ Binary reports $VERSION, expected $EXPECTED_VERSION"
  exit 1
fi
sh -n "$SUPERVISOR_TMP" || {
  echo "❌ Downloaded supervisor is not valid POSIX shell"
  exit 1
}
"$SUPERVISOR_TMP" --check-dependencies || {
  echo "❌ Supervisor runtime dependencies are unavailable; install jq and Perl with Time::HiRes"
  exit 1
}

# Create and validate the destination before installing. Prefer current-user
# access, and ask for elevation only when the selected path requires it.
if { [ -e "$INSTALL_DIR" ] || [ -L "$INSTALL_DIR" ]; } && [ ! -d "$INSTALL_DIR" ]; then
  echo "❌ Install destination exists but is not a directory: $INSTALL_DIR"
  exit 1
fi

if [ ! -d "$INSTALL_DIR" ]; then
  if mkdir -p "$INSTALL_DIR" 2>/dev/null; then
    echo "📁 Created install directory ${INSTALL_DIR}"
  else
    if ! command -v sudo >/dev/null 2>&1; then
      echo "❌ Cannot create ${INSTALL_DIR} and sudo is unavailable"
      echo "   Set OCTOSTORE_INSTALL_DIR to a user-writable directory such as \$HOME/.local/bin"
      exit 1
    fi
    echo "🔑 Creating ${INSTALL_DIR} (needs sudo)..."
    sudo install -d -o root -m 0755 "$INSTALL_DIR"
  fi
fi

if [ ! -d "$INSTALL_DIR" ]; then
  echo "❌ Install destination is not a directory: $INSTALL_DIR"
  exit 1
fi

TARGET="${INSTALL_DIR}/${BINARY}"
SUPERVISOR_TARGET="${INSTALL_DIR}/${SUPERVISOR_BINARY}"
for install_target in "$TARGET" "$SUPERVISOR_TARGET"; do
  if [ -L "$install_target" ]; then
    echo "❌ Install target must not be a symbolic link: $install_target"
    exit 1
  fi
  if [ -e "$install_target" ] && [ ! -f "$install_target" ]; then
    echo "❌ Install target exists but is not a regular file: $install_target"
    exit 1
  fi
done

if [ -w "$INSTALL_DIR" ]; then
  BINARY_STAGE=$(mktemp "${TARGET}.tmp.XXXXXX")
  SUPERVISOR_STAGE=$(mktemp "${SUPERVISOR_TARGET}.tmp.XXXXXX")
  cp "$TMP" "$BINARY_STAGE"
  cp "$SUPERVISOR_TMP" "$SUPERVISOR_STAGE"
  chmod 0755 "$BINARY_STAGE" "$SUPERVISOR_STAGE"
else
  if ! command -v sudo >/dev/null 2>&1; then
    echo "❌ Cannot write to ${INSTALL_DIR} and sudo is unavailable"
    echo "   Set OCTOSTORE_INSTALL_DIR to a user-writable directory such as \$HOME/.local/bin"
    exit 1
  fi
  echo "🔑 Installing to ${INSTALL_DIR} (needs sudo)..."
  ELEVATED_INSTALL=1
  BINARY_STAGE=$(sudo mktemp "${TARGET}.tmp.XXXXXX")
  SUPERVISOR_STAGE=$(sudo mktemp "${SUPERVISOR_TARGET}.tmp.XXXXXX")
  sudo install -o root -m 0755 "$TMP" "$BINARY_STAGE"
  sudo install -o root -m 0755 "$SUPERVISOR_TMP" "$SUPERVISOR_STAGE"
fi

STAGED_VERSION=$("$BINARY_STAGE" --version | awk '{print $2}')
if [ "$STAGED_VERSION" != "$EXPECTED_VERSION" ] || ! sh -n "$SUPERVISOR_STAGE"; then
  echo "❌ Same-filesystem staged install failed verification"
  exit 1
fi

# Preserve the previous pair in same-directory backups. Two directory entries
# cannot be replaced in one POSIX rename, so a failed or trapped second rename
# rolls the first one back before the installer exits.
if [ -e "$TARGET" ]; then
  if [ "$ELEVATED_INSTALL" -eq 1 ]; then
    BINARY_BACKUP=$(sudo mktemp "${TARGET}.backup.XXXXXX")
    sudo cp -p "$TARGET" "$BINARY_BACKUP"
  else
    BINARY_BACKUP=$(mktemp "${TARGET}.backup.XXXXXX")
    cp -p "$TARGET" "$BINARY_BACKUP"
  fi
fi
if [ -e "$SUPERVISOR_TARGET" ]; then
  if [ "$ELEVATED_INSTALL" -eq 1 ]; then
    SUPERVISOR_BACKUP=$(sudo mktemp "${SUPERVISOR_TARGET}.backup.XXXXXX")
    sudo cp -p "$SUPERVISOR_TARGET" "$SUPERVISOR_BACKUP"
  else
    SUPERVISOR_BACKUP=$(mktemp "${SUPERVISOR_TARGET}.backup.XXXXXX")
    cp -p "$SUPERVISOR_TARGET" "$SUPERVISOR_BACKUP"
  fi
fi

interrupt_after_rename_for_test() {
  rename_phase=$1
  [ "${OCTOSTORE_TEST_INTERRUPT_AFTER_RENAME:-}" = "$rename_phase" ] || return 0
  interrupt_signal=${OCTOSTORE_TEST_INTERRUPT_SIGNAL:-TERM}
  case "$interrupt_signal" in
    HUP|INT|TERM) ;;
    *)
      echo "❌ OCTOSTORE_TEST_INTERRUPT_SIGNAL must be HUP, INT, or TERM" >&2
      exit 1
      ;;
  esac
  # This deterministic fixture seam sends a real signal after a successful
  # same-filesystem replacement while rollback is still armed.
  kill -"$interrupt_signal" "$$"
}

INSTALL_COMMIT_ACTIVE=1
SUPERVISOR_COMMIT_STARTED=1
install_mv "$SUPERVISOR_STAGE" "$SUPERVISOR_TARGET"
SUPERVISOR_STAGE=""
interrupt_after_rename_for_test supervisor
BINARY_COMMIT_STARTED=1
install_mv "$BINARY_STAGE" "$TARGET"
BINARY_STAGE=""
interrupt_after_rename_for_test binary

for install_target in "$TARGET" "$SUPERVISOR_TARGET"; do
  if [ -L "$install_target" ] || [ ! -f "$install_target" ] || [ ! -x "$install_target" ]; then
    echo "❌ Installed target is not a regular executable file: $install_target"
    exit 1
  fi
done

INSTALLED_VERSION=$("$TARGET" --version | awk '{print $2}')
INSTALLED_BINARY_ACTUAL=$(sha256_file "$TARGET")
INSTALLED_SUPERVISOR_ACTUAL=$(sha256_file "$SUPERVISOR_TARGET")
if [ "$INSTALLED_VERSION" != "$EXPECTED_VERSION" ] || \
   ! sh -n "$SUPERVISOR_TARGET" || \
   [ "$INSTALLED_BINARY_ACTUAL" != "$ACTUAL" ] || \
   [ "$INSTALLED_SUPERVISOR_ACTUAL" != "$SUPERVISOR_ACTUAL" ]; then
  echo "❌ Installed executable pair failed exact post-commit verification"
  exit 1
fi

if [ "$ELEVATED_INSTALL" -eq 1 ]; then
  case "$PLATFORM" in
    linux)
      INSTALLED_STATE=$(stat -c '%u %a' "$TARGET")
      SUPERVISOR_INSTALLED_STATE=$(stat -c '%u %a' "$SUPERVISOR_TARGET")
      ;;
    macos)
      INSTALLED_STATE=$(stat -f '%u %Lp' "$TARGET")
      SUPERVISOR_INSTALLED_STATE=$(stat -f '%u %Lp' "$SUPERVISOR_TARGET")
      ;;
  esac
  if [ "$INSTALLED_STATE" != "0 755" ] || [ "$SUPERVISOR_INSTALLED_STATE" != "0 755" ]; then
    echo "❌ Privileged install did not produce root-owned 0755 executables"
    exit 1
  fi
fi

# Keep rollback armed through all post-install validation. Only a fully
# verified executable pair retires the backups.
INSTALL_COMMIT_ACTIVE=0

echo "✅ Installed ${BINARY} to ${TARGET}"
echo "✅ Installed ${SUPERVISOR_BINARY} to ${SUPERVISOR_TARGET}"
echo ""
echo "Secure loopback-only first run:"
printf '  OCTOSTORE_BIN='
shell_quote "$TARGET"
printf '\n'
printf '%s\n' \
  "  OCTOSTORE_PORT=\"\${OCTOSTORE_PORT:-3000}\"" \
  "  TOKEN_FILE=\"\${XDG_CONFIG_HOME:-\$HOME/.config}/octostore/static-tokens\"" \
  "  umask 077" \
  "  mkdir -p \"\$(dirname \"\$TOKEN_FILE\")\"" \
  "  TOKEN=\$(LC_ALL=C od -An -N32 -tx1 /dev/urandom | tr -d \" \\n\")" \
  "  printf \"local:%s\\n\" \"\$TOKEN\" > \"\$TOKEN_FILE\"" \
  "  chmod 600 \"\$TOKEN_FILE\"" \
  "  BIND_ADDR=\"127.0.0.1:\$OCTOSTORE_PORT\" STATIC_TOKENS_FILE=\"\$TOKEN_FILE\" \"\$OCTOSTORE_BIN\" serve &" \
  "  OCTOSTORE_PID=\$!" \
  "  curl --fail --silent --show-error --retry 20 --retry-connrefused --retry-delay 1 \"http://127.0.0.1:\$OCTOSTORE_PORT/health\""
