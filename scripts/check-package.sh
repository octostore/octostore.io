#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

PACKAGE_LIST=$(mktemp "${TMPDIR:-/tmp}/octostore-package-list.XXXXXX")
EXPECTED_LIST=$(mktemp "${TMPDIR:-/tmp}/octostore-package-expected.XXXXXX")
ARCHIVE_LIST=$(mktemp "${TMPDIR:-/tmp}/octostore-archive-list.XXXXXX")
PUBLISH_LOG=$(mktemp "${TMPDIR:-/tmp}/octostore-publish-dry-run.XXXXXX")
cleanup() {
  rm -f "$PACKAGE_LIST" "$EXPECTED_LIST" "$ARCHIVE_LIST" "$PUBLISH_LOG"
}
trap cleanup EXIT INT TERM

fail() {
  echo "package contract failed: $*" >&2
  exit 1
}

VERSION=$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "invalid package version"

cargo package --list --allow-dirty --locked >"$PACKAGE_LIST"
printf '%s\n' \
  .cargo_vcs_info.json \
  Cargo.lock \
  Cargo.toml \
  Cargo.toml.orig \
  LICENSE-MIT \
  README.md \
  benches/lock_benchmarks.rs \
  install.sh \
  openapi.yaml \
  scripts/reference-supervisor.sh \
  scripts/smoke-release-fixture.sh \
  scripts/smoke-supervisor.sh \
  scripts/smoke-two-agents.sh \
  scripts/two-agent-supervised-demo.sh \
  site/agents/SKILL.md \
  site/assets/octostore-logo.svg \
  skills/octostore/SKILL.md \
  src/app.rs \
  src/auth.rs \
  src/bin/benchmark.rs \
  src/bin/integration_test.rs \
  src/cli.rs \
  src/config.rs \
  src/elections.rs \
  src/error.rs \
  src/lib.rs \
  src/locks.rs \
  src/main.rs \
  src/metrics.rs \
  src/models.rs \
  src/rate_limit.rs \
  src/sessions.rs \
  src/store.rs \
  src/webhooks.rs \
  tests/cli_e2e.rs \
  tests/openapi_correspondence.rs \
  tests/skill_contract.rs \
  >"$EXPECTED_LIST"
LC_ALL=C sort -o "$PACKAGE_LIST" "$PACKAGE_LIST"
LC_ALL=C sort -o "$EXPECTED_LIST" "$EXPECTED_LIST"
diff -u "$EXPECTED_LIST" "$PACKAGE_LIST" \
  || fail "package list differs from the exact allowlist"

set +e
cargo publish --dry-run --allow-dirty --locked 2>&1 | tee "$PUBLISH_LOG"
PUBLISH_STATUS=${PIPESTATUS[0]}
set -e

if [[ "$PUBLISH_STATUS" -ne 0 ]]; then
  echo "cargo publish dry run failed" >&2
  exit "$PUBLISH_STATUS"
fi
UNEXPECTED_WARNINGS=$(grep -E '^warning:' "$PUBLISH_LOG" \
  | grep -Fv 'warning: aborting upload due to dry run' || true)
if [[ -n "$UNEXPECTED_WARNINGS" ]]; then
  printf '%s\n' "$UNEXPECTED_WARNINGS" >&2
  echo "cargo publish dry run emitted unexpected packaging warnings" >&2
  exit 1
fi

CRATE="target/package/octostore-$VERSION.crate"
[[ -f $CRATE ]] || fail "cargo publish dry run did not create $CRATE"
ARCHIVE_ROOT="octostore-$VERSION/"
while IFS= read -r entry; do
  [[ $entry == "$ARCHIVE_ROOT"* ]] \
    || fail "archive path is outside its versioned root: $entry"
  relative=${entry#"$ARCHIVE_ROOT"}
  [[ -n $relative ]] || fail "archive contains an unexpected root entry"
  printf '%s\n' "$relative"
done < <(tar -tzf "$CRATE") >"$ARCHIVE_LIST"

LC_ALL=C sort -o "$ARCHIVE_LIST" "$ARCHIVE_LIST"
diff -u "$PACKAGE_LIST" "$ARCHIVE_LIST" \
  || fail "generated archive differs from the reviewed package list"

echo "package contract passed: strict allowlist, exact archive contents, and warning-free publish dry run"
