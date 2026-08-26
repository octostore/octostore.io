#!/usr/bin/env bash
# shellcheck disable=SC2016 # Contract strings intentionally contain unevaluated workflow expressions.
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

require_text() {
  local file=$1
  local text=$2
  grep -Fq -- "$text" "$file" || {
    echo "$file: missing release contract: $text" >&2
    exit 1
  }
}

extract_workflow_run() {
  local workflow=$1
  local step_name=$2
  local output=$3
  awk -v wanted="      - name: $step_name" '
    $0 == wanted { found = 1; next }
    found && $0 == "        run: |" { capture = 1; next }
    capture {
      if ($0 ~ /^          /) {
        print substr($0, 11)
      } else if ($0 ~ /^[[:space:]]*$/) {
        print ""
      } else {
        exit
      }
    }
  ' "$workflow" >"$output"
  [[ -s "$output" ]] || {
    echo "$workflow: could not extract run block for step: $step_name" >&2
    exit 1
  }
}

require_immutable_actions() {
  local file=$1
  local line
  local ref

  while IFS= read -r line; do
    ref=${line##*@}
    ref=${ref%% *}
    if [[ ! "$ref" =~ ^[0-9a-f]{40}$ ]]; then
      echo "$file: action is not pinned to an immutable commit: $line" >&2
      exit 1
    fi
  done < <(grep -E '^[[:space:]]*-[[:space:]]+uses:' "$file")
}

while IFS= read -r workflow; do
  require_immutable_actions "$workflow"
done < <(find .github/workflows -type f \( -name '*.yml' -o -name '*.yaml' \) | LC_ALL=C sort)

# Monitoring is deliberately outside GitHub Actions.  Keep the standalone
# monitor's local-state and branch-write protections from being weakened.
if [[ -e .github/workflows/monitoring.yml || -e .github/workflows/daily-report.yml ]]; then
  echo "scheduled monitoring must not execute in GitHub Actions" >&2
  exit 1
fi
require_text scripts/uptime_monitor.py 'fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)'
require_text scripts/uptime_monitor.py 'unsafe metrics target'
require_text scripts/uptime_monitor.py 'target.is_symlink()'
require_text scripts/uptime_monitor.py 'git", "reset", "--hard", "origin/monitoring-data"'
require_text scripts/uptime_monitor.py 'git", "push", "origin", "monitoring-data"'
require_text scripts/uptime_monitor.py 'for attempt in range(3):'

require_text .github/workflows/release.yml 'permissions: {}'
require_text .github/workflows/release-dispatch.yml 'workflow_run:'
require_text .github/workflows/release-dispatch.yml 'workflows: [CI]'
require_text .github/workflows/release-dispatch.yml "github.event.workflow_run.conclusion == 'success'"
require_text .github/workflows/release-dispatch.yml "github.event.workflow_run.event == 'push'"
require_text .github/workflows/release-dispatch.yml "github.event.workflow_run.head_branch == 'main'"
require_text .github/workflows/release-dispatch.yml 'RELEASE_SHA: ${{ github.event.workflow_run.head_sha }}'
require_text .github/workflows/release-dispatch.yml '! "$RELEASE_SHA" =~ ^[0-9a-f]{40}$'
require_text .github/workflows/release-dispatch.yml 'if [[ "$MAIN_SHA" != "$RELEASE_SHA" ]]'
require_text .github/workflows/release-dispatch.yml "git config user.name 'github-actions[bot]'"
require_text .github/workflows/release-dispatch.yml "git config user.email '41898282+github-actions[bot]@users.noreply.github.com'"
require_text .github/workflows/release-dispatch.yml 'scripts/require-stable-release-tag.sh "$RELEASE_TAG"'
require_text .github/workflows/release-dispatch.yml 'git tag -a "$RELEASE_TAG" "$RELEASE_SHA"'
require_text .github/workflows/release-dispatch.yml 'git push origin "refs/tags/$RELEASE_TAG"'
require_text .github/workflows/release-dispatch.yml 'event_type=stable-release'
require_text .github/workflows/release-dispatch.yml 'EVIDENCE trusted_release_continuation'
require_text .github/workflows/release-dispatch.yml 'EVIDENCE stable_release_dispatched'
if grep -Fq '${{ secrets.' .github/workflows/release-dispatch.yml; then
  echo ".github/workflows/release-dispatch.yml: the unprotected dispatcher must not access secrets" >&2
  exit 1
fi
if [[ $(grep -c '^    permissions:' .github/workflows/release.yml) -ne 6 ]]; then
  echo ".github/workflows/release.yml: every release job must declare minimal permissions" >&2
  exit 1
fi
require_text .github/workflows/release.yml 'node-version: 24.11.1'
require_text .github/workflows/release.yml 'toolchain: 1.92.0'
require_text .github/workflows/release.yml 'npm ci --ignore-scripts --no-audit --no-fund'
require_text .github/workflows/release.yml './node_modules/.bin/playwright install --with-deps chromium'
if [[ $(grep -cF 'repos/$GITHUB_REPOSITORY/immutable-releases' .github/workflows/release.yml) -lt 3 ]]; then
  echo ".github/workflows/release.yml: immutable-release policy must be checked before work, draft creation, and publication" >&2
  exit 1
fi
if [[ $(grep -cF 'OCTOSTORE_RELEASE_POLICY_TOKEN' .github/workflows/release.yml) -lt 3 ]]; then
  echo ".github/workflows/release.yml: every immutable-release policy check requires the admin-read policy token" >&2
  exit 1
fi
require_text .github/workflows/release.yml 'GH_TOKEN="$RELEASE_POLICY_TOKEN"'
require_text .github/workflows/release.yml 'refusing to update an already-public release'
require_text .github/workflows/release.yml 'expected a private draft immediately before publication'
require_text .github/workflows/release.yml 'cleanup_failed_publication()'
require_text .github/workflows/release.yml 'published release is not immutable'
require_text .github/workflows/release.yml 'gh release delete "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --yes'
require_text .github/workflows/release.yml 'public mutable release still exists after cleanup'
require_text .github/workflows/release.yml 'EVIDENCE immutable_release_verified'
require_text .github/workflows/ci.yml 'node-version: 24.11.1'
require_text .github/workflows/ci.yml 'toolchain: 1.92.0'
require_text .github/workflows/ci.yml 'npm ci --ignore-scripts --no-audit --no-fund'
require_text .github/workflows/ci.yml './node_modules/.bin/playwright install --with-deps chromium'
require_text scripts/ci-local.sh 'npm ci --ignore-scripts --no-audit --no-fund'
require_text scripts/ci-local.sh './node_modules/.bin/playwright install chromium'
require_text scripts/ci-local.sh 'go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.12'
require_text scripts/ci-local.sh 'npm run test:site:browser'
require_text rust-toolchain.toml 'channel = "1.92.0"'
require_text package.json '"@playwright/test": "1.62.1"'
require_text package.json '"@redocly/cli": "2.44.1"'
require_text package.json '"html-validate": "10.10.0"'
require_text package.json '"skills": "1.5.21"'
require_text package-lock.json '"integrity": "sha512-'
require_text scripts/smoke-skill-install.sh 'SKILLS_BIN="$ROOT/node_modules/.bin/skills"'
require_text scripts/smoke-skill-install.sh 'SKILL_SOURCE=${OCTOSTORE_SKILL_SOURCE:-$ROOT}'
require_text scripts/smoke-skill-install.sh 'CI=1 "$SKILLS_BIN" add "$SKILL_SOURCE"'

node <<'NODE'
const fs = require('node:fs');
const lock = JSON.parse(fs.readFileSync('package-lock.json', 'utf8'));
const rootVersion = lock.packages?.['']?.devDependencies?.skills;
const locked = lock.packages?.['node_modules/skills'];
if (rootVersion !== '1.5.21' || locked?.version !== '1.5.21') {
  throw new Error('package-lock.json must lock skills exactly at 1.5.21');
}
if (locked.integrity !== 'sha512-CJ4wx692UkQAW+DLpjJg/ww6dJBojq5E8sQBOqP639GutO72v4EFiV/fq1etW2r9NhM/mwaIq8YoqKFJ9XV7ng==') {
  throw new Error('package-lock.json must bind skills 1.5.21 to its reviewed SHA-512 integrity');
}
if (locked.bin?.skills !== 'bin/cli.mjs') {
  throw new Error('package-lock.json must expose the reviewed local skills binary');
}
NODE

release_owned_sources=(
  .github/workflows/ci.yml
  .github/workflows/deploy.yml
  .github/workflows/release.yml
)
while IFS= read -r shell_source; do
  release_owned_sources+=("$shell_source")
done < <(
  find . \
    \( -path './.git' -o -path './node_modules' -o -path './target' \) -prune -o \
    -type f -name '*.sh' -print | LC_ALL=C sort
)
runner_a=$(printf '%s%s' n px)
runner_b=$(printf '%s%s' n pm)
runner_c=$(printf '%s%s' pn pm)
runner_d=$(printf '%s%s' ya rn)
runner_e=$(printf '%s%s' bu nx)
node_tool_bypass_pattern="${runner_a}|${runner_b}[[:space:]]+(--[^[:space:]]+[[:space:]]+)*(exec|x)([[:space:]]|$)|${runner_c}[[:space:]]+dlx|${runner_d}[[:space:]]+dlx|${runner_e}"
if bypasses=$(grep -EnH -- "$node_tool_bypass_pattern" "${release_owned_sources[@]}"); then
  printf '%s\n' "$bypasses" >&2
  echo "release-owned shell must execute only tools from the committed npm integrity graph" >&2
  exit 1
fi

# Secret-bearing release execution must load workflow code from the trusted
# default branch. The dispatch tag is data, not executable authority, and must
# resolve exactly to that default-branch SHA before any secret is referenced.
require_text .github/workflows/release.yml 'repository_dispatch:'
require_text .github/workflows/release.yml 'types: [stable-release]'
require_text .github/workflows/release.yml 'RELEASE_TAG: ${{ github.event.client_payload.tag }}'
require_text .github/workflows/release.yml 'Establish trusted default-branch release authority'
require_text .github/workflows/release.yml '"$GITHUB_EVENT_NAME" != repository_dispatch'
require_text .github/workflows/release.yml '"$GITHUB_REF" != refs/heads/main'
require_text .github/workflows/release.yml 'not trusted main SHA $GITHUB_SHA.'
require_text .github/workflows/release.yml 'EVIDENCE trusted_default_branch_release'
if grep -Eq '^[[:space:]]+push:|tags:[[:space:]]*\[' .github/workflows/release.yml; then
  echo ".github/workflows/release.yml: secret-bearing releases must not execute from tag-push workflow code" >&2
  exit 1
fi
trust_line=$(grep -nF 'EVIDENCE trusted_default_branch_release' .github/workflows/release.yml \
  | head -n 1 | cut -d: -f1)
first_secret_line=$(grep -nF '${{ secrets.' .github/workflows/release.yml \
  | head -n 1 | cut -d: -f1)
if [[ -z $trust_line || -z $first_secret_line || $trust_line -ge $first_secret_line ]]; then
  echo ".github/workflows/release.yml: trusted main/tag binding must precede every secret reference" >&2
  exit 1
fi

# Stable releases and manual redeploys both resolve an immutable commit. The
# deploy host independently checks the same ancestry before touching live state.
require_text .github/workflows/release.yml 'git merge-base --is-ancestor "$TAG_COMMIT" origin/main'
require_text .github/workflows/release.yml 'ref: ${{ github.sha }}'
require_text .github/workflows/release.yml 'refs/tags/$RELEASE_TAG:refs/tags/$RELEASE_TAG'
require_text .github/workflows/release.yml 'TAG_COMMIT="$(git rev-parse "refs/tags/$RELEASE_TAG^{commit}")"'
require_text .github/workflows/release.yml 'remote tag $RELEASE_TAG resolves to $TAG_COMMIT, not event SHA $GITHUB_SHA.'
require_text .github/workflows/release.yml 'git checkout --detach "$GITHUB_SHA"'
require_text .github/workflows/release.yml 'test "$(git rev-parse HEAD)" = "$GITHUB_SHA"'
require_text .github/workflows/release.yml 'target_commitish: ${{ github.sha }}'
require_text .github/workflows/release.yml "--json targetCommitish --jq '.targetCommitish'"
require_text .github/workflows/release.yml 'test "$RELEASE_TARGET_COMMIT" = "$GITHUB_SHA"'
require_text scripts/require-stable-release-tag.sh '[[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]'
require_text .github/workflows/release.yml 'scripts/require-stable-release-tag.sh "$RELEASE_TAG"'
require_text .github/workflows/release.yml 'scripts/require-stable-release-tag.sh "${{ github.event.client_payload.tag }}"'
require_text .github/workflows/deploy.yml 'scripts/require-stable-release-tag.sh "$RELEASE_TAG"'
require_text .github/workflows/release.yml 'EXPECTED_COMMIT="$(git rev-parse HEAD)"'
require_text .github/workflows/release.yml '"$EXPECTED_PREVIOUS_ASSET_SHA" < deploy/deploy.sh'
require_text .github/workflows/release.yml $'    environment:\n      name: production'
require_text .github/workflows/deploy.yml $'    environment:\n      name: production'
require_text .github/workflows/release.yml 'deployments: write'
require_text .github/workflows/deploy.yml 'deployments: write'
if grep -Fq 'actions: read' .github/workflows/release.yml .github/workflows/deploy.yml; then
  echo "production workflows must not request unused actions: read permission" >&2
  exit 1
fi
require_text .github/workflows/release.yml 'deployments?environment=production&per_page=100'
require_text .github/workflows/deploy.yml 'deployments?environment=production&per_page=100'
require_text .github/workflows/release.yml 'production_deployment_bootstrap tag=$PREVIOUS_TAG'
require_text .github/workflows/deploy.yml 'production_deployment_bootstrap tag=$PREVIOUS_TAG'
require_text .github/workflows/release.yml 'gh attestation verify "$PREVIOUS_ASSET"'
require_text .github/workflows/deploy.yml 'gh attestation verify "$PREVIOUS_ASSET"'
require_text .github/workflows/release.yml 'production_deployment_recorded id=$DEPLOYMENT_ID tag=$RELEASE_TAG'
require_text .github/workflows/deploy.yml 'production_deployment_recorded id=$DEPLOYMENT_ID tag=$RELEASE_TAG'
if grep -Fq 'https://api.octostore.io/openapi.yaml' \
  .github/workflows/release.yml .github/workflows/deploy.yml; then
  echo "hosted deploy workflows must not depend on live OpenAPI before durable recovery" >&2
  exit 1
fi

early_tag_line=$(grep -nF 'scripts/require-stable-release-tag.sh "$RELEASE_TAG"' \
  .github/workflows/release.yml | head -n 1 | cut -d: -f1)
ancestry_line=$(grep -nF 'git merge-base --is-ancestor "$TAG_COMMIT" origin/main' \
  .github/workflows/release.yml | head -n 1 | cut -d: -f1)
if [[ -z $early_tag_line || -z $ancestry_line || $early_tag_line -ge $ancestry_line ]]; then
  echo ".github/workflows/release.yml: exact stable-tag validation must be the first release gate after checkout" >&2
  exit 1
fi

if [[ $(grep -c '^    environment:' .github/workflows/release.yml) -ne 3 ]]; then
  echo ".github/workflows/release.yml: publish, finalize, and deploy must each bind the protected production environment" >&2
  exit 1
fi
for job in publish finalize deploy; do
  if ! awk -v wanted="  $job:" '
    $0 == wanted { in_job = 1; next }
    in_job && /^  [a-zA-Z0-9_-]+:/ { exit !found }
    in_job && $0 == "    environment:" { saw_environment = 1 }
    in_job && saw_environment && $0 == "      name: production" { found = 1 }
    END { exit !found }
  ' .github/workflows/release.yml; then
    echo ".github/workflows/release.yml: $job must be reviewer-gated by production" >&2
    exit 1
  fi
done
require_text .github/workflows/release.yml 'Require protected production environment before publication'
require_text .github/workflows/release.yml 'Require protected production environment before immutable publication'
require_text .github/workflows/release.yml 'release_publication_required_reviewers_verified'
require_text .github/workflows/release.yml 'immutable_publication_required_reviewers_verified'

for boundary in \
  'Require exact tag/SHA binding before build' \
  'Require exact tag/SHA binding before draft creation' \
  'Publish to crates.io'
do
  boundary_line=$(grep -nF -- "- name: $boundary" .github/workflows/release.yml | head -n 1 | cut -d: -f1)
  if [[ -z $boundary_line ]]; then
    echo ".github/workflows/release.yml: missing exact tag/SHA gate for $boundary" >&2
    exit 1
  fi
done

draft_gate_line=$(grep -nF -- '- name: Require exact tag/SHA binding before draft creation' \
  .github/workflows/release.yml | head -n 1 | cut -d: -f1)
draft_create_line=$(grep -nF -- '- name: Create GitHub Release' \
  .github/workflows/release.yml | head -n 1 | cut -d: -f1)
if [[ -z $draft_gate_line || -z $draft_create_line || $draft_gate_line -ge $draft_create_line ]]; then
  echo ".github/workflows/release.yml: draft creation must follow an exact tag/SHA gate" >&2
  exit 1
fi

deploy_tag_line=$(grep -nF 'scripts/require-stable-release-tag.sh "${{ github.event.client_payload.tag }}"' \
  .github/workflows/release.yml | tail -n 1 | cut -d: -f1)
deploy_ssh_line=$(grep -nF 'ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=yes' \
  .github/workflows/release.yml | tail -n 1 | cut -d: -f1)
if [[ -z $deploy_tag_line || -z $deploy_ssh_line \
|| $deploy_ssh_line -le $deploy_tag_line \
|| $((deploy_ssh_line - deploy_tag_line)) -gt 8 ]]; then
  echo ".github/workflows/release.yml: exact stable-tag validation must run immediately before production SSH" >&2
  exit 1
fi

# Job environments are the authorization boundary; Deployment API records are
# only post-deploy audit history. The environment policy must be checked before
# repository-to-environment secret migration, SSH material, or production
# mutation can begin.
for workflow in .github/workflows/release.yml .github/workflows/deploy.yml; do
  require_text "$workflow" 'RELEASE_POLICY_TOKEN: ${{ secrets.OCTOSTORE_RELEASE_POLICY_TOKEN }}'
  require_text "$workflow" 'if [[ -z "$RELEASE_POLICY_TOKEN" ]]'
  require_text "$workflow" 'GH_TOKEN="$RELEASE_POLICY_TOKEN" gh api "repos/$GITHUB_REPOSITORY/environments/production"'
  require_text "$workflow" 'MIGRATION_TOKEN: ${{ secrets.OCTOSTORE_MIGRATION_TOKEN }}'
  require_text "$workflow" 'if [[ -z "$MIGRATION_TOKEN" ]]'
  require_text "$workflow" 'GH_TOKEN="$MIGRATION_TOKEN" gh api'
  require_text "$workflow" 'OCTOSTORE_MIGRATION_TOKEN with production secret-migration write access is required'
  require_text "$workflow" 'repos/$GITHUB_REPOSITORY/environments/production'
  require_text "$workflow" 'production_environment_required_reviewers_verified'
  require_text "$workflow" 'required_reviewers'
  require_text "$workflow" '.prevent_self_review == true'
  require_text "$workflow" '.deployment_branch_policy.protected_branches == true'
  require_text "$workflow" '.deployment_branch_policy.custom_branch_policies == false'
  require_text "$workflow" 'require non-self review and protected-branch-only deployment'
  require_text "$workflow" 'DEPLOY_HOST_VALUE: ${{ secrets.DEPLOY_HOST }}'
  require_text "$workflow" 'DEPLOY_SSH_KEY_VALUE: ${{ secrets.DEPLOY_SSH_KEY }}'
  require_text "$workflow" 'DEPLOY_SSH_KNOWN_HOSTS_VALUE: ${{ secrets.DEPLOY_SSH_KNOWN_HOSTS }}'
  require_text "$workflow" 'repos/$GITHUB_REPOSITORY/actions/secrets?per_page=100'
  require_text "$workflow" 'repos/$GITHUB_REPOSITORY/environments/production/secrets?per_page=100'
  require_text "$workflow" 'gh secret set "$SECRET_NAME" --env production --repo "$GITHUB_REPOSITORY"'
  require_text "$workflow" 'repos/$GITHUB_REPOSITORY/actions/secrets/$SECRET_NAME'
  require_text "$workflow" 'repository rollback copies were retained'
  require_text "$workflow" 'production_ssh_secrets_already_environment_scoped'
  require_text "$workflow" 'production_ssh_secrets_migrated_to_environment'
  environment_line=$(grep -nF '      - name: Require protected production environment' "$workflow" \
    | tail -n 1 | cut -d: -f1)
  migration_line=$(grep -nF '      - name: Migrate production SSH secrets' "$workflow" \
    | cut -d: -f1)
  ssh_setup_line=$(grep -nF '      - name: Set up SSH' "$workflow" | cut -d: -f1)
  verify_line=$(grep -nF 'repository rollback copies were retained' "$workflow" | cut -d: -f1)
  delete_line=$(grep -nF 'if ! GH_TOKEN="$MIGRATION_TOKEN" gh api --method DELETE' "$workflow" \
    | cut -d: -f1)
  if [[ -z $environment_line || -z $migration_line || -z $ssh_setup_line \
    || $environment_line -ge $migration_line || $migration_line -ge $ssh_setup_line ]]; then
    echo "$workflow: approval preflight, secret migration, and SSH setup are out of order" >&2
    exit 1
  fi
  if [[ -z $verify_line || -z $delete_line || $verify_line -ge $delete_line ]]; then
    echo "$workflow: environment secret verification must precede repository secret deletion" >&2
    exit 1
  fi
done

if grep -Fq 'MIGRATION_TOKEN: ${{ secrets.OCTOSTORE_MIGRATION_TOKEN }}' \
  <(sed -n '1,/^  publish:/p' .github/workflows/release.yml); then
  echo ".github/workflows/release.yml: mutation-capable migration authority is forbidden before reviewer-gated publication" >&2
  exit 1
fi
for workflow in .github/workflows/release.yml .github/workflows/deploy.yml; do
  if awk '
    $0 == "      - name: Migrate production SSH secrets" { found = 1; next }
    found && $0 == "        run: |" { capture = 1; next }
    capture && $0 ~ /^          / { print substr($0, 11); next }
    capture && $0 !~ /^[[:space:]]*$/ { exit }
  ' "$workflow" | grep -Fq 'RELEASE_POLICY_TOKEN'; then
    echo "$workflow: read-only policy credential must not perform secret migration" >&2
    exit 1
  fi
done

require_text .github/workflows/release.yml 'OCTOSTORE_SMOKE_BINARY="$PWD/${{ matrix.name }}" scripts/smoke-release-fixture.sh'
require_text .github/workflows/release.yml 'key: ${{ matrix.os }}-${{ matrix.target }}'
require_text .github/workflows/release.yml 'actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2'
require_text .github/workflows/release.yml 'artifact-metadata: write'
require_text .github/workflows/release.yml '--signer-workflow "github.com/$GITHUB_REPOSITORY/.github/workflows/release.yml"'
require_text .github/workflows/release.yml '--source-ref "$GITHUB_REF"'
require_text .github/workflows/release.yml '--source-digest "$GITHUB_SHA"'
require_text .github/workflows/release.yml '--deny-self-hosted-runners'
require_text .github/workflows/release.yml 'release-evidence-${{ matrix.name }}.tsv'
require_text .github/workflows/release.yml 'cargo package --locked'
require_text .github/workflows/release.yml 'scripts/verify-crates-io-package.sh live "$VERSION" "$CRATE"'
require_text .github/workflows/release.yml 'already published with the exact unyanked candidate archive'
require_text .github/workflows/release.yml 'Verify the exact draft release asset set and matrix provenance'
require_text .github/workflows/release.yml 'gh release view "$RELEASE_TAG"'
require_text .github/workflows/release.yml "--json assets --jq '.assets[].name'"
require_text .github/workflows/release.yml 'gh release download "$RELEASE_TAG"'
require_text .github/workflows/release.yml '(cd "$VERIFY_DIR" && sha256sum --check SHA256SUMS)'
require_text .github/workflows/release.yml 'gh release verify "$RELEASE_TAG"'
require_text .github/workflows/release.yml 'gh release verify-asset "$RELEASE_TAG"'
require_text .github/workflows/release.yml 'test "$EVIDENCE_SHA" = "$(sha256sum "$VERIFY_DIR/$ASSET"'
require_text .github/workflows/deploy.yml 'EXPECTED_COMMIT="$(gh api'
require_text .github/workflows/deploy.yml '--json isDraft,isPrerelease,isImmutable'
require_text .github/workflows/deploy.yml 'manual redeploy requires a published, immutable, non-prerelease GitHub release'
require_text .github/workflows/deploy.yml 'git fetch --force origin main "refs/tags/$RELEASE_TAG:refs/tags/$RELEASE_TAG"'
require_text .github/workflows/deploy.yml 'git merge-base --is-ancestor "$EXPECTED_COMMIT" origin/main'
require_text .github/workflows/deploy.yml 'gh attestation verify "$ASSET"'
require_text .github/workflows/release.yml '--source-ref "refs/heads/main"'
require_text .github/workflows/deploy.yml '--source-ref "refs/heads/main"'
require_text .github/workflows/deploy.yml '--source-digest "$EXPECTED_COMMIT"'
require_text .github/workflows/deploy.yml '"$EXPECTED_PREVIOUS_ASSET_SHA" < deploy/deploy.sh'
require_text .github/workflows/ci.yml 'fetch-depth: 0'
require_text .github/workflows/ci.yml 'run: scripts/ci-local.sh'
require_text scripts/ci-local.sh 'scripts/smoke-downgrade-compatibility.sh'
require_text scripts/smoke-downgrade-compatibility.sh '90eb7ebfdbdc2004c8340c1bdabe0a786d858959'
require_text scripts/smoke-downgrade-compatibility.sh 'ADMIN_KEY=compat-admin-key'
require_text scripts/smoke-downgrade-compatibility.sh 'SENTINEL_HOLDER=ffffffff-ffff-ffff-ffff-ffffffffffff'
require_text scripts/smoke-downgrade-compatibility.sh 'v0.14 impossible sentinel -> v0.13.2 admin rejection/hold/acquire -> v0.14 successor recovery'
require_text deploy/deploy.sh 'git merge-base --is-ancestor "$EXPECTED_COMMIT" origin/main'
require_text deploy/deploy.sh 'EXPECTED_ASSET_SHA="${3:?Usage:'
require_text deploy/deploy.sh 'EXPECTED_PREVIOUS_ASSET_SHA="${4:?Usage:'
require_text deploy/deploy.sh 'asset digest does not match the exact-commit GitHub attestation'
require_text deploy/deploy.sh 'current production binary does not match independently attested previous release'
require_text deploy/deploy.sh 'LIVE_ASSET_SHA'
require_text deploy/deploy.sh 'install_tagged_canary_pair'
require_text deploy/deploy.sh 'run_supervised_canary'
require_text deploy/deploy.sh 'set_safe_site_modes'
require_text deploy/deploy.sh 'verify_safe_site_modes'
require_text deploy/deploy.sh 'live_supervised_canary=leader-waiter,cancellation,takeover,workers-stopped,processes-cleared,secret-free'
require_text scripts/check-package.sh 'package list differs from the exact allowlist'
require_text scripts/check-package.sh 'generated archive differs from the reviewed package list'
require_text scripts/verify-crates-io-package.sh 'existing crate checksum does not match the exact candidate archive'
require_text scripts/verify-crates-io-package.sh 'existing crate version is yanked'
require_text install.sh 'Install commit failed; restoring the previous executable pair'
require_text install.sh 'OCTOSTORE_RELEASE_METADATA_URL'
require_text install.sh '.immutable | select(type == "boolean")'
require_text install.sh 'Independent release metadata digest mismatch for $SUPERVISOR_ASSET'
require_text scripts/smoke-release-fixture.sh 'octostore-supervisor\.backup\..+'
require_text scripts/smoke-release-fixture.sh 'installer accepted a substituted supervisor and matching SHA256SUMS'
require_text scripts/smoke-release-fixture.sh 'OCTOSTORE_SUPERVISOR="$INSTALLED_SUPERVISOR"'
require_text scripts/smoke-release-fixture.sh '"$INSTALLED" "$INSTALLED_SUPERVISOR"'

# Recovery is durable across process death and checked before a new attempt.
require_text deploy/deploy.sh 'trap '\''handle_failure $? "ERR at line $LINENO"'\'' ERR'
require_text deploy/deploy.sh 'trap '\''handle_failure 130 INT'\'' INT'
require_text deploy/deploy.sh 'trap '\''handle_failure 143 TERM'\'' TERM'
require_text deploy/deploy.sh 'trap '\''handle_failure 129 HUP'\'' HUP'
require_text deploy/deploy.sh 'STATE_FILE="$DEPLOY_ROOT/.octostore-deploy-state"'
require_text deploy/deploy.sh 'RECOVERY: found an incomplete deployment'
require_text deploy/deploy.sh 'ROLLBACK_FAILED: recovery is incomplete'
require_text deploy/deploy.sh 'verify_api_version "$STATE_PREVIOUS_VERSION" rollback'
require_text deploy/deploy.sh 'verify_api_health rollback'
require_text deploy/deploy.sh 'verify_exact_site_tree "$STATE_PREVIOUS_COMMIT" rollback'
require_text deploy/deploy.sh '"$STATE_PREVIOUS_COMMIT" "$STATE_PREVIOUS_VERSION" rollback'
require_text deploy/deploy.sh 'EVIDENCE rollback_complete'
require_text deploy/deploy.sh 'EVIDENCE known_good_retained'

# Public proof enumerates the Git site manifest and separately proves the
# documented API health object and versioned raw-GitHub installer bytes.
require_text deploy/deploy.sh 'git -c core.quotePath=false ls-tree -r --name-only "$commit" -- site'
require_text deploy/deploy.sh 'served site files do not exactly match the Git manifest'
require_text deploy/deploy.sh 'verify_exact_site_tree "$EXPECTED_COMMIT" live'
require_text deploy/deploy.sh 'PRODUCTION_INSTALLER_ORIGIN=https://raw.githubusercontent.com/octostore/octostore.io'
require_text deploy/deploy.sh 'curl -fsS --max-time 3 "$API_ORIGIN/health"'
require_text deploy/deploy.sh 'verify_api_health live'
require_text deploy/deploy.sh 'git show "$commit:install.sh"'
require_text deploy/deploy.sh '"$INSTALLER_ORIGIN/v$version/install.sh"'
require_text deploy/deploy.sh 'verify_public_installer "$EXPECTED_COMMIT" "$EXPECTED_VERSION" live'

# Provenance failures are behavioral: tag/SHA mismatch, off-main tags, and a
# wrong attested asset digest must exit before checkout, service, or binary work.
require_text tests/fixtures/deploy-failure-fixture.sh 'run_provenance_failure_case'
require_text tests/fixtures/deploy-failure-fixture.sh 'tag-commit-mismatch'
require_text tests/fixtures/deploy-failure-fixture.sh 'off-main-tag'
require_text tests/fixtures/deploy-failure-fixture.sh 'run_asset_digest_failure_case'
require_text tests/fixtures/deploy-failure-fixture.sh 'case=asset-digest before_mutation=true'
require_text tests/fixtures/deploy-failure-fixture.sh 'case=previous-asset-digest before_mutation=true'
require_text tests/fixtures/deploy-failure-fixture.sh 'provenance failure invoked systemctl'
require_text tests/fixtures/deploy-failure-fixture.sh 'provenance failure changed the binary'
require_text tests/fixtures/deploy-failure-fixture.sh 'run_proof_failure_case health'
require_text tests/fixtures/deploy-failure-fixture.sh 'run_proof_failure_case installer'
require_text tests/fixtures/deploy-failure-fixture.sh 'run_canary_failure_case'
require_text tests/fixtures/deploy-failure-fixture.sh 'assert_safe_site_modes'
require_text tests/fixtures/deploy-failure-fixture.sh 'fixture_supervised_canary=passed installed_pair=true'
require_text tests/fixtures/deploy-failure-fixture.sh 'case=rollback-health rollback_failure=reported'
require_text tests/fixtures/stable-release-tag-fixture.sh 'rc, preview, nightly, malformed, and build-suffixed tags rejected'

CONTRACT_TMP=$(mktemp -d "$ROOT/.release-workflow-contract.XXXXXX")
cleanup_contract_tmp() {
  rm -rf "$CONTRACT_TMP"
}
trap cleanup_contract_tmp EXIT

trust_run="$CONTRACT_TMP/release-trust-boundary.sh"
extract_workflow_run .github/workflows/release.yml \
  'Establish trusted default-branch release authority' "$trust_run"
trust_bin="$CONTRACT_TMP/trust-bin"
trust_log="$CONTRACT_TMP/trust.calls"
mkdir -p "$trust_bin"
cat >"$trust_bin/git" <<'MOCK_TRUST_GIT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$MOCK_TRUST_LOG"
case "${1:-}" in
  fetch|checkout) ;;
  rev-parse)
    case "${2:-}" in
      'refs/tags/v9.9.9^{commit}') printf '%s\n' "$MOCK_TAG_COMMIT" ;;
      HEAD) printf '%s\n' "$GITHUB_SHA" ;;
      *) echo "unexpected trust-boundary rev-parse: $*" >&2; exit 64 ;;
    esac
    ;;
  merge-base)
    [[ ${MOCK_ON_MAIN:-true} == true ]]
    ;;
  *) echo "unexpected trust-boundary git invocation: $*" >&2; exit 64 ;;
esac
MOCK_TRUST_GIT
chmod 0755 "$trust_bin/git"

run_release_trust_fixture() {
  local label=$1
  local event_name=$2
  local event_ref=$3
  local tag_commit=$4
  local on_main=$5
  local expected_status=$6
  local status
  : >"$trust_log"
  set +e
  PATH="$trust_bin:$PATH" MOCK_TRUST_LOG="$trust_log" \
    MOCK_TAG_COMMIT="$tag_commit" MOCK_ON_MAIN="$on_main" \
    GITHUB_EVENT_NAME="$event_name" GITHUB_REF="$event_ref" \
    GITHUB_SHA=1111111111111111111111111111111111111111 \
    RELEASE_TAG=v9.9.9 \
    bash "$trust_run" >"$CONTRACT_TMP/trust-$label.out" 2>&1
  status=$?
  set -e
  if [[ $expected_status == reject && $status == 0 ]]; then
    echo ".github/workflows/release.yml: $label untrusted release authority unexpectedly passed" >&2
    exit 1
  fi
  if [[ $expected_status == accept && $status != 0 ]]; then
    echo ".github/workflows/release.yml: trusted default-branch release authority unexpectedly failed" >&2
    exit 1
  fi
  if [[ $label == wrong-event || $label == wrong-ref ]] && [[ -s $trust_log ]]; then
    echo ".github/workflows/release.yml: $label reached Git before rejecting the workflow-code trust boundary" >&2
    exit 1
  fi
}

run_release_trust_fixture wrong-event push refs/heads/main \
  1111111111111111111111111111111111111111 true reject
run_release_trust_fixture wrong-ref repository_dispatch refs/heads/untrusted \
  1111111111111111111111111111111111111111 true reject
run_release_trust_fixture tag-mismatch repository_dispatch refs/heads/main \
  2222222222222222222222222222222222222222 true reject
run_release_trust_fixture off-main repository_dispatch refs/heads/main \
  1111111111111111111111111111111111111111 false reject
run_release_trust_fixture trusted repository_dispatch refs/heads/main \
  1111111111111111111111111111111111111111 true accept

release_run="$CONTRACT_TMP/release-finalize.sh"
extract_workflow_run .github/workflows/release.yml \
  'Publish the validated draft release' "$release_run"
mock_bin="$CONTRACT_TMP/mock-bin"
mkdir -p "$mock_bin"
cat >"$mock_bin/gh" <<'MOCK_GH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$MOCK_GH_LOG"
read_state() {
  # shellcheck disable=SC1090
  source "$MOCK_GH_STATE"
}
write_state() {
  {
    printf 'enabled=%q\n' "$enabled"
    printf 'draft=%q\n' "$draft"
    printf 'immutable=%q\n' "$immutable"
    printf 'exists=%q\n' "$exists"
  } >"$MOCK_GH_STATE"
}
read_state
case "${1:-} ${2:-}" in
  'api repos/test/repo/immutable-releases')
    printf '%s\n' "$enabled"
    ;;
  'api repos/test/repo/releases/tags/v9.9.9')
    [[ "$exists" == true ]] || exit 1
    printf '{"draft":%s,"immutable":%s}\n' "$draft" "$immutable"
    ;;
  'release edit')
    draft=false
    write_state
    ;;
  'release delete')
    exists=false
    write_state
    ;;
  *)
    echo "unexpected mock gh invocation: $*" >&2
    exit 64
    ;;
esac
MOCK_GH
cat >"$mock_bin/git" <<'MOCK_GIT'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  fetch|checkout) ;;
  rev-parse)
    case "${2:-}" in
      'refs/tags/v9.9.9^{commit}'|HEAD) printf '%s\n' "$GITHUB_SHA" ;;
      *) echo "unexpected mock git rev-parse: $*" >&2; exit 64 ;;
    esac
    ;;
  *) echo "unexpected mock git invocation: $*" >&2; exit 64 ;;
esac
MOCK_GIT
chmod 0755 "$mock_bin/gh" "$mock_bin/git"

run_release_failure_fixture() {
  local enabled_value=$1
  local label=$2
  local expected_log=$3
  local state="$CONTRACT_TMP/release-$label.state"
  local log="$CONTRACT_TMP/release-$label.calls"
  local status
  {
    printf 'enabled=%q\n' "$enabled_value"
    printf 'draft=%q\n' true
    printf 'immutable=%q\n' false
    printf 'exists=%q\n' true
  } >"$state"
  : >"$log"
  set +e
  PATH="$mock_bin:$PATH" \
    MOCK_GH_STATE="$state" MOCK_GH_LOG="$log" \
    RELEASE_POLICY_TOKEN=fixture-policy-token \
    GITHUB_REPOSITORY=test/repo RELEASE_TAG=v9.9.9 \
    GITHUB_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
    bash "$release_run" >"$CONTRACT_TMP/release-$label.out" 2>&1
  status=$?
  set -e
  ((status != 0)) || {
    echo "release publication fixture $label unexpectedly succeeded" >&2
    exit 1
  }
  grep -Fq "$expected_log" "$log" || {
    echo "release publication fixture $label missed expected call: $expected_log" >&2
    exit 1
  }
}

run_release_failure_fixture false policy-disabled \
  'api repos/test/repo/immutable-releases'
if grep -Fq 'release edit' "$CONTRACT_TMP/release-policy-disabled.calls"; then
  echo "disabled immutable-release policy reached publication" >&2
  exit 1
fi
run_release_failure_fixture true publication-race 'release delete'
fixture_exists=$(awk -F= '$1 == "exists" { print $2 }' \
  "$CONTRACT_TMP/release-publication-race.state")
if [[ "$fixture_exists" != false ]]; then
  echo "failed publication left a public mutable release in the behavioral fixture" >&2
  exit 1
fi

# An environment binding pauses this job for GitHub's configured reviewers;
# this API preflight then independently fails closed if the environment is
# absent or its required-reviewer rule has been removed before SSH setup.
environment_policy_bin="$CONTRACT_TMP/environment-policy-bin"
environment_policy_log="$CONTRACT_TMP/environment-policy.calls"
mkdir -p "$environment_policy_bin"
cat >"$environment_policy_bin/gh" <<'MOCK_GH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$MOCK_GH_LOG"
if [[ ${GH_TOKEN:-} != fixture-policy-token ]]; then
  echo "environment-policy API was not authenticated with the administration-read policy token" >&2
  exit 65
fi
case "${1:-} ${2:-}" in
  'api repos/test/repo/environments/production')
    case "$MOCK_ENVIRONMENT_POLICY" in
      absent) exit 1 ;;
      unprotected) printf '%s\n' '{"deployment_branch_policy":null,"protection_rules":[]}' ;;
      self-review) printf '%s\n' '{"deployment_branch_policy":{"protected_branches":true,"custom_branch_policies":false},"protection_rules":[{"type":"required_reviewers","enabled":true,"prevent_self_review":false,"reviewers":[{"type":"User"}]}]}' ;;
      unrestricted-branches) printf '%s\n' '{"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":false},"protection_rules":[{"type":"required_reviewers","enabled":true,"prevent_self_review":true,"reviewers":[{"type":"User"}]}]}' ;;
      protected) printf '%s\n' '{"deployment_branch_policy":{"protected_branches":true,"custom_branch_policies":false},"protection_rules":[{"type":"required_reviewers","enabled":true,"prevent_self_review":true,"reviewers":[{"type":"User"}]}]}' ;;
      *) echo "unexpected environment-policy fixture state: $MOCK_ENVIRONMENT_POLICY" >&2; exit 64 ;;
    esac
    ;;
  *) echo "unexpected environment-policy gh invocation: $*" >&2; exit 64 ;;
esac
MOCK_GH
chmod 0755 "$environment_policy_bin/gh"

run_environment_policy_fixture() {
  local workflow=$1
  local label=$2
  local expected_status=$3
  local credential=$4
  local preflight="$CONTRACT_TMP/${label}-environment-policy.sh"
  local status
  extract_workflow_run "$workflow" 'Require protected production environment' "$preflight"
  : >"$environment_policy_log"
  set +e
  PATH="$environment_policy_bin:$PATH" \
    MOCK_GH_LOG="$environment_policy_log" \
    MOCK_ENVIRONMENT_POLICY="$label" \
    RELEASE_POLICY_TOKEN="$credential" GITHUB_REPOSITORY=test/repo \
    bash "$preflight" >"$CONTRACT_TMP/${label}-environment-policy.out" 2>&1
  status=$?
  set -e
  if [[ $expected_status == reject && $status == 0 ]]; then
    echo "$workflow: $label production-environment policy unexpectedly passed before SSH" >&2
    exit 1
  fi
  if [[ $expected_status == accept && $status != 0 ]]; then
    echo "$workflow: protected production-environment policy unexpectedly failed" >&2
    exit 1
  fi
  if [[ $label == missing ]]; then
    if [[ -s $environment_policy_log ]]; then
      echo "$workflow: missing policy credential unexpectedly reached the environment API" >&2
      exit 1
    fi
    grep -Fq 'OCTOSTORE_RELEASE_POLICY_TOKEN with repository administration-read access is required' \
      "$CONTRACT_TMP/${label}-environment-policy.out" || {
      echo "$workflow: missing policy credential did not fail explicitly" >&2
      exit 1
    }
  else
    grep -Fq 'api repos/test/repo/environments/production' "$environment_policy_log" || {
      echo "$workflow: environment-policy fixture did not use authenticated environment API authority" >&2
      exit 1
    }
  fi
}

for workflow in .github/workflows/release.yml .github/workflows/deploy.yml; do
  run_environment_policy_fixture "$workflow" missing reject ''
  run_environment_policy_fixture "$workflow" absent reject fixture-policy-token
  run_environment_policy_fixture "$workflow" unprotected reject fixture-policy-token
  run_environment_policy_fixture "$workflow" self-review reject fixture-policy-token
  run_environment_policy_fixture "$workflow" unrestricted-branches reject fixture-policy-token
  run_environment_policy_fixture "$workflow" protected accept fixture-policy-token
done

# Migration is transactional at the repository-copy boundary: environment
# writes may be partial, but no repository copy is deleted until all three
# environment secret names have been observed. A later run can safely finish a
# partial write/delete, and the already-migrated path performs no rewrite.
secret_migration_bin="$CONTRACT_TMP/secret-migration-bin"
mkdir -p "$secret_migration_bin"
cat >"$secret_migration_bin/gh" <<'MOCK_GH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$MOCK_MIGRATION_LOG"
if [[ ${GH_TOKEN:-} != fixture-migration-token ]]; then
  echo "secret migration was not authenticated with the production migration token" >&2
  exit 65
fi
case "${1:-} ${2:-}" in
  'api repos/test/repo/actions/secrets?per_page=100')
    cat "$MOCK_REPOSITORY_SECRETS"
    ;;
  'api repos/test/repo/environments/production/secrets?per_page=100')
    while IFS= read -r secret_name; do
      if [[ -n $secret_name && $secret_name != "${MOCK_HIDE_ENVIRONMENT_SECRET:-}" ]]; then
        printf '%s\n' "$secret_name"
      fi
    done <"$MOCK_ENVIRONMENT_SECRETS"
    ;;
  'secret set')
    secret_name=${3:-}
    if [[ $secret_name == "${MOCK_FAIL_SET_SECRET:-}" ]]; then
      exit 1
    fi
    secret_value=$(tr -d '\000')
    [[ -n $secret_value ]] || exit 66
    if ! grep -Fxq -- "$secret_name" "$MOCK_ENVIRONMENT_SECRETS"; then
      printf '%s\n' "$secret_name" >>"$MOCK_ENVIRONMENT_SECRETS"
    fi
    ;;
  'api --method')
    [[ ${3:-} == DELETE ]] || exit 64
    endpoint=${4:-}
    secret_name=${endpoint##*/}
    if [[ $secret_name == "${MOCK_FAIL_DELETE_SECRET:-}" ]]; then
      exit 1
    fi
    next_repository_secrets="$MOCK_REPOSITORY_SECRETS.next"
    grep -Fxv -- "$secret_name" "$MOCK_REPOSITORY_SECRETS" \
      >"$next_repository_secrets" || true
    mv "$next_repository_secrets" "$MOCK_REPOSITORY_SECRETS"
    ;;
  *)
    echo "unexpected secret-migration gh invocation: $*" >&2
    exit 64
    ;;
esac
MOCK_GH
chmod 0755 "$secret_migration_bin/gh"

run_secret_migration_fixture() {
  local workflow=$1
  local label=$2
  local expected_status=$3
  local workflow_slug
  local migration_run
  local fixture_dir
  local status
  local verify_call_line
  local delete_call_line
  workflow_slug=$(basename "$workflow" .yml)
  migration_run="$CONTRACT_TMP/$workflow_slug-$label-secret-migration.sh"
  fixture_dir="$CONTRACT_TMP/$workflow_slug-$label-secret-migration"
  mkdir -p "$fixture_dir"
  extract_workflow_run "$workflow" 'Migrate production SSH secrets' "$migration_run"
  : >"$fixture_dir/repository-secrets"
  : >"$fixture_dir/environment-secrets"
  : >"$fixture_dir/calls"
  deploy_host_value=fixture-host
  deploy_ssh_key_value=fixture-private-key
  deploy_known_hosts_value=fixture-known-hosts
  fail_set_secret=''
  fail_delete_secret=''
  hide_environment_secret=''

  case "$label" in
    migrate)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS \
        >"$fixture_dir/repository-secrets"
      ;;
    missing-value)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS \
        >"$fixture_dir/repository-secrets"
      deploy_ssh_key_value=''
      ;;
    set-failure)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS \
        >"$fixture_dir/repository-secrets"
      fail_set_secret=DEPLOY_SSH_KEY
      ;;
    verification-failure)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS \
        >"$fixture_dir/repository-secrets"
      hide_environment_secret=DEPLOY_SSH_KNOWN_HOSTS
      ;;
    delete-failure)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS \
        >"$fixture_dir/repository-secrets"
      fail_delete_secret=DEPLOY_SSH_KEY
      ;;
    partial-environment)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY >"$fixture_dir/environment-secrets"
      ;;
    already-migrated)
      printf '%s\n' DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS \
        >"$fixture_dir/environment-secrets"
      ;;
    *)
      echo "unknown secret migration fixture: $label" >&2
      exit 1
      ;;
  esac

  set +e
  PATH="$secret_migration_bin:$PATH" \
    MOCK_MIGRATION_LOG="$fixture_dir/calls" \
    MOCK_REPOSITORY_SECRETS="$fixture_dir/repository-secrets" \
    MOCK_ENVIRONMENT_SECRETS="$fixture_dir/environment-secrets" \
    MOCK_FAIL_SET_SECRET="$fail_set_secret" \
    MOCK_FAIL_DELETE_SECRET="$fail_delete_secret" \
    MOCK_HIDE_ENVIRONMENT_SECRET="$hide_environment_secret" \
    MIGRATION_TOKEN=fixture-migration-token GITHUB_REPOSITORY=test/repo \
    DEPLOY_HOST_VALUE="$deploy_host_value" \
    DEPLOY_SSH_KEY_VALUE="$deploy_ssh_key_value" \
    DEPLOY_SSH_KNOWN_HOSTS_VALUE="$deploy_known_hosts_value" \
    bash "$migration_run" >"$fixture_dir/output" 2>&1
  status=$?
  set -e

  if [[ $expected_status == reject && $status == 0 ]]; then
    echo "$workflow: $label secret migration unexpectedly passed before SSH" >&2
    exit 1
  fi
  if [[ $expected_status == accept && $status != 0 ]]; then
    echo "$workflow: $label secret migration unexpectedly failed" >&2
    exit 1
  fi
  if grep -Fq -e fixture-host -e fixture-private-key -e fixture-known-hosts \
    "$fixture_dir/calls" "$fixture_dir/output"; then
    echo "$workflow: $label secret migration exposed a secret value" >&2
    exit 1
  fi

  case "$label" in
    missing-value|set-failure|verification-failure|partial-environment)
      if grep -Fq 'api --method DELETE' "$fixture_dir/calls"; then
        echo "$workflow: $label deleted a repository rollback copy before complete verification" >&2
        exit 1
      fi
      ;;
    migrate)
      for secret_name in DEPLOY_HOST DEPLOY_SSH_KEY DEPLOY_SSH_KNOWN_HOSTS; do
        grep -Fxq -- "$secret_name" "$fixture_dir/environment-secrets" || {
          echo "$workflow: migration did not verify environment secret $secret_name" >&2
          exit 1
        }
        if grep -Fxq -- "$secret_name" "$fixture_dir/repository-secrets"; then
          echo "$workflow: migration retained repository secret $secret_name" >&2
          exit 1
        fi
      done
      if [[ $(grep -cF 'secret set ' "$fixture_dir/calls") -ne 3 \
        || $(grep -cF 'api --method DELETE' "$fixture_dir/calls") -ne 3 ]]; then
        echo "$workflow: migration did not set, verify, and delete exactly three SSH secrets" >&2
        exit 1
      fi
      ;;
    already-migrated)
      if grep -Fq -e 'secret set ' -e 'api --method DELETE' "$fixture_dir/calls"; then
        echo "$workflow: already-migrated path rewrote or deleted secrets" >&2
        exit 1
      fi
      grep -Fq 'production_ssh_secrets_already_environment_scoped' "$fixture_dir/output" || {
        echo "$workflow: already-migrated path did not report idempotent success" >&2
        exit 1
      }
      ;;
  esac

  if [[ $label == migrate || $label == delete-failure ]]; then
    verify_call_line=$(grep -nF \
      'api repos/test/repo/environments/production/secrets?per_page=100' \
      "$fixture_dir/calls" | tail -n 1 | cut -d: -f1)
    delete_call_line=$(grep -nF 'api --method DELETE' "$fixture_dir/calls" \
      | head -n 1 | cut -d: -f1)
    if [[ -z $verify_call_line || -z $delete_call_line \
      || $verify_call_line -ge $delete_call_line ]]; then
      echo "$workflow: $label did not verify all environment names before repository deletion" >&2
      exit 1
    fi
  fi
}

for workflow in .github/workflows/release.yml .github/workflows/deploy.yml; do
  run_secret_migration_fixture "$workflow" missing-value reject
  run_secret_migration_fixture "$workflow" set-failure reject
  run_secret_migration_fixture "$workflow" verification-failure reject
  run_secret_migration_fixture "$workflow" delete-failure reject
  run_secret_migration_fixture "$workflow" partial-environment reject
  run_secret_migration_fixture "$workflow" migrate accept
  run_secret_migration_fixture "$workflow" already-migrated accept
done

run_cargo_secret_migration_fixture() {
  local label=$1
  local expected_status=$2
  local migration_run="$CONTRACT_TMP/cargo-$label-secret-migration.sh"
  local fixture_dir="$CONTRACT_TMP/cargo-$label-secret-migration"
  local cargo_value=fixture-cargo-token
  local fail_set_secret=''
  local fail_delete_secret=''
  local hide_environment_secret=''
  local status
  mkdir -p "$fixture_dir"
  extract_workflow_run .github/workflows/release.yml \
    'Migrate crates.io credential into the approved environment' "$migration_run"
  : >"$fixture_dir/repository-secrets"
  : >"$fixture_dir/environment-secrets"
  : >"$fixture_dir/calls"
  case "$label" in
    migrate)
      printf '%s\n' CARGO_REGISTRY_TOKEN >"$fixture_dir/repository-secrets"
      ;;
    missing-value)
      printf '%s\n' CARGO_REGISTRY_TOKEN >"$fixture_dir/repository-secrets"
      cargo_value=''
      ;;
    set-failure)
      printf '%s\n' CARGO_REGISTRY_TOKEN >"$fixture_dir/repository-secrets"
      fail_set_secret=CARGO_REGISTRY_TOKEN
      ;;
    verification-failure)
      printf '%s\n' CARGO_REGISTRY_TOKEN >"$fixture_dir/repository-secrets"
      hide_environment_secret=CARGO_REGISTRY_TOKEN
      ;;
    delete-failure)
      printf '%s\n' CARGO_REGISTRY_TOKEN >"$fixture_dir/repository-secrets"
      fail_delete_secret=CARGO_REGISTRY_TOKEN
      ;;
    already-migrated)
      printf '%s\n' CARGO_REGISTRY_TOKEN >"$fixture_dir/environment-secrets"
      ;;
    absent-both) ;;
    *) echo "unknown crates.io migration fixture: $label" >&2; exit 1 ;;
  esac

  set +e
  PATH="$secret_migration_bin:$PATH" \
    MOCK_MIGRATION_LOG="$fixture_dir/calls" \
    MOCK_REPOSITORY_SECRETS="$fixture_dir/repository-secrets" \
    MOCK_ENVIRONMENT_SECRETS="$fixture_dir/environment-secrets" \
    MOCK_FAIL_SET_SECRET="$fail_set_secret" \
    MOCK_FAIL_DELETE_SECRET="$fail_delete_secret" \
    MOCK_HIDE_ENVIRONMENT_SECRET="$hide_environment_secret" \
    MIGRATION_TOKEN=fixture-migration-token GITHUB_REPOSITORY=test/repo \
    CARGO_REGISTRY_TOKEN_VALUE="$cargo_value" \
    bash "$migration_run" >"$fixture_dir/output" 2>&1
  status=$?
  set -e
  if [[ $expected_status == reject && $status == 0 ]]; then
    echo ".github/workflows/release.yml: $label crates.io migration unexpectedly passed" >&2
    exit 1
  fi
  if [[ $expected_status == accept && $status != 0 ]]; then
    echo ".github/workflows/release.yml: $label crates.io migration unexpectedly failed" >&2
    exit 1
  fi
  if grep -Fq fixture-cargo-token "$fixture_dir/calls" "$fixture_dir/output"; then
    echo ".github/workflows/release.yml: $label crates.io migration exposed the credential" >&2
    exit 1
  fi
  case "$label" in
    missing-value|set-failure|verification-failure)
      if grep -Fq 'api --method DELETE' "$fixture_dir/calls"; then
        echo ".github/workflows/release.yml: $label deleted the repository crates.io rollback copy before verification" >&2
        exit 1
      fi
      ;;
    migrate)
      grep -Fxq CARGO_REGISTRY_TOKEN "$fixture_dir/environment-secrets"
      ! grep -Fxq CARGO_REGISTRY_TOKEN "$fixture_dir/repository-secrets"
      grep -Fq 'crates_io_token_migrated_to_environment' "$fixture_dir/output"
      ;;
    already-migrated)
      if grep -Fq -e 'secret set ' -e 'api --method DELETE' "$fixture_dir/calls"; then
        echo ".github/workflows/release.yml: already-migrated crates.io token was rewritten" >&2
        exit 1
      fi
      grep -Fq 'crates_io_token_already_environment_scoped' "$fixture_dir/output"
      ;;
  esac
}

run_cargo_secret_migration_fixture absent-both reject
run_cargo_secret_migration_fixture missing-value reject
run_cargo_secret_migration_fixture set-failure reject
run_cargo_secret_migration_fixture verification-failure reject
run_cargo_secret_migration_fixture delete-failure reject
run_cargo_secret_migration_fixture migrate accept
run_cargo_secret_migration_fixture already-migrated accept

# A tag can be moved after the event has captured GITHUB_SHA. The crates.io
# publication boundary must reject that retarget before invoking cargo publish.
publish_run="$CONTRACT_TMP/release-publish.sh"
extract_workflow_run .github/workflows/release.yml 'Publish to crates.io' "$publish_run"
retarget_bin="$CONTRACT_TMP/retarget-bin"
retarget_log="$CONTRACT_TMP/tag-retarget.calls"
retarget_state="$CONTRACT_TMP/tag-retarget.target"
mkdir -p "$retarget_bin"
cat >"$retarget_bin/git" <<'MOCK_GIT'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  fetch)
    printf '%s\n' "$RETARGETED_TAG_SHA" >"$MOCK_TAG_TARGET"
    printf 'remote_tag_retargeted=%s\n' "$RETARGETED_TAG_SHA" >>"$MOCK_RELEASE_LOG"
    ;;
  rev-parse)
    case "${2:-}" in
      'refs/tags/v9.9.9^{commit}') cat "$MOCK_TAG_TARGET" ;;
      HEAD) printf '%s\n' "$GITHUB_SHA" ;;
      *) echo "unexpected mock git rev-parse: $*" >&2; exit 64 ;;
    esac
    ;;
  checkout)
    printf 'checkout=%s\n' "${3:-}" >>"$MOCK_RELEASE_LOG"
    ;;
  *) echo "unexpected mock git invocation: $*" >&2; exit 64 ;;
esac
MOCK_GIT
cat >"$retarget_bin/cargo" <<'MOCK_CARGO'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo=%s\n' "$*" >>"$MOCK_RELEASE_LOG"
MOCK_CARGO
chmod 0755 "$retarget_bin/git" "$retarget_bin/cargo"

event_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
retargeted_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
printf '%s\n' "$event_sha" >"$retarget_state"
: >"$retarget_log"
set +e
PATH="$retarget_bin:$PATH" \
  MOCK_RELEASE_LOG="$retarget_log" MOCK_TAG_TARGET="$retarget_state" \
  RETARGETED_TAG_SHA="$retargeted_sha" \
  RELEASE_TAG=v9.9.9 GITHUB_SHA="$event_sha" \
  bash "$publish_run" >"$CONTRACT_TMP/tag-retarget.out" 2>&1
retarget_status=$?
set -e
if ((retarget_status == 0)); then
  echo "retargeted release tag unexpectedly reached crates.io publication" >&2
  exit 1
fi
grep -Fq "remote_tag_retargeted=$retargeted_sha" "$retarget_log" || {
  echo "retarget fixture did not move the remote tag after event capture" >&2
  exit 1
}
if grep -Fq 'cargo=publish' "$retarget_log"; then
  echo "retargeted release tag reached irreversible crates.io publication" >&2
  exit 1
fi
grep -Fq "remote tag v9.9.9 resolves to $retargeted_sha, not event SHA $event_sha." \
  "$CONTRACT_TMP/tag-retarget.out" || {
  echo "retargeted release tag did not report the tag/SHA provenance failure" >&2
  exit 1
}

if [[ ${OCTOSTORE_RELEASE_CONTRACT_SCOPE:-full} == workflow-only ]]; then
  echo "workflow release contract passed: trusted default-branch dispatch, reviewer-gated publication, split policy/migration authority, transactional crates.io and SSH-secret migration, immutable publication cleanup, durable deployment inputs, and contained metrics writes"
  exit 0
fi

# This is the behavioral gate: injected errors/signals must restore and prove
# the prior release, failed rollback must remain nonzero, and a rerun must
# recover durable state before attempting the target again.
tests/fixtures/stable-release-tag-fixture.sh
tests/fixtures/deploy-failure-fixture.sh
tests/fixtures/crates-io-resume-fixture.sh

echo "release contract passed: trusted default-branch dispatch, reviewer-gated publication, split policy/migration authority, transactional crates.io and SSH-secret migration, immutable publication cleanup, durable deployment inputs, contained metrics writes, locked tools, behavioral provenance, rollback, package resume, and public proof"
