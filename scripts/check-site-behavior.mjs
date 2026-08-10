#!/usr/bin/env node

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const source = fs.readFileSync(path.join(repositoryRoot, "site", "assets", "site.js"), "utf8");
const dashboardMarkup = fs.readFileSync(path.join(repositoryRoot, "site", "dashboard.html"), "utf8");
const dashboardScript = dashboardMarkup.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(dashboardScript, "dashboard must contain an inline application script");
const adminMarkup = fs.readFileSync(path.join(repositoryRoot, "site", "admin.html"), "utf8");
const adminScript = adminMarkup.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(adminScript, "admin dashboard must contain an inline application script");

class FakeElement {
  constructor({ dataset = {}, textContent = "" } = {}) {
    this.dataset = dataset;
    this.textContent = textContent;
    this.hidden = false;
    this.disabled = false;
    this.listeners = new Map();
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  async emit(type, event = {}) {
    for (const listener of this.listeners.get(type) ?? []) await listener(event);
  }
}

class FakeField extends FakeElement {
  constructor(id, candidateId) {
    super({ dataset: { candidateId } });
    this.id = id;
    this.value = "";
    this.focused = false;
    this.selected = false;
  }

  focus() {
    this.focused = true;
  }

  select() {
    this.selected = true;
  }
}

const roomLabel = new FakeElement({ textContent: "room not created" });
const status = new FakeElement({ textContent: "not started" });
const handoff = new FakeElement();
handoff.hidden = true;
const atlasField = new FakeField("agent-atlas-instruction", "agent-atlas");
const cometField = new FakeField("agent-comet-instruction", "agent-comet");
const fields = [atlasField, cometField];
const fieldBySelector = new Map(fields.map((field) => [`#${field.id}`, field]));

function candidate(id) {
  const stateLabel = new FakeElement({ textContent: "READY" });
  const row = new FakeElement({ dataset: { candidate: id, state: "ready" } });
  row.querySelector = (selector) => (selector === "[data-candidate-state]" ? stateLabel : null);
  row.stateLabel = stateLabel;
  return row;
}

const candidates = [candidate("agent-atlas"), candidate("agent-comet")];
const heroButton = new FakeElement({ textContent: "Create one shared room" });
const boardButton = new FakeElement({ textContent: "Create one shared room" });
const atlasCopy = new FakeElement({
  dataset: { copyTarget: "#agent-atlas-instruction" },
  textContent: "Copy for agent Atlas",
});
const cometCopy = new FakeElement({
  dataset: { copyTarget: "#agent-comet-instruction" },
  textContent: "Copy for agent Comet",
});

const demo = new FakeElement({ dataset: { apiBase: "https://api.example.test" } });
demo.querySelector = (selector) =>
  ({
    "[data-election-room]": roomLabel,
    "[data-race-status]": status,
    "[data-agent-handoff]": handoff,
  })[selector] ?? null;
demo.querySelectorAll = (selector) => {
  if (selector === "[data-agent-instruction]") return fields;
  if (selector === "[data-candidate]") return candidates;
  return [];
};
demo.contains = (element) => element === boardButton;
demo.scrollIntoView = () => {};

const pageListeners = new Map();
const timers = new Map();
let nextTimer = 1;
const windowObject = {
  addEventListener(type, listener) {
    pageListeners.set(type, listener);
  },
  setTimeout(callback, delay) {
    const id = nextTimer++;
    timers.set(id, { callback, delay, repeating: false });
    return id;
  },
  clearTimeout(id) {
    timers.delete(id);
  },
  setInterval(callback, delay) {
    const id = nextTimer++;
    timers.set(id, { callback, delay, repeating: true });
    return id;
  },
  clearInterval(id) {
    timers.delete(id);
  },
};

const documentObject = {
  querySelector(selector) {
    if (selector === "[data-election-demo]") return demo;
    if (selector === "[data-current-year]") return null;
    return fieldBySelector.get(selector) ?? null;
  },
  querySelectorAll(selector) {
    if (selector === "[data-copy-target]") return [atlasCopy, cometCopy];
    if (selector === "[data-run-election]") return [heroButton, boardButton];
    if (selector === ".site-nav" || selector === "[data-copy-instruction]") return [];
    return [];
  },
};

class FakeEventSource {
  static instances = [];

  constructor(url) {
    this.url = url;
    this.closed = false;
    this.listeners = new Map();
    FakeEventSource.instances.push(this);
  }

  addEventListener(type, listener) {
    this.listeners.set(type, listener);
  }

  close() {
    this.closed = true;
  }

  emit(type, data) {
    this.listeners.get(type)?.({ data: JSON.stringify(data) });
  }
}

windowObject.EventSource = FakeEventSource;
const fetchQueue = [];
const fetch = async () => {
  const next = fetchQueue.shift();
  if (next instanceof Error) throw next;
  assert.ok(next, "test did not provide a fetch response");
  return {
    ok: true,
    status: 200,
    headers: { get: () => null },
    json: async () => next,
  };
};

const navigatorObject = {
  clipboard: {
    writeText: async () => {
      throw new Error("clipboard permission denied");
    },
  },
};

vm.runInNewContext(source, {
  AbortController,
  EventSource: FakeEventSource,
  console,
  document: documentObject,
  fetch,
  navigator: navigatorObject,
  window: windowObject,
});

const flushAsync = () => new Promise((resolve) => setImmediate(resolve));

fetchQueue.push({
  election_id: "room-one",
  campaign_path: "/elections/room-one/campaign",
  status_path: "/elections/room-one",
  watch_path: "/elections/room-one/watch",
});
await boardButton.emit("click");

assert.equal(handoff.hidden, false, "successful creation must reveal both instructions");
assert.match(atlasField.value, /room-one.*agent-atlas/, "Atlas instruction must include room and candidate");
assert.match(cometField.value, /room-one.*agent-comet/, "Comet instruction must include room and candidate");
assert.equal(FakeEventSource.instances.length, 1, "successful creation must start one observer");

await cometCopy.emit("click");
assert.equal(cometCopy.textContent, "Select to copy");
assert.equal(cometField.focused, true, "clipboard denial must focus Comet's visible field");
assert.equal(cometField.selected, true, "clipboard denial must select Comet's visible field");

const oldStream = FakeEventSource.instances[0];
assert.equal(
  Number.isFinite(Date.parse("2026-02-30T12:00:00Z")),
  true,
  "regression fixture must remain a Date.parse-accepted invalid calendar date",
);
assert.equal(
  Number.isFinite(Date.parse("2026-08-03 12:00:00Z")),
  true,
  "regression fixture must remain Date.parse-accepted but non-RFC3339",
);
oldStream.emit("state", {
  schema_version: 1,
  election_id: "room-one",
  status: "leader",
  leader: {
    candidate_id: "agent-atlas",
    term: 1,
    expires_at: "2026-08-03T12:00:00Z",
    metadata: null,
  },
  retry_after_ms: 1000,
  observed_at: "2026-08-03T11:59:45Z",
});
assert.equal(candidates[0].dataset.state, "leader", "valid SSE state must be rendered");

fetchQueue.push({
  election_id: "room-one",
  status: "vacant",
  retry_after_ms: 0,
});
oldStream.emit("state", {
  election_id: "room-one",
  status: "leader",
  leader: {
    candidate_id: "agent-atlas",
    term: 2,
    expires_at: "2026-08-03T12:00:60Z",
    metadata: null,
  },
  retry_after_ms: 1000,
  schema_version: 1,
  observed_at: "2026-08-03T12:00:00Z",
});
assert.equal(candidates[0].dataset.state, "unconfirmed", "RFC3339 second 60 must invalidate SSE state");
assert.match(status.textContent, /invalid election state.*no longer confirms/i);
await flushAsync();
assert.equal(candidates[0].dataset.state, "shared", "a valid status read may restore confirmation");

const polling = [...timers.values()].find((timer) => timer.repeating && timer.delay === 2000);
assert.ok(polling, "malformed SSE must fall back to bounded status polling");
fetchQueue.push({
  election_id: "room-one",
  status: "leader",
  leader: {
    candidate_id: "agent-atlas",
    term: 3,
    expires_at: "2026-08-03T12:00:60Z",
    metadata: null,
  },
  retry_after_ms: 1000,
});
await polling.callback();
assert.equal(candidates[0].dataset.state, "unconfirmed", "RFC3339 second 60 in HTTP status must be unconfirmed");
assert.match(status.textContent, /invalid election leader response/i);

fetchQueue.push({
  election_id: "room-one",
  status: "leader",
  leader: {
    candidate_id: "agent-atlas",
    term: 4,
    expires_at: "2026-08-03 12:00:00Z",
    metadata: null,
  },
  retry_after_ms: 1000,
});
await polling.callback();
assert.equal(candidates[0].dataset.state, "unconfirmed", "non-RFC3339 HTTP-200 status must be unconfirmed");
assert.match(status.textContent, /invalid election leader response/i);

fetchQueue.push({});
await boardButton.emit("click");
assert.equal(candidates[0].dataset.state, "unavailable", "malformed HTTP-200 create must be unavailable");
assert.equal(handoff.hidden, true, "malformed create must not expose agent instructions");
assert.match(status.textContent, /invalid election create response/i);

fetchQueue.push(new Error("offline"));
await boardButton.emit("click");
assert.equal(oldStream.closed, true, "a new create attempt must leave the previous observer closed");
assert.equal(candidates[0].dataset.state, "unavailable");
const failedStatus = status.textContent;
oldStream.emit("state", {
  leader: { candidate_id: "agent-atlas", term: 99 },
});
assert.equal(status.textContent, failedStatus, "stale observer callbacks must not overwrite failure state");
assert.equal(candidates[0].dataset.state, "unavailable");

fetchQueue.push({
  election_id: "room-two",
  campaign_path: "/elections/room-two/campaign",
  status_path: "/elections/room-two",
  watch_path: "/elections/room-two/watch",
});
await boardButton.emit("click");
const currentStream = FakeEventSource.instances.at(-1);
const observationTimer = [...timers.values()].find((timer) => timer.delay === 15 * 60 * 1000);
assert.ok(observationTimer, "a bounded observation timer must be active");
observationTimer.callback();
assert.equal(currentStream.closed, true, "bounded observation must close its stream");
assert.equal(candidates[0].dataset.state, "unconfirmed");
assert.match(status.textContent, /observation ended.*no longer confirms/i);
const endedStatus = status.textContent;
currentStream.emit("state", {
  leader: { candidate_id: "agent-comet", term: 100 },
});
assert.equal(status.textContent, endedStatus, "ended observations must reject late callbacks");

assert.equal(typeof pageListeners.get("pagehide"), "function", "pagehide must invalidate observation");

function dashboardHarness({
  hash,
  search = "",
  storedAuth = null,
  exchangeResponse = null,
  rotateResponse = null,
  locksResponse = null,
}) {
  const storage = new Map();
  if (storedAuth) storage.set("octostore_auth", JSON.stringify(storedAuth));
  const requests = [];
  let clearedPath = null;
  const element = {
    addEventListener() {},
    appendChild() {},
    classList: { add() {}, remove() {} },
    disabled: false,
    hidden: false,
    innerHTML: "",
    remove() {},
    textContent: "",
    style: {},
  };
  const elements = new Map();
  const elementById = (id) => {
    if (!elements.has(id)) elements.set(id, { ...element, classList: { ...element.classList } });
    return elements.get(id);
  };
  const location = {
    hash,
    pathname: "/dashboard.html",
    search,
    href: "",
    replace() {},
  };
  const context = vm.createContext({
    URL,
    URLSearchParams,
    console: { error() {} },
    document: {
      addEventListener() {},
      body: element,
      createElement: () => ({ ...element }),
      getElementById: elementById,
    },
    fetch: async (url, options) => {
      requests.push({ url, options, clearedPath });
      let body;
      if (url.endsWith("/auth/github/exchange")) {
        assert.ok(exchangeResponse, "legacy URL credentials must not trigger an exchange");
        body = exchangeResponse;
      } else if (url.endsWith("/auth/token/rotate")) {
        assert.ok(rotateResponse, "test did not provide a token rotation response");
        body = rotateResponse;
      } else if (url.endsWith("/locks")) {
        assert.ok(locksResponse, "test did not provide a lock-list response");
        body = locksResponse;
      } else {
        assert.fail(`unexpected dashboard request: ${url}`);
      }
      return {
        ok: true,
        json: async () => body,
      };
    },
    history: {
      replaceState(_state, _title, path) {
        clearedPath = path;
        location.hash = "";
        location.search = "";
      },
    },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      removeItem: (key) => storage.delete(key),
      setItem: (key, value) => storage.set(key, value),
    },
    navigator: { clipboard: { writeText: async () => {} } },
    setInterval: () => 1,
    setTimeout: () => 1,
    window: { location },
  });
  vm.runInContext(dashboardScript, context);
  return {
    authenticate: () => vm.runInContext("getAuthForDashboard()", context),
    requests,
    renderLockList(response) {
      context.lockListFixture = response;
      vm.runInContext(
        "userLocks = validateLockListResponse(lockListFixture); renderLocks();",
        context,
      );
      return elementById("locks-list").innerHTML;
    },
    async rotate(auth) {
      context.authFixture = auth;
      return vm.runInContext("authData = authFixture; rotateToken()", context);
    },
    async loadLocks(auth) {
      context.authFixture = auth;
      return vm.runInContext("authData = authFixture; loadUserLocks()", context);
    },
    renderAuthenticatedSurface(auth) {
      context.authFixture = auth;
      vm.runInContext(
        "authData = authFixture; updateUserInfo(authFixture.username, authFixture.userId); " +
          "updateExamples(authFixture.apiBase);",
        context,
      );
      return [...elements.values()].map((candidate) =>
        `${candidate.textContent}\n${candidate.innerHTML}`).join("\n");
    },
    renderProtectedWorkExample(auth) {
      context.authFixture = auth;
      vm.runInContext("authData = authFixture; updateExamples(authFixture.apiBase);", context);
      return elementById("example-status").innerHTML
        .replace(/<[^>]*>/g, "")
        .replaceAll("&quot;", '"')
        .replaceAll("&amp;", "&");
    },
    authState: () => vm.runInContext("authData", context),
    storage,
    wasCleared: () => clearedPath === "/dashboard.html",
  };
}

const exchangedAuth = {
  token: "api-secret",
  user_id: "550e8400-e29b-41d4-a716-446655440000",
  github_username: "octocat",
  namespace: "github-namespace",
};
const freshDashboard = dashboardHarness({
  hash: "#exchange_code=single-use-code&issuer=http%3A%2F%2F127.0.0.1%3A4100&token=legacy-fragment-secret",
  search: "?token=legacy-query-secret&username=legacy-query-user",
  exchangeResponse: exchangedAuth,
  locksResponse: { locks: [], total: 0, prefix: null },
});
const freshAuth = await freshDashboard.authenticate();
assert.equal(freshDashboard.requests.length, 1, "exchange_code must trigger one exchange request");
assert.equal(freshDashboard.requests[0].url, "http://127.0.0.1:4100/auth/github/exchange");
assert.equal(freshDashboard.requests[0].options.method, "POST");
assert.equal(freshDashboard.requests[0].options.headers["Content-Type"], "application/json");
assert.deepEqual(JSON.parse(freshDashboard.requests[0].options.body), { exchange_code: "single-use-code" });
assert.equal(freshDashboard.requests[0].clearedPath, "/dashboard.html", "URL state must clear before exchange");
assert.equal(freshAuth.token, exchangedAuth.token);
assert.equal(freshAuth.username, exchangedAuth.github_username);
assert.equal(freshAuth.userId, exchangedAuth.user_id);
assert.equal(freshAuth.apiBase, "http://127.0.0.1:4100");
assert.deepEqual(JSON.parse(freshDashboard.storage.get("octostore_auth")), {
  ...exchangedAuth,
  api_base: "http://127.0.0.1:4100",
});
await freshDashboard.loadLocks(freshAuth);
const authenticatedDashboardDom = freshDashboard.renderAuthenticatedSurface(freshAuth);
assert.doesNotMatch(
  authenticatedDashboardDom,
  /api-secret|Authorization:\s*Bearer/i,
  "the issued bearer token must never be inserted into authenticated dashboard DOM examples",
);
assert.match(authenticatedDashboardDom, /OCTOSTORE_TOKEN_FILE/);
assert.match(authenticatedDashboardDom, /--acquire-timeout 60/);
const authenticatedProtectedWorkExample = freshDashboard.renderProtectedWorkExample(freshAuth);
assert.match(authenticatedProtectedWorkExample, /^AGENT_WORKER=\.\/run-agent$/m);
assert.match(
  authenticatedProtectedWorkExample,
  /^octostore-supervisor lock "repo\/octostore\/issue-1842" - "\$AGENT_WORKER" -- \\$/m,
);
assert.match(authenticatedProtectedWorkExample, /^  octostore lock hold "repo\/octostore\/issue-1842" \\$/m);
assert.match(authenticatedProtectedWorkExample, /^    --ttl 120 --acquire-timeout 60 --json$/m);
assert.doesNotMatch(
  authenticatedProtectedWorkExample,
  /^octostore lock hold\b/m,
  "authenticated dashboard must never regenerate a bare protected-work hold",
);
assert.equal(
  freshDashboard.requests.at(-1).url,
  "http://127.0.0.1:4100/locks",
  "subsequent dashboard API requests must remain bound to the handoff issuer",
);

const returningDashboard = dashboardHarness({
  hash: "#token=legacy-fragment-secret&username=legacy-fragment-user&user_id=legacy-user-id",
  search: "?token=legacy-query-secret&username=legacy-query-user&user_id=legacy-query-id",
  storedAuth: { token: "stored-secret", username: "returning-user", userId: "returning-id" },
});
const returningAuth = await returningDashboard.authenticate();
assert.equal(returningDashboard.requests.length, 0, "legacy URL credentials must never trigger an exchange");
assert.equal(returningDashboard.wasCleared(), true, "legacy URL credentials must be scrubbed");
assert.equal(returningAuth.token, "stored-secret", "returning visitor must restore localStorage token");
assert.equal(returningAuth.username, "returning-user");
assert.equal(returningAuth.userId, "returning-id");
assert.equal(returningAuth.apiBase, "https://api.octostore.io");

const rotatedAuth = {
  token: "rotated-secret",
  user_id: "550e8400-e29b-41d4-a716-446655440000",
  github_username: "returning-user",
  namespace: "github-namespace",
};
const rotationDashboard = dashboardHarness({
  hash: "",
  storedAuth: { ...exchangedAuth, api_base: "https://self-hosted.example.test" },
  rotateResponse: rotatedAuth,
});
const beforeRotation = await rotationDashboard.authenticate();
await rotationDashboard.rotate(beforeRotation);
const rotationRequest = rotationDashboard.requests.at(-1);
assert.equal(rotationRequest.url, "https://self-hosted.example.test/auth/token/rotate");
assert.equal(rotationRequest.options.method, "POST");
assert.equal(rotationRequest.options.headers.Authorization, "Bearer api-secret");
assert.deepEqual(
  JSON.parse(rotationDashboard.storage.get("octostore_auth")),
  { ...rotatedAuth, api_base: "https://self-hosted.example.test" },
  "successful rotation must replace the complete persisted auth record",
);
assert.equal(rotationDashboard.authState().token, rotatedAuth.token);

const reloadedDashboard = dashboardHarness({
  hash: "",
  storedAuth: JSON.parse(rotationDashboard.storage.get("octostore_auth")),
});
const reloadedAuth = await reloadedDashboard.authenticate();
assert.equal(reloadedAuth.token, rotatedAuth.token, "reload must restore the rotated token");
assert.equal(reloadedAuth.username, rotatedAuth.github_username);
assert.equal(reloadedAuth.userId, rotatedAuth.user_id);
assert.equal(reloadedAuth.apiBase, "https://self-hosted.example.test");

const malformedRotationDashboard = dashboardHarness({
  hash: "",
  storedAuth: { ...exchangedAuth, api_base: "https://api.octostore.io" },
  rotateResponse: {
    token: "replacement-with-missing-identity",
    user_id: exchangedAuth.user_id,
    namespace: null,
  },
});
const malformedBefore = await malformedRotationDashboard.authenticate();
await malformedRotationDashboard.rotate(malformedBefore);
assert.deepEqual(
  JSON.parse(malformedRotationDashboard.storage.get("octostore_auth")),
  { ...exchangedAuth, api_base: "https://api.octostore.io" },
  "malformed rotation response must not partially replace persisted auth",
);

const missingIssuerDashboard = dashboardHarness({
  hash: "#exchange_code=unbound-code",
  exchangeResponse: exchangedAuth,
});
await assert.rejects(
  missingIssuerDashboard.authenticate(),
  /missing its code or issuer/,
  "a handoff without an issuing authority must fail closed",
);
assert.equal(missingIssuerDashboard.requests.length, 0);

const unsafeIssuerDashboard = dashboardHarness({
  hash: "#exchange_code=unsafe-code&issuer=https%3A%2F%2Fapi.example.test%2Fnested",
  exchangeResponse: exchangedAuth,
});
await assert.rejects(
  unsafeIssuerDashboard.authenticate(),
  /OAuth issuer/,
  "an issuer with path ambiguity must fail before the exchange",
);
assert.equal(unsafeIssuerDashboard.requests.length, 0);
assert.equal(
  malformedRotationDashboard.authState().token,
  exchangedAuth.token,
  "malformed rotation response must not partially replace in-memory auth",
);

const documentedLockList = {
  locks: [
    {
      name: "repos/octostore/issue-42",
      status: "held",
      holder_id: "550e8400-e29b-41d4-a716-446655440000",
      fencing_token: 42,
      expires_at: "2026-08-03T12:00:00Z",
      metadata: "task=42",
    },
  ],
  total: 1,
  prefix: null,
};
const lockMarkup = returningDashboard.renderLockList(documentedLockList);
assert.match(lockMarkup, /repos\/octostore\/issue-42/);
assert.match(lockMarkup, /Status: held/);
assert.match(lockMarkup, /Holder pseudonym/);
assert.match(lockMarkup, /Fencing Token: <code>42<\/code>/);
assert.doesNotMatch(lockMarkup, /Lease ID|releaseLock|>\s*Release\s*</i);
assert.doesNotMatch(lockMarkup, /undefined/);
assert.throws(
  () => returningDashboard.renderLockList({ locks: [{ name: "missing-contract-fields" }], total: 1, prefix: null }),
  /Invalid lock status response/,
  "dashboard must reject rows that do not match documented LockStatus",
);
assert.throws(
  () => returningDashboard.renderLockList({
    locks: [{ ...documentedLockList.locks[0], expires_at: "2026-02-30T12:00:00Z" }],
    total: 1,
    prefix: null,
  }),
  /Invalid lock status response/,
  "dashboard must reject calendar-invalid timestamps even when Date.parse accepts them",
);
assert.throws(
  () => returningDashboard.renderLockList({
    locks: [{ ...documentedLockList.locks[0], expires_at: "2026-08-03T12:00:60Z" }],
    total: 1,
    prefix: null,
  }),
  /Invalid lock status response/,
  "dashboard must reject RFC3339 second 60",
);

function adminHarness() {
  const requests = [];
  const responses = [];
  const context = vm.createContext({
    URL,
    console,
    document: { addEventListener() {} },
    fetch: async (url, options) => {
      requests.push({ url, options });
      const response = responses.shift();
      assert.ok(response, `test did not provide an admin response for ${url}`);
      return {
        ok: response.ok ?? true,
        status: response.status ?? 200,
        json: async () => response.body,
      };
    },
  });
  vm.runInContext(adminScript, context);
  vm.runInContext(
    "globalThis.adminFixtureApp = Object.create(AdminApp.prototype);" +
      "adminFixtureApp.token = 'admin-secret'; adminFixtureApp.currentWindow = '1h';",
    context,
  );
  return {
    async fetchValidated(url, label, validator) {
      context.adminFixtureUrl = url;
      context.adminFixtureLabel = label;
      context.adminFixtureValidator = validator;
      return vm.runInContext(
        "adminFixtureApp._fetchJson(adminFixtureUrl, adminFixtureLabel, " +
          "adminFixtureApp[adminFixtureValidator])",
        context,
      );
    },
    apiUrl(apiBase, path) {
      context.adminFixtureBase = apiBase;
      context.adminFixturePath = path;
      return vm.runInContext(
        "adminFixtureApp.apiBase = adminFixtureBase; adminFixtureApp._api(adminFixturePath)",
        context,
      );
    },
    requests,
    responses,
  };
}

const admin = adminHarness();
assert.equal(
  admin.apiUrl("https://self-hosted.example.test", "/admin/status"),
  "https://self-hosted.example.test/admin/status",
  "OAuth-backed admin requests must retain the stored issuing authority",
);
const validAdminStatus = {
  healthy: true,
  uptime_seconds: 60,
  active_locks: 1,
  total_users: 1,
  total_acquires: 3,
  total_releases: 2,
  users: [{
    id: "550e8400-e29b-41d4-a716-446655440000",
    github_username: "octocat",
    created_at: "2026-08-03T12:00:00Z",
  }],
  locks: [{
    name: "repos/octostore/release",
    holder_username: "octocat",
    metadata: null,
    fencing_token: 9,
    expires_at: "2026-08-03T12:01:00Z",
    ttl_remaining_seconds: 45,
  }],
};
admin.responses.push({ body: validAdminStatus });
const checkedStatus = await admin.fetchValidated(
  "https://api.octostore.io/admin/status", "admin status", "_validateAdminStatus",
);
assert.equal(checkedStatus.total_users, 1);
assert.equal(admin.requests.at(-1).options.headers.Authorization, "Bearer admin-secret");
assert.equal(admin.requests.at(-1).options.headers["X-Admin-Key"], "admin-secret");

admin.responses.push({ status: 403, ok: false, body: validAdminStatus });
await assert.rejects(
  admin.fetchValidated(
    "https://api.octostore.io/admin/status", "admin status", "_validateAdminStatus",
  ),
  /admin status request failed \(403\)/,
  "admin dashboard must reject non-success HTTP responses before rendering their body",
);

const validTimeseries = {
  window: "1h",
  timestamps: [1, 2],
  request_volume: [3, 4],
  acquire_rates: [1, 1],
  release_rates: [0, 1],
  active_locks: [1, 0],
};
admin.responses.push({ body: validTimeseries });
const checkedTimeseries = await admin.fetchValidated(
  "https://api.octostore.io/admin/metrics/timeseries?window=1h",
  "metrics time series",
  "_validateTimeseries",
);
assert.equal(checkedTimeseries.timestamps.length, 2);
admin.responses.push({ body: { ...validTimeseries, release_rates: [0] } });
await assert.rejects(
  admin.fetchValidated(
    "https://api.octostore.io/admin/metrics/timeseries?window=1h",
    "metrics time series",
    "_validateTimeseries",
  ),
  /metrics time-series response did not match the expected schema/,
  "admin dashboard must reject misaligned chart arrays",
);

const endpointMetrics = {
  count: 3,
  avg_ms: "1.0",
  p50_ms: "1.0",
  p95_ms: "2.0",
  p99_ms: "3.0",
  errors: 0,
  min_ms: 1,
  max_ms: 3,
};
const validMetrics = {
  uptime_seconds: 60,
  total_requests: 3,
  requests_per_second: 0.05,
  active_locks: 1,
  total_users: 1,
  endpoints: { acquire: endpointMetrics },
  lock_store: {
    total_acquires: 3,
    total_releases: 2,
    total_expirations: 0,
    cache_hits: 0,
    cache_misses: 0,
  },
  memory_bytes: 1024,
};
admin.responses.push({ body: validMetrics });
const checkedMetrics = await admin.fetchValidated(
  "https://api.octostore.io/metrics", "metrics snapshot", "_validateMetrics",
);
assert.equal(checkedMetrics.lock_store.total_acquires, 3);
assert.equal(
  admin.requests.at(-1).url,
  "https://api.octostore.io/metrics",
  "admin dashboard must use the implemented metrics endpoint",
);
admin.responses.push({ body: { ...validMetrics, endpoints: { acquire: { count: "3" } } } });
await assert.rejects(
  admin.fetchValidated(
    "https://api.octostore.io/metrics", "metrics snapshot", "_validateMetrics",
  ),
  /metrics snapshot response did not match the expected schema/,
  "admin dashboard must reject malformed HTTP-200 metric snapshots",
);

process.stdout.write(
  "site behavior passed: strict RFC3339 create/status/SSE and lock schemas, observer invalidation, bounded unconfirmed state, selectable dual instructions, one-time auth exchange, durable validated token rotation, legacy URL rejection, capability-free lock rendering, and status-aware validated admin metrics\n",
);
