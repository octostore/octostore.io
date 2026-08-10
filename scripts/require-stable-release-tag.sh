#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 RELEASE_TAG" >&2
  exit 64
fi

TAG=$1
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "ERROR: release tag must match ^v[0-9]+\.[0-9]+\.[0-9]+$: $TAG" >&2
  exit 64
fi

echo "stable release tag validated: $TAG"
