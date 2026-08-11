import fs from "node:fs";
import path from "node:path";

import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

const evidenceRoot = process.env.OCTOSTORE_BROWSER_EVIDENCE_DIR;

function captureRuntimeFailures(page) {
  const failures = [];
  page.on("console", (message) => {
    if (message.type() === "error") failures.push(`console: ${message.text()}`);
  });
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("requestfailed", (request) => {
    failures.push(`requestfailed: ${request.method()} ${request.url()} ${request.failure()?.errorText ?? "unknown"}`);
  });
  return failures;
}

async function screenshot(page, name) {
  if (!evidenceRoot) return;
  const destination = path.resolve(evidenceRoot, `${name}.png`);
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  await page.screenshot({ path: destination, animations: "disabled" });
}

async function assertNoHorizontalOverflow(page) {
  const dimensions = await page.evaluate(() => ({
    clientWidth: document.documentElement.clientWidth,
    scrollWidth: document.documentElement.scrollWidth,
  }));
  expect(dimensions.scrollWidth, JSON.stringify(dimensions)).toBeLessThanOrEqual(dimensions.clientWidth + 1);
}

async function assertNoWcagViolations(page) {
  const results = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"])
    .analyze();
  expect(results.violations, JSON.stringify(results.violations, null, 2)).toEqual([]);
}

for (const surface of [
  { name: "home", path: "/", heading: "Stop two agents from doing the same work." },
  { name: "agents", path: "/agents/", heading: "Stop two agents from doing the same work." },
]) {
  for (const viewport of [
    { name: "desktop", width: 1440, height: 1000 },
    { name: "narrow", width: 390, height: 844 },
  ]) {
    test(`${surface.name} passes ${viewport.name} first-viewport and accessibility checks`, async ({ browser }) => {
      const context = await browser.newContext({
        viewport: { width: viewport.width, height: viewport.height },
        reducedMotion: "reduce",
      });
      const page = await context.newPage();
      const failures = captureRuntimeFailures(page);

      await page.goto(surface.path, { waitUntil: "networkidle" });
      await expect(page.getByRole("heading", { level: 1, name: surface.heading })).toBeVisible();
      if (surface.name === "home") {
        const wire = await page.locator(".coordination-stage").boundingBox();
        expect(wire, "homepage coordination wire must render").not.toBeNull();
        expect(wire.y, JSON.stringify(wire)).toBeLessThan(viewport.height);
        expect(wire.y + wire.height, JSON.stringify(wire)).toBeGreaterThan(0);
      }
      await assertNoHorizontalOverflow(page);
      await assertNoWcagViolations(page);
      await screenshot(page, `${surface.name}-${viewport.name}-first-viewport`);

      await page.keyboard.press("Tab");
      await expect(page.locator(":focus")).toBeVisible();
      const focusStyle = await page.locator(":focus").evaluate((element) => {
        const style = getComputedStyle(element);
        return { outlineStyle: style.outlineStyle, outlineWidth: style.outlineWidth };
      });
      expect(focusStyle.outlineStyle).not.toBe("none");
      expect(Number.parseFloat(focusStyle.outlineWidth)).toBeGreaterThanOrEqual(2);

      expect(failures).toEqual([]);
      await context.close();
    });
  }
}

test("homepage creates one shared room and produces two copyable instructions", async ({ browser }) => {
  const context = await browser.newContext({
    permissions: ["clipboard-read", "clipboard-write"],
    viewport: { width: 1440, height: 1000 },
  });
  const page = await context.newPage();
  const failures = captureRuntimeFailures(page);

  await page.addInitScript(() => {
    window.EventSource = class {
      addEventListener() {}
      close() {}
    };
  });
  await page.route("https://api.octostore.io/elections", async (route) => {
    await route.fulfill({
      contentType: "application/json",
      status: 201,
      body: JSON.stringify({
        election_id: "roomQA123",
        campaign_path: "/elections/roomQA123/campaign",
        status_path: "/elections/roomQA123",
        watch_path: "/elections/roomQA123/watch",
      }),
    });
  });

  await page.goto("/", { waitUntil: "networkidle" });
  await page.getByRole("button", { name: "Create one shared room" }).first().click();

  const atlas = page.locator("#agent-atlas-instruction");
  const comet = page.locator("#agent-comet-instruction");
  await expect(atlas).toHaveValue(/roomQA123/);
  await expect(comet).toHaveValue(/roomQA123/);
  await expect(atlas).toHaveValue(/agent-atlas/);
  await expect(comet).toHaveValue(/agent-comet/);

  const copyAtlas = page.locator('[data-copy-target="#agent-atlas-instruction"]');
  await copyAtlas.click();
  await expect(copyAtlas).toHaveText("Copied");
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain("roomQA123");
  await screenshot(page, "home-desktop-shared-room");

  expect(failures).toEqual([]);
  await context.close();
});

test("agents page copy control works from the keyboard", async ({ browser }) => {
  const context = await browser.newContext({
    permissions: ["clipboard-read", "clipboard-write"],
    viewport: { width: 1440, height: 1000 },
  });
  const page = await context.newPage();
  const failures = captureRuntimeFailures(page);
  await page.goto("/agents/", { waitUntil: "networkidle" });

  const copySkill = page.locator('[data-copy-target="#agent-skill-install"]');
  await copySkill.focus();
  await page.keyboard.press("Enter");
  await expect(copySkill).toHaveText("Copied");
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain("git clone");
  const skillInstall = await page.evaluate(() => navigator.clipboard.readText());
  expect(skillInstall).toContain(
    "git clone --branch v0.14.4 --depth 1 https://github.com/octostore/octostore.io",
  );
  expect(skillInstall).toContain("npm ci --ignore-scripts --no-audit --no-fund");
  expect(skillInstall).toContain("./node_modules/.bin/skills add \\");
  expect(skillInstall).toContain("https://github.com/octostore/octostore.io/tree/v0.14.4");
  expect(skillInstall).toContain("--skill octostore --agent codex -y");
  expect(skillInstall).not.toMatch(/\bnpx\b/);

  expect(failures).toEqual([]);
  await context.close();
});

test("public protected-work examples keep the worker behind the supervisor", async ({ browser }) => {
  const context = await browser.newContext({
    permissions: ["clipboard-read", "clipboard-write"],
    viewport: { width: 1440, height: 1000 },
  });
  const page = await context.newPage();
  const failures = captureRuntimeFailures(page);

  await page.goto("/leader-election/", { waitUntil: "networkidle" });
  await page.locator('[data-copy-target="#leader-quickstart"]').click();
  const leaderQuickstart = await page.evaluate(() => navigator.clipboard.readText());
  expect(leaderQuickstart.match(/octostore-supervisor election/g)).toHaveLength(2);
  expect(leaderQuickstart.match(/^  octostore election hold\b/gm)).toHaveLength(2);
  expect(leaderQuickstart.match(/--acquire-timeout 60/g)).toHaveLength(2);
  expect(leaderQuickstart).toContain('"$ATLAS_WORKER"');
  expect(leaderQuickstart).toContain('"$COMET_WORKER"');
  expect(leaderQuickstart).not.toMatch(/^octostore election hold\b/m);

  await page.goto("/coordination/", { waitUntil: "networkidle" });
  await page.locator('[data-copy-target="#coordination-claim"]').click();
  const coordinationClaim = await page.evaluate(() => navigator.clipboard.readText());
  expect(coordinationClaim).toContain("AGENT_WORKER=./run-billing-agent");
  expect(coordinationClaim).toContain(
    'octostore-supervisor lock "invoice/1842" - "$AGENT_WORKER" -- \\',
  );
  expect(coordinationClaim).toContain('  octostore lock hold "invoice/1842" \\');
  expect(coordinationClaim).toContain("--ttl 120 --acquire-timeout 60 --json");
  expect(coordinationClaim).not.toMatch(/^octostore lock hold\b/m);
  expect(failures).toEqual([]);
  await page.close();

  await context.addInitScript(() => {
    localStorage.setItem("octostore_auth", JSON.stringify({
      token: "browser-test-secret",
      user_id: "550e8400-e29b-41d4-a716-446655440000",
      github_username: "octocat",
      namespace: "browser-test",
      api_base: "https://api.octostore.io",
    }));
  });
  const dashboardPage = await context.newPage();
  await dashboardPage.route("https://api.octostore.io/locks", async (route) => {
    await route.fulfill({
      contentType: "application/json",
      status: 200,
      body: JSON.stringify({ locks: [], total: 0, prefix: null }),
    });
  });
  await dashboardPage.route("https://github.com/octocat.png", async (route) => {
    await route.fulfill({
      contentType: "image/svg+xml",
      status: 200,
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"></svg>',
    });
  });

  await dashboardPage.goto("/dashboard.html", { waitUntil: "networkidle" });
  await expect(dashboardPage.locator("#main-content")).toBeVisible();
  const dashboardHold = await dashboardPage.locator("#example-status").innerText();
  expect(dashboardHold).toContain("AGENT_WORKER=./run-agent");
  expect(dashboardHold).toContain(
    'octostore-supervisor lock "repo/octostore/issue-1842" - "$AGENT_WORKER" -- \\',
  );
  expect(dashboardHold).toContain('  octostore lock hold "repo/octostore/issue-1842" \\');
  expect(dashboardHold).toContain("--ttl 120 --acquire-timeout 60 --json");
  expect(dashboardHold).not.toMatch(/^octostore lock hold\b/m);

  await context.close();
});

test("reduced-motion preference disables first-viewport animation", async ({ browser }) => {
  const context = await browser.newContext({
    reducedMotion: "reduce",
    viewport: { width: 1440, height: 1000 },
  });
  const page = await context.newPage();
  await page.goto("/", { waitUntil: "networkidle" });

  for (const selector of [".agent-hero-copy", ".coordination-stage"]) {
    await expect(page.locator(selector)).toHaveCSS("animation-name", "none");
  }
  await expect(page.locator("html")).toHaveCSS("scroll-behavior", "auto");
  await context.close();
});
