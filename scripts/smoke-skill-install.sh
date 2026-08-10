#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
INSTALL_FIXTURE=$(mktemp -d "$ROOT/.smoke-skill-install.XXXXXX")
SKILLS_BIN="$ROOT/node_modules/.bin/skills"
SKILL_SOURCE=${OCTOSTORE_SKILL_SOURCE:-$ROOT}

cleanup() {
  rm -rf "$INSTALL_FIXTURE"
}
trap cleanup EXIT INT TERM

[[ -x "$SKILLS_BIN" ]] || {
  echo "locked skills CLI is unavailable; run npm ci before the skill-install smoke" >&2
  exit 64
}

(
  cd "$INSTALL_FIXTURE"
  CI=1 "$SKILLS_BIN" add "$SKILL_SOURCE" \
    --skill octostore --agent codex -y
)

mapfile -t INSTALLED_SKILLS < <(
  find "$INSTALL_FIXTURE" \( -type f -o -type l \) -path '*/octostore/SKILL.md' -print
)
[[ "${#INSTALLED_SKILLS[@]}" -ge 1 ]] || {
  echo "skills CLI exited successfully without installing octostore/SKILL.md" >&2
  exit 70
}

for installed_skill in "${INSTALLED_SKILLS[@]}"; do
  cmp "$ROOT/skills/octostore/SKILL.md" "$installed_skill"
done

echo "skill install smoke passed: pinned skills CLI installed octostore unattended"
