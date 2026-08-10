#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
CHECK="$ROOT/scripts/require-stable-release-tag.sh"

fail() {
  echo "stable release tag fixture failed: $*" >&2
  exit 1
}

[[ -x "$CHECK" ]] || fail "stable-tag helper is not executable"
"$CHECK" v0.14.0 >/dev/null || fail "exact stable tag was rejected"

INVALID_CASES=(
  'rc|v0.14.0-rc.1'
  'preview|v0.14.0-preview.2'
  'nightly|v0.14.0-nightly.20260804'
  'malformed|v0.14'
  'build-suffixed|v0.14.0+build.7'
)

for invalid_case in "${INVALID_CASES[@]}"; do
  label=${invalid_case%%|*}
  invalid_tag=${invalid_case#*|}
  set +e
  output=$("$CHECK" "$invalid_tag" 2>&1)
  status=$?
  set -e
  [[ $status -eq 64 ]] || fail "$label tag exited $status instead of 64: $invalid_tag"
  grep -Fq 'release tag must match ^v[0-9]+\.[0-9]+\.[0-9]+$' <<<"$output" \
    || fail "$label rejection did not identify the exact stable-tag contract"
done

echo "stable release tag fixture passed: rc, preview, nightly, malformed, and build-suffixed tags rejected"
