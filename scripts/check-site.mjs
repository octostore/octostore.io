#!/usr/bin/env node

import fs from "node:fs";
import crypto from "node:crypto";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const siteRoot = path.join(repositoryRoot, "site");
const siteOrigin = "https://octostore.io";
const errors = [];

function filesBelow(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const candidate = path.join(directory, entry.name);
    return entry.isDirectory() ? filesBelow(candidate) : [candidate];
  });
}

function attributeValues(markup, attribute) {
  const pattern = new RegExp(`\\b${attribute}\\s*=\\s*(["'])(.*?)\\1`, "gis");
  return [...markup.matchAll(pattern)].map((match) => match[2]);
}

function elementTextById(markup, id) {
  const escapedId = id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const pattern = new RegExp(
    `<([a-z][\\w:-]*)\\b[^>]*\\bid=(["'])${escapedId}\\2[^>]*>([\\s\\S]*?)<\\/\\1>`,
    "i",
  );
  const innerMarkup = markup.match(pattern)?.[3];
  if (innerMarkup === undefined) return null;
  return innerMarkup
    .replace(/<[^>]*>/g, "")
    .replaceAll("&amp;", "&")
    .replaceAll("&quot;", '"')
    .replaceAll("&#039;", "'")
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">");
}

function sectionBetween(text, startMarker, endMarker, label) {
  const start = text.indexOf(startMarker);
  const end = text.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || end < 0) {
    errors.push(`${label}: protected-work example section could not be located`);
    return "";
  }
  return text.slice(start, end);
}

function assertSupervisorCoupledExample(label, text, { kind, supervisorCount, required }) {
  if (text === null || text === "") {
    errors.push(`${label}: protected-work example is missing`);
    return;
  }

  for (const fragment of required) {
    if (!text.includes(fragment)) errors.push(`${label}: missing protected-work coupling ${fragment}`);
  }

  const supervisorPattern = new RegExp(`\\boctostore-supervisor ${kind}\\b`, "g");
  const holdPattern = new RegExp(`\\boctostore ${kind} hold\\b`, "g");
  const timeoutPattern = /--acquire-timeout 60\b/g;
  if ((text.match(supervisorPattern) ?? []).length !== supervisorCount) {
    errors.push(`${label}: expected ${supervisorCount} explicit ${kind} supervisor wrapper(s)`);
  }
  if ((text.match(holdPattern) ?? []).length !== supervisorCount) {
    errors.push(`${label}: expected ${supervisorCount} nested ${kind} hold command(s)`);
  }
  if ((text.match(timeoutPattern) ?? []).length !== supervisorCount) {
    errors.push(`${label}: every protected-work hold must retain --acquire-timeout 60`);
  }

  const lines = text.split("\n");
  const holdLine = new RegExp(`^\\s*octostore ${kind} hold\\b`);
  const bareHoldLine = new RegExp(`^octostore ${kind} hold\\b`);
  for (const [index, line] of lines.entries()) {
    if (bareHoldLine.test(line)) {
      errors.push(`${label}: contains a bare top-level protected-work hold`);
    }
    if (!holdLine.test(line)) continue;
    const previous = lines[index - 1]?.trimEnd() ?? "";
    if (!previous.includes(`octostore-supervisor ${kind}`) || !/-- \\+$/.test(previous)) {
      errors.push(`${label}: hold is not nested directly under its supervisor wrapper`);
    }
  }
}

function publicPath(file) {
  const relative = path.relative(siteRoot, file).split(path.sep).join("/");
  if (relative === "index.html") return "/";
  if (relative.endsWith("/index.html")) return `/${relative.slice(0, -"index.html".length)}`;
  return `/${relative}`;
}

function localFile(url) {
  const pathname = decodeURIComponent(url.pathname).replace(/^\/+/, "");
  return path.join(siteRoot, url.pathname.endsWith("/") ? pathname + "index.html" : pathname);
}

const htmlFiles = filesBelow(siteRoot).filter((file) => file.endsWith(".html")).sort();
const markupByFile = new Map(htmlFiles.map((file) => [file, fs.readFileSync(file, "utf8")]));

for (const file of htmlFiles) {
  const label = path.relative(siteRoot, file);
  const markup = markupByFile.get(file);
  const ids = attributeValues(markup, "id");
  const duplicateIds = [...new Set(ids.filter((id, index) => ids.indexOf(id) !== index))];
  if (duplicateIds.length > 0) errors.push(`${label}: duplicate IDs: ${duplicateIds.join(", ")}`);

  for (const image of markup.matchAll(/<img\b[^>]*>/gis)) {
    if (!/\balt\s*=\s*(["']).*?\1/is.test(image[0])) {
      errors.push(`${label}: image is missing alt text: ${image[0]}`);
    }
  }

  for (const script of markup.matchAll(/<script\b[^>]*>/gis)) {
    const source = attributeValues(script[0], "src")[0];
    if (!source) continue;
    const scriptUrl = new URL(source, new URL(publicPath(file), siteOrigin));
    if (scriptUrl.origin !== siteOrigin) {
      errors.push(`${label}: public site loads external runtime JavaScript ${source}`);
    }
  }

  for (const rawTarget of [...attributeValues(markup, "href"), ...attributeValues(markup, "src")]) {
    if (!rawTarget || /^(?:data|javascript|mailto|tel):/i.test(rawTarget)) continue;
    const target = new URL(rawTarget, new URL(publicPath(file), siteOrigin));
    if (target.origin !== siteOrigin) continue;

    const targetFile = localFile(target);
    if (!fs.existsSync(targetFile)) {
      errors.push(`${label}: missing local target ${rawTarget}`);
      continue;
    }
    if (target.hash && targetFile.endsWith(".html")) {
      const fragment = decodeURIComponent(target.hash.slice(1));
      const targetMarkup = markupByFile.get(targetFile) ?? fs.readFileSync(targetFile, "utf8");
      if (!attributeValues(targetMarkup, "id").includes(fragment)) {
        errors.push(`${label}: missing fragment target ${rawTarget}`);
      }
    }
  }
}

const socialPng = fs.readFileSync(path.join(siteRoot, "assets", "octostore-og.png"));
if (socialPng.readUInt32BE(16) !== 1200 || socialPng.readUInt32BE(20) !== 630) {
  errors.push("assets/octostore-og.png: expected exact 1200x630 Open Graph dimensions");
}
const socialSource = fs.readFileSync(path.join(siteRoot, "assets", "octostore-og.svg"), "utf8");
for (const required of [
  "STOP ",
  "TWO",
  " AGENTS",
  "FROM DOING THE",
  "SAME WORK.",
  "ONE SHARED LEASE · BEFORE SIDE EFFECTS",
  "AGENT ATLAS",
  "OWNS WORK",
  "AGENT COMET",
  "WAITS",
]) {
  if (!socialSource.includes(required)) {
    errors.push(`assets/octostore-og.svg: missing agent-first social-card copy: ${required}`);
  }
}

const homepage = markupByFile.get(path.join(siteRoot, "index.html"));
const agentGuide = markupByFile.get(path.join(siteRoot, "agents", "index.html"));
const docs = markupByFile.get(path.join(siteRoot, "docs", "index.html"));
const leaderElection = markupByFile.get(path.join(siteRoot, "leader-election", "index.html"));
const coordination = markupByFile.get(path.join(siteRoot, "coordination", "index.html"));
const dashboard = markupByFile.get(path.join(siteRoot, "dashboard.html"));
const adminDashboard = markupByFile.get(path.join(siteRoot, "admin.html"));
const legacyStatus = markupByFile.get(path.join(siteRoot, "status.html"));
const orchestrationCompatibility = markupByFile.get(path.join(siteRoot, "docs", "agent-orchestration.html"));
const readme = fs.readFileSync(path.join(repositoryRoot, "README.md"), "utf8");
const environmentExample = fs.readFileSync(path.join(repositoryRoot, ".env.example"), "utf8");
const openApi = fs.readFileSync(path.join(repositoryRoot, "openapi.yaml"), "utf8");
const sourceSkill = fs.readFileSync(path.join(repositoryRoot, "skills", "octostore", "SKILL.md"), "utf8");
const openSpec = fs.readFileSync(path.join(repositoryRoot, "OPENSPEC-agent-first-coordination.md"), "utf8");
const canonicalOpenSpec = openSpec.split("## Adversarial review disposition", 1)[0];
const cargoManifest = fs.readFileSync(path.join(repositoryRoot, "Cargo.toml"), "utf8");
const changelog = fs.readFileSync(path.join(repositoryRoot, "CHANGELOG.md"), "utf8");
const siteScript = fs.readFileSync(path.join(siteRoot, "assets", "site.js"), "utf8");
const supervisedDemo = fs.readFileSync(path.join(repositoryRoot, "scripts", "two-agent-supervised-demo.sh"), "utf8");
const twoAgentSmoke = fs.readFileSync(path.join(repositoryRoot, "scripts", "smoke-two-agents.sh"), "utf8");
const pinnedSkillCommand = "./node_modules/.bin/skills add";
const immutableSkillArtifact =
  "https://github.com/octostore/octostore.io/releases/download/v0.14.4/octostore-agent-skill.md";
const supervisedDemoInvocation = [
  "OCTOSTORE_ATLAS_WORKER=./run-agent-atlas \\",
  "OCTOSTORE_COMET_WORKER=./run-agent-comet \\",
  "./scripts/two-agent-supervised-demo.sh",
].join("\n");

for (const [label, text] of [
  [".env.example", environmentExample],
  ["README.md", readme],
  ["openapi.yaml", openApi],
  ["site/docs/index.html", docs],
]) {
  if (!text.includes("OCTOSTORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS=true")) {
    errors.push(`${label}: missing the explicit private-network webhook opt-in`);
  }
}

for (const [label, markup] of [
  ["agents/index.html", agentGuide],
  ["README.md", readme],
]) {
  if (!markup.includes(supervisedDemoInvocation)) {
    errors.push(`${label}: does not invoke the exact smoke-tested supervised two-agent demo`);
  }
}

for (const [label, text] of [
  ["README.md", readme],
  ["agents/index.html", agentGuide],
  ["coordination/index.html", coordination],
  ["docs/index.html", docs],
]) {
  const execute = 'OCTOSTORE_VERSION="$VERSION" sh octostore-install.sh';
  const executeIndex = text.indexOf(execute);
  const inspectIndex = text.indexOf("cat octostore-install.sh");
  if (executeIndex < 0 || inspectIndex < 0 || inspectIndex > executeIndex) {
    errors.push(`${label}: pinned installer execution must follow complete-file inspection`);
  }
  if (/\b(?:head|tail)\b[^\n]*octostore-install\.sh|\bsed\s+-n\s+[^\n]*octostore-install\.sh/.test(text)) {
    errors.push(`${label}: installer inspection must not show only a finite or selected subset`);
  }
}
if ((supervisedDemo.match(/election create --json/g) || []).length !== 1) {
  errors.push("two-agent-supervised-demo.sh: must create exactly one shared room");
}
for (const required of [
  "export OCTOSTORE_ELECTION",
  '"$OCTOSTORE_SUPERVISOR" election "$OCTOSTORE_ELECTION" agent-atlas',
  '"$OCTOSTORE_SUPERVISOR" election "$OCTOSTORE_ELECTION" agent-comet',
  'agent-atlas.jsonl" 2>"$DEMO_DIR/agent-atlas.err" &',
  'agent-comet.jsonl" 2>"$DEMO_DIR/agent-comet.err" &',
  "--acquire-timeout 60 --json",
  '\"event\":\"waiting\"',
  '\"event\":\"leader\"',
  'kill -TERM "$LEADER_PID"',
  "took over after cancellation",
  "waiting, takeover, and cancellation",
]) {
  if (!supervisedDemo.includes(required)) {
    errors.push(`two-agent-supervised-demo.sh: missing public outcome contract ${required}`);
  }
}
for (const required of [
  '"$ROOT/scripts/two-agent-supervised-demo.sh"',
  "OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY=1",
  "agent-atlas-worker.log",
  "agent-comet-worker.log",
]) {
  if (!twoAgentSmoke.includes(required)) {
    errors.push(`smoke-two-agents.sh: missing verbatim demo proof ${required}`);
  }
}

for (const [label, markup, required] of [
  ["index.html", homepage, "Task locks: authenticated, hosted or self-hosted"],
  ["agents/index.html", agentGuide, "Authenticated hosted or self-hosted boundary"],
  ["docs/index.html", docs, "Locks are authenticated on both the hosted and self-hosted authorities"],
  ["README.md", readme, "Authenticated hosted or self-hosted authority"],
  ["skills/octostore/SKILL.md", sourceSkill, "authenticated hosted or self-hosted OctoStore"],
  ["OPENSPEC-agent-first-coordination.md", canonicalOpenSpec, "OAuth-authenticated hosted or authenticated self-hosted authority"],
  ["Cargo.toml", cargoManifest, "authenticated hosted or self-hosted task locks"],
  ["CHANGELOG.md", changelog, "authenticated hosted or self-hosted lock"],
]) {
  if (!markup.includes(required)) {
    errors.push(`${label}: hosted authenticated-lock topology is missing '${required}'`);
  }
}
for (const [label, markup] of [
  ["index.html", homepage],
  ["agents/index.html", agentGuide],
  ["docs/index.html", docs],
  ["README.md", readme],
  ["skills/octostore/SKILL.md", sourceSkill],
  ["OPENSPEC-agent-first-coordination.md (canonical)", canonicalOpenSpec],
  ["Cargo.toml", cargoManifest],
  ["CHANGELOG.md", changelog],
]) {
  for (const forbidden of [
    "authenticated self-hosting",
    "Authenticated self-hosted boundary",
    "Locks are the self-hosted primitive",
    "Locks require an authenticated self-hosted OctoStore",
    "Authenticated self-hosted authority",
  ]) {
    if (markup.includes(forbidden)) {
      errors.push(`${label}: describes authenticated locks as self-host-only with '${forbidden}'`);
    }
  }
  for (const line of markup.split("\n")) {
    const selfHostedRequirement =
      /\b(?:locks?|task owner)\b.{0,120}\b(?:only|requires?|requirement)\b.{0,120}\bself-hosted\b/i.test(line) ||
      /\bself-hosted\b.{0,120}\b(?:only|requires?|requirement)\b.{0,120}\b(?:locks?|task owner)\b/i.test(line);
    const hostedAlternative =
      /hosted or self-hosted|hosted and self-hosted|OAuth-authenticated hosted|authenticated hosted/i.test(line);
    if (selfHostedRequirement && !hostedAlternative) {
      errors.push(`${label}: semantically describes authenticated locks as self-host-only: ${line.trim()}`);
    }
  }
}
for (const required of [
  "const HOSTED_API_BASE = 'https://api.octostore.io'",
  "--acquire-timeout 60 --json",
  "apiUrl(authData.apiBase, '/locks')",
]) {
  if (!dashboard.includes(required)) {
    errors.push(`dashboard.html: hosted authenticated-lock behavior is missing ${required}`);
  }
}
if (!dashboard.includes("OCTOSTORE_TOKEN_FILE=") || !dashboard.includes("/path/to/octostore-token")) {
  errors.push("dashboard.html: hosted authenticated-lock behavior is missing the token-file example");
}

assertSupervisorCoupledExample(
  "README.md election quickstart",
  sectionBetween(readme, "Under the hood", "Exactly one hold process", "README.md election quickstart"),
  {
    kind: "election",
    supervisorCount: 2,
    required: [
      "ATLAS_WORKER=./run-agent-atlas",
      "COMET_WORKER=./run-agent-comet",
      '"$ATLAS_WORKER"',
      '"$COMET_WORKER"',
    ],
  },
);
assertSupervisorCoupledExample(
  "README.md lock quickstart",
  sectionBetween(readme, "Use a stable lock key", "Lock hold creates", "README.md lock quickstart"),
  {
    kind: "lock",
    supervisorCount: 1,
    required: ["AGENT_WORKER=./run-agent", '"$AGENT_WORKER"'],
  },
);
assertSupervisorCoupledExample(
  "leader-election/index.html #leader-quickstart",
  elementTextById(leaderElection, "leader-quickstart"),
  {
    kind: "election",
    supervisorCount: 2,
    required: [
      "ATLAS_WORKER=./run-agent-atlas",
      "COMET_WORKER=./run-agent-comet",
      '"$ATLAS_WORKER"',
      '"$COMET_WORKER"',
    ],
  },
);
assertSupervisorCoupledExample(
  "coordination/index.html #coordination-claim",
  elementTextById(coordination, "coordination-claim"),
  {
    kind: "lock",
    supervisorCount: 1,
    required: ["AGENT_WORKER=./run-billing-agent", '"$AGENT_WORKER"'],
  },
);
assertSupervisorCoupledExample(
  "dashboard.html static #example-status",
  elementTextById(dashboard, "example-status"),
  {
    kind: "lock",
    supervisorCount: 1,
    required: ["AGENT_WORKER=./run-agent", '"$AGENT_WORKER"'],
  },
);

const updateExamplesStart = dashboard.indexOf("function updateExamples(apiBase)");
const updateExamplesEnd = dashboard.indexOf("function setupEventListeners()", updateExamplesStart);
if (updateExamplesStart < 0 || updateExamplesEnd < 0) {
  errors.push("dashboard.html: dynamic protected-work example generator is missing");
} else {
  const updateExamplesSource = dashboard.slice(updateExamplesStart, updateExamplesEnd);
  for (const required of [
    "AGENT_WORKER=",
    "./run-agent",
    "octostore-supervisor",
    '"$AGENT_WORKER"',
    "octostore</span> lock hold",
    "--acquire-timeout 60",
  ]) {
    if (!updateExamplesSource.includes(required)) {
      errors.push(`dashboard.html: dynamic protected-work example is missing ${required}`);
    }
  }
  const dynamicSupervisor = updateExamplesSource.indexOf("octostore-supervisor");
  const dynamicHold = updateExamplesSource.indexOf("octostore</span> lock hold");
  if (dynamicSupervisor < 0 || dynamicHold < dynamicSupervisor ||
      !updateExamplesSource.slice(dynamicSupervisor, dynamicHold).includes("-- \\\\")) {
    errors.push("dashboard.html: dynamic lock hold is not nested under its supervisor wrapper");
  }
}

for (const [label, markup] of [
  ...[...markupByFile.entries()].map(([file, value]) => [path.relative(siteRoot, file), value]),
  ["README.md", readme],
]) {
  if (/interactive\s+(?:api|console)/i.test(markup)) {
    errors.push(`${label}: promises an interactive API or console, but the API documentation is read-only`);
  }
}

for (const [label, markup] of [
  ["index.html", homepage],
  ["agents/index.html", agentGuide],
  ["docs/index.html", docs],
  ["README.md", readme],
]) {
  if (!markup.includes(pinnedSkillCommand)) {
    errors.push(`${label}: missing lockfile-installed skills CLI command`);
  }
  for (const required of ["npm ci --ignore-scripts --no-audit --no-fund", "--agent codex -y"]) {
    if (!markup.includes(required)) errors.push(`${label}: missing locked skill-install contract ${required}`);
  }
  if (/\bnpx\b[^\n]*\bskills(?:@|\s)/.test(markup)) {
    errors.push(`${label}: skill installation bypasses the committed npm lockfile`);
  }
}

if (!homepage.includes(immutableSkillArtifact)) {
  errors.push("index.html: primary agent-skill path must use the immutable v0.14.4 release artifact");
}
if (/href=["']\/agents\/SKILL\.md["']/.test(homepage)) {
  errors.push("index.html: mutable hosted skill must not be presented as a primary immutable release path");
}
for (const required of [
  immutableSkillArtifact,
  "immutable v0.14.4 release artifact",
  "The hosted [current skill](https://octostore.io/agents/SKILL.md) follows the site and may change between releases.",
]) {
  if (!readme.includes(required) && !homepage.includes(required)) {
    errors.push(`public skill messaging: missing immutable-versus-current distinction ${required}`);
  }
}

for (const required of [
  'id="agent-atlas-instruction"',
  'id="agent-comet-instruction"',
  'data-copy-target="#agent-atlas-instruction"',
  'data-copy-target="#agent-comet-instruction"',
]) {
  if (!homepage.includes(required)) errors.push(`index.html: missing dual-instruction contract ${required}`);
}

for (const required of [
  "Linux and macOS on AMD64 or ARM64",
  "sha256sum",
  "shasum",
  "OCTOSTORE_VERSION=\"$VERSION\" sh octostore-install.sh",
  "requests <code>sudo</code> only",
  "OCTOSTORE_INSTALL_DIR=\"$HOME/.local/bin\"",
  "loopback-only first-run recipe",
  "octostore --version",
  "authority_observed_unix_ms",
  "authority_observed_continuous_ms",
  "same host",
  "Perl",
  "Bash",
  "<code>seq</code>",
]) {
  if (!agentGuide.includes(required)) errors.push(`agents/index.html: missing fresh-machine contract ${required}`);
}
if (/\b[0-9]+ words\b/.test(agentGuide)) {
  errors.push("agents/index.html: contains brittle exact skill word count");
}
const readmeInstallIndex = readme.indexOf('OCTOSTORE_VERSION="$VERSION" sh octostore-install.sh');
const readmeVersionIndex = readme.indexOf("octostore --version");
const readmeFirstCoordinationIndex = readme.indexOf("octostore election create --json");
const readmePrerequisiteIndex = readme.indexOf("### Fresh-machine prerequisites");
if (
  readmeInstallIndex < 0 ||
  readmeVersionIndex < 0 ||
  readmeFirstCoordinationIndex < 0 ||
  !(readmeInstallIndex < readmeVersionIndex && readmeVersionIndex < readmeFirstCoordinationIndex)
) {
  errors.push("README.md: pinned installer and version verification must precede the first octostore coordination command");
}
if (readmePrerequisiteIndex < 0 || readmePrerequisiteIndex > readmeInstallIndex) {
  errors.push("README.md: fresh-machine prerequisites must precede the pinned installer command");
}
for (const required of [
  "sudo apt-get install -y git nodejs npm curl jq ca-certificates perl bash coreutils",
  "sudo apk add --no-cache git nodejs npm curl jq ca-certificates perl bash coreutils",
  "brew install git node curl jq coreutils perl bash",
  'command -v git node npm sh curl jq sha256sum perl bash seq',
  'command -v git node npm sh curl jq shasum perl bash seq',
  "CLOCK_BOOTTIME",
  "CLOCK_MONOTONIC_RAW",
  'ln -sf "$(brew --prefix coreutils)/bin/gseq" "$HOME/.local/bin/seq"',
]) {
  if (!readme.includes(required)) errors.push(`README.md: missing fresh-machine prerequisite contract ${required}`);
}
for (const required of [
  "observationGeneration",
  "beginGeneration",
  "isCurrentGeneration",
  "markUnconfirmed",
  "validateCreateResponse",
  "validateElectionState",
  "invalid election stream response",
]) {
  if (!siteScript.includes(required)) errors.push(`assets/site.js: missing observer safety contract ${required}`);
}

for (const line of orchestrationCompatibility.split("\n").filter((candidate) => /orchestration/i.test(candidate))) {
  if (!/coordination/i.test(line)) {
    errors.push("docs/agent-orchestration.html: every orchestration claim must be immediately qualified as coordination");
    break;
  }
}

const dashboardAuthContract = [
  "fragmentParams.get('exchange_code')",
  "fragmentParams.get('issuer')",
  "history.replaceState(null, '', window.location.pathname)",
  "apiUrl(apiBase, '/auth/github/exchange')",
  "method: 'POST'",
  "'Content-Type': 'application/json'",
  "JSON.stringify({ exchange_code: exchangeCode })",
  "persistAuthResponse(await response.json(), apiBase)",
  "validateAuthTokenResponse",
  "localStorage.setItem('octostore_auth', serialized)",
  "localStorage.getItem('octostore_auth')",
  "stored.github_username || stored.username",
  "stored.user_id || stored.userId",
  "stored.api_base || HOSTED_API_BASE",
];
for (const required of dashboardAuthContract) {
  if (!dashboard.includes(required)) {
    errors.push(`dashboard.html: missing one-time auth exchange contract ${required}`);
  }
}

const credentialBearingSurfaces = new Map(
  [...markupByFile.entries()].flatMap(([file, markup]) => {
    const inlineRuntime = [...markup.matchAll(/<script\b(?![^>]*\bsrc\s*=)[^>]*>([\s\S]*?)<\/script>/gi)]
      .map((match) => match[1])
      .join("\n");
    if (!/\b(?:localStorage|sessionStorage|Authorization|X-Admin-Key|adminToken)\b/.test(inlineRuntime)) {
      return [];
    }
    return [[path.relative(siteRoot, file).split(path.sep).join("/"), markup]];
  }),
);
for (const required of ["dashboard.html", "admin.html"]) {
  if (!credentialBearingSurfaces.has(required)) {
    errors.push(`${required}: expected credential-bearing surface was not discovered by the site contract`);
  }
}
for (const [label, markup] of credentialBearingSurfaces) {
  const cspTag = markup.match(/<meta\b[^>]*http-equiv\s*=\s*(["'])Content-Security-Policy\1[^>]*>/i)?.[0];
  const csp = cspTag && attributeValues(cspTag, "content")[0];
  if (!csp || !csp.includes("default-src 'none'") || !csp.includes("object-src 'none'") ||
      !csp.includes("base-uri 'none'") || /script-src[^;]*(?:'unsafe-inline'|'unsafe-eval'|https?:)/i.test(csp)) {
    errors.push(`${label}: credential-bearing surface requires a restrictive script CSP`);
    continue;
  }

  for (const scriptTag of markup.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    const source = attributeValues(scriptTag[0], "src")[0];
    if (source) {
      const scriptUrl = new URL(source, new URL(publicPath(path.join(siteRoot, label)), siteOrigin));
      if (scriptUrl.origin !== siteOrigin) {
        errors.push(`${label}: credential-bearing surface loads external runtime JavaScript ${source}`);
      }
      continue;
    }

    const digest = crypto.createHash("sha256").update(scriptTag[2]).digest("base64");
    if (!csp.includes(`'sha256-${digest}'`)) {
      errors.push(`${label}: inline credential-bearing script is not pinned by its CSP hash`);
    }
  }
}

for (const forbidden of [
  "cdn.jsdelivr.net",
  "new Chart(",
  "onclick=",
  "innerHTML",
  "localStorage.setItem('adminToken', hash)",
  "https://api.octostore.io/admin/metrics'",
  "https://api.octostore.io/admin/telemetry",
]) {
  if (adminDashboard.includes(forbidden)) {
    errors.push(`admin.html: legacy credential surface contains forbidden runtime pattern ${forbidden}`);
  }
}
for (const required of [
  "this._api('/admin/status')",
  "this._api(`/admin/metrics/timeseries?window=${this.currentWindow}`)",
  "this._api('/metrics')",
  "normalizeApiBase(gh.api_base || HOSTED_API_BASE)",
  "if (!response.ok)",
  "_validateAdminStatus",
  "_validateTimeseries",
  "_validateMetrics",
]) {
  if (!adminDashboard.includes(required)) {
    errors.push(`admin.html: missing validated endpoint contract ${required}`);
  }
}

for (const required of [
  'http-equiv="refresh" content="0; url=/admin.html"',
  'rel="canonical" href="https://octostore.io/admin.html"',
  'href="/admin.html"',
]) {
  if (!legacyStatus.includes(required)) {
    errors.push(`status.html: missing no-script admin compatibility contract ${required}`);
  }
}
for (const forbidden of ["<script", "localStorage", "adminKey", "cdn.jsdelivr.net", "innerHTML"]) {
  if (legacyStatus.includes(forbidden)) {
    errors.push(`status.html: retired credential surface contains forbidden runtime pattern ${forbidden}`);
  }
}

for (const required of ["validateLockListResponse", "Holder pseudonym", "Capability-free status"]) {
  if (!dashboard.includes(required)) {
    errors.push(`dashboard.html: missing documented capability-free lock-list contract ${required}`);
  }
}
for (const required of [
  "isRfc3339DateTime",
  "validateAuthTokenResponse",
  "persistAuthResponse",
  "authData = nextAuth",
]) {
  if (!dashboard.includes(required)) {
    errors.push(`dashboard.html: missing strict credential/schema contract ${required}`);
  }
}
if (dashboard.includes("Date.parse(")) {
  errors.push("dashboard.html: Date.parse-only timestamp acceptance is forbidden");
}
if (siteScript.includes("Date.parse(")) {
  errors.push("assets/site.js: Date.parse-only timestamp acceptance is forbidden");
}
for (const forbidden of ["lock.lease_id", "releaseLock(", "onclick=\"releaseLock"] ) {
  if (dashboard.includes(forbidden)) {
    errors.push(`dashboard.html: list responses cannot power lease capability controls ${forbidden}`);
  }
}
for (const forbidden of [
  "Authorization: Bearer YOUR_TOKEN",
  "updateExamples(token",
  "${safeToken}",
  "tokenTextEl.textContent = token",
  "pre-filled token",
]) {
  if (dashboard.includes(forbidden)) {
    errors.push(`dashboard.html: issued credentials must not enter rendered DOM examples via ${forbidden}`);
  }
}

const clearUrlIndex = dashboard.indexOf("history.replaceState(null, '', window.location.pathname)");
const exchangeIndex = dashboard.indexOf("apiUrl(apiBase, '/auth/github/exchange')");
const storeAuthIndex = dashboard.indexOf("localStorage.setItem('octostore_auth', serialized)");
if (!(clearUrlIndex >= 0 && clearUrlIndex < exchangeIndex && exchangeIndex < storeAuthIndex)) {
  errors.push("dashboard.html: URL state must be cleared before the one-time exchange response is stored");
}

for (const forbidden of [
  "getTokenFromUrl",
  "window.location.search",
  ".get('token')",
  '.get("token")',
  ".get('username')",
  '.get("username")',
  ".get('user_id')",
  '.get("user_id")',
]) {
  if (dashboard.includes(forbidden)) {
    errors.push(`dashboard.html: forbidden legacy URL credential parsing ${forbidden}`);
  }
}

const credentialName = /\b(?:exchangeCode|exchange_code|token|username|userId|user_id|authData|authInfo|stored|exchanged)\b/i;
for (const consoleCall of dashboard.matchAll(/console\.[a-z]+\(([\s\S]*?)\);/gi)) {
  const argumentsText = consoleCall[1];
  const interpolation = /\$\{([^}]*)\}/g;
  const interpolatedCredential = [...argumentsText.matchAll(interpolation)].some((match) => credentialName.test(match[1]));
  const expressionText = argumentsText
    .replace(/'(?:\\.|[^'\\])*'/g, "")
    .replace(/"(?:\\.|[^"\\])*"/g, "")
    .replace(/`(?:\\.|[^`\\])*`/g, "");
  if (interpolatedCredential || credentialName.test(expressionText)) {
    errors.push("dashboard.html: credential-shaped values must never be written to the console");
    break;
  }
}

if (errors.length > 0) {
  process.stderr.write(`${errors.join("\n")}\n`);
  process.exit(1);
}

process.stdout.write(
  `site contract passed: ${htmlFiles.length} HTML files, routes/assets, accessibility structure, pinned onboarding, compatibility claim boundaries, CSP-pinned credential surfaces and rotation, capability-free lock lists, strict RFC3339 live demo schemas, and social card\n`,
);
