#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
FIXTURE_ROOT=$(mktemp -d "$ROOT/.crates-io-resume-fixture.XXXXXX")
VERIFY="$ROOT/scripts/verify-crates-io-package.sh"
VERSION=0.14.0
CRATE="$FIXTURE_ROOT/octostore-$VERSION.crate"

cleanup() {
  rm -rf "$FIXTURE_ROOT"
}
trap cleanup EXIT INT TERM

fail() {
  echo "crates.io resume fixture failed: $*" >&2
  exit 1
}

printf 'deterministic candidate crate fixture\n' >"$CRATE"
if command -v sha256sum >/dev/null 2>&1; then
  CHECKSUM=$(sha256sum "$CRATE" | awk '{print $1}')
else
  CHECKSUM=$(shasum -a 256 "$CRATE" | awk '{print $1}')
fi
MISMATCH_CHECKSUM=0000000000000000000000000000000000000000000000000000000000000000

jq -n --arg version "$VERSION" --arg checksum "$CHECKSUM" \
  '{version:{num:$version,checksum:$checksum,yanked:false}}' \
  >"$FIXTURE_ROOT/matching.json"
jq -n --arg version "$VERSION" --arg checksum "$MISMATCH_CHECKSUM" \
  '{version:{num:$version,checksum:$checksum,yanked:false}}' \
  >"$FIXTURE_ROOT/mismatching.json"
jq -n --arg version "$VERSION" --arg checksum "$CHECKSUM" \
  '{version:{num:$version,checksum:$checksum,yanked:true}}' \
  >"$FIXTURE_ROOT/yanked.json"
printf '{}\n' >"$FIXTURE_ROOT/absent.json"

[[ $($VERIFY response 200 "$VERSION" "$FIXTURE_ROOT/matching.json" "$CRATE") == exact ]] \
  || fail "matching checksum was not accepted"
[[ $($VERIFY response 404 "$VERSION" "$FIXTURE_ROOT/absent.json" "$CRATE") == absent ]] \
  || fail "absent version was not accepted for initial publication"

if $VERIFY response 200 "$VERSION" "$FIXTURE_ROOT/mismatching.json" "$CRATE" \
  >"$FIXTURE_ROOT/mismatch.out" 2>"$FIXTURE_ROOT/mismatch.err"; then
  fail "mismatching checksum was accepted"
fi
if $VERIFY response 200 "$VERSION" "$FIXTURE_ROOT/yanked.json" "$CRATE" \
  >"$FIXTURE_ROOT/yanked.out" 2>"$FIXTURE_ROOT/yanked.err"; then
  fail "yanked version was accepted"
fi

echo "crates.io resume fixture passed: absent, exact, mismatched, and yanked versions"
