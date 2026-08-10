# Agent-first browser QA evidence

**Date:** 2026-08-04
**Candidate base:** `36db47f6cace55560d29c00d6d9677a55fe4bac8` plus the uncommitted agent-first candidate
**Browser tooling:** Playwright `1.62.1`, Chromium, axe-core `4.12.1`
**Scope:** `/` and `/agents/` at `1440x1000` and `390x844`

## Result

The scoped browser gate passes all seven tests:

- first-viewport outcome and coordination proof render at desktop and narrow widths;
- no page-level horizontal overflow;
- no axe violations for WCAG 2.0/2.1 A or AA tags;
- keyboard focus is visible;
- the agent-skill copy control works from the keyboard;
- the homepage creates one mocked shared room outside both candidates, gives the same room to Atlas and Comet, and copies the selected instruction;
- reduced-motion preferences disable first-viewport animation; and
- no console errors, page errors, or failed requests occurred in the tested flows.

The retained shared-room screenshot uses a deterministic mocked room response. It proves browser behavior, not hosted production availability.

## Defect found and repaired

`BQA-001` (serious accessibility): command blocks that could scroll horizontally were not reachable by keyboard in Safari. The first axe run failed on the affected `<pre>` regions. Adding `tabindex="0"` to those regions repaired the issue; the complete browser gate then passed.

A separate initial test failure used a text-content assertion against a textarea. The rendered value was correct; the harness now asserts the form-control value.

## Reproduction

```sh
npm ci --ignore-scripts --no-audit --no-fund
./node_modules/.bin/playwright install chromium
npm run test:site:browser:evidence
```

The normal release gate runs `npm run test:site:browser` without regenerating retained evidence.

## Boundaries

This evidence does not prove production content, hosted API availability, performance, Safari/Firefox rendering, or the release/deployment lanes. Those remain separate gates.
