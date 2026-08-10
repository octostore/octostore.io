#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "crates.io package verification failed: $*" >&2
  exit 1
}

sha256_file() {
  local path=$1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    shasum -a 256 "$path" | awk '{print $1}'
  fi
}

verify_response() {
  local status=$1
  local version=$2
  local response_file=$3
  local crate_file=$4
  local expected_checksum published_version published_checksum yanked

  [[ -f $crate_file ]] || fail "candidate crate is missing: $crate_file"
  expected_checksum=$(sha256_file "$crate_file")

  case "$status" in
    404)
      printf 'absent\n'
      return
      ;;
    200)
      ;;
    *)
      fail "version lookup returned HTTP $status"
      ;;
  esac

  published_version=$(jq -er '.version.num | select(type == "string")' "$response_file") \
    || fail "metadata omitted a valid version number"
  published_checksum=$(jq -er '.version.checksum | select(type == "string")' "$response_file") \
    || fail "metadata omitted a valid checksum"
  jq -e '.version.yanked | type == "boolean"' "$response_file" >/dev/null \
    || fail "metadata omitted a valid yanked flag"
  yanked=$(jq -r '.version.yanked' "$response_file")

  [[ $published_version == "$version" ]] \
    || fail "metadata version does not match the release version"
  [[ $published_checksum =~ ^[0-9a-f]{64}$ ]] \
    || fail "metadata checksum is malformed"
  [[ $yanked == false ]] || fail "existing crate version is yanked"
  [[ $published_checksum == "$expected_checksum" ]] \
    || fail "existing crate checksum does not match the exact candidate archive"
  printf 'exact\n'
}

usage() {
  echo "usage: $0 live VERSION CRATE | response HTTP_STATUS VERSION METADATA_JSON CRATE" >&2
  exit 64
}

[[ $# -ge 1 ]] || usage
mode=$1
shift

case "$mode" in
  response)
    [[ $# -eq 4 ]] || usage
    verify_response "$1" "$2" "$3" "$4"
    ;;
  live)
    [[ $# -eq 2 ]] || usage
    version=$1
    crate_file=$2
    [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "release version is not stable semver"
    command -v curl >/dev/null || fail "curl is unavailable"
    command -v jq >/dev/null || fail "jq is unavailable"
    response_file=$(mktemp "${TMPDIR:-/tmp}/octostore-crates-io-response.XXXXXX")
    cleanup() {
      rm -f "$response_file"
    }
    trap cleanup EXIT INT TERM
    status=$(curl --retry 3 --retry-all-errors --silent --show-error \
      --user-agent 'octostore-release-check (+https://github.com/octostore/octostore.io)' \
      --output "$response_file" --write-out '%{http_code}' \
      "https://crates.io/api/v1/crates/octostore/$version")
    verify_response "$status" "$version" "$response_file" "$crate_file"
    ;;
  *)
    usage
    ;;
esac
