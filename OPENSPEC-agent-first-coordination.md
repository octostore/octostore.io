# OpenSpec: agent-first coordination for OctoStore

**Status:** Approved execution contract under active implementation. This status is intentionally evidence-neutral; exact candidate, validation, review, merge, release, deployment, and production states must be reported outside the frozen candidate as separate digest-bound lanes.<br>
**Created:** 2026-08-02<br>
**Baseline refreshed:** 2026-08-04 against live remote `main` at `36db47f` in an isolated worktree<br>
**Source input:** [Darren Shepherd / David Aronchick thread](https://x.com/ibuildthecloud/status/2084075760512614839)<br>
**Working repository:** `/Users/daaronch/code/second-brain/projects/octostore.io`<br>
**Program type:** Product surface, HTTP API ergonomics, CLI, agent onboarding, and public messaging<br>
**Execution model:** Active phased implementation with separate local, CI, merge, release, deployment, and production-proof gates

## Executive decision

OctoStore should be presented and packaged as the smallest coordination primitive an agent fleet needs before it changes anything:

> **Stop two agents from doing the same work.**

The product does not become an agent orchestrator. It does not choose work, run prompts, delegate agents, merge branches, or provide a queue. It gives independent processes a shared, time-bounded answer to one of two questions:

1. **Who coordinates this group right now?** Use a leader election.
2. **Who owns this exact item right now?** Use a task lock.

The API remains the source of truth. A thin `octostore` CLI makes the lease lifecycle easy for agents to background and supervise. A concise, versioned `SKILL.md` becomes the primary agent onboarding surface. The public website leads with the two-agent outcome and reveals HTTP, curl, and implementation details progressively.

This is an agent-first acquisition and usability decision, not a claim that agents are the only users. Human operators, schedulers, controllers, and ordinary HTTP clients remain supported; they simply encounter the agent use case first.

## Goal card

**Goal ID:** `OCTO-AGENT-COORDINATION-01`<br>
**Objective:** Make OctoStore the fastest honest path for two agents to coordinate ownership before side effects.<br>
**Owner:** David Aronchick retains product acceptance; the active Codex goal owns implementation, release execution, and live-verification evidence<br>
**Dependencies:** Start implementation from current `origin/main`; preserve the existing server startup and public election contract; resolve the blocking decisions below through adversarial review.<br>
**Primary deliverables:** additive API contract, thin lease-supervisable CLI, versioned agent skill, two-agent demo, agent-first public surfaces, and evidence-gated release plan.<br>
**Done when:** the acceptance criteria in this document pass, the adversarial review has resolved the required decisions, and implementation/release/deployment/live verification are reported as separate evidenced states.<br>
**Not done when:** only copy, a mock demo, a local green test run, or a generated artifact exists.

### Evidence recording rule

This OpenSpec defines the contract and gates; it must not certify its own current bytes. Historical rejected candidates may be recorded before a replacement freeze, but passing gates and reviewer agreement belong in an external freeze packet, pull request, release record, and deployment evidence bound to the exact manifest digest or commit. Once a candidate is frozen for whole-candidate review, writing a new success claim into this file invalidates that candidate and requires new unchanged-byte gates and a new digest.

## Why this exists

The supplied thread identifies a real mismatch between the current product and the current path to adoption:

- The technical interface is good, but the site asks people to learn too much before showing what they can get done.
- A new visitor should first see how to get agents to coordinate, not a curl transcript or a tour of distributed-systems terminology.
- The strongest initial use case is independent agents working on overlapping repository work, where one agent should claim a task or become the merge coordinator before side effects begin.
- A CLI is useful for the lifecycle that is awkward in raw HTTP: acquire, heartbeat, observe loss, and exit cleanly.
- The opportunity is agent self-coordination through a few primitives and explicit guardrails, not a new declarative orchestration system.
- Watchable, bounded coordination state is valuable, but rebuilding ZooKeeper, a work queue, or a workflow engine would add the wrong surface area.

The product problem is therefore not “add more distributed-systems features.” It is “make the existing primitive legible, runnable, and supervisable by an agent in under a few minutes.”

## Verified starting point

This spec is grounded in the current checkout, not an assumed greenfield product.

| Area | Verified starting point | Consequence |
| --- | --- | --- |
| Repository | Local `main` is at `90eb7eb` / tag `v0.13.2`; local `main` is seven commits behind `origin/main` at `36db47f` | Start implementation from current `origin/main`; do not use this draft or the stale local checkout as release evidence |
| Server | One Rust binary, SQLite/WAL-backed, default server startup remains the compatibility path | Add CLI subcommands without breaking `octostore` as a server |
| Public API | Account-free `/elections` create, campaign, status, renew, and resign paths | Preserve the hosted no-credential entry point and its capability semantics |
| Authenticated API | `/locks`, lock ACLs on current `origin/main`, sessions, lock watches, webhooks, metrics, and admin paths exist; the hosted OAuth callback binds to the hosted dashboard | Make hosted and self-hosted task ownership easier without exposing anonymous locks or bypassing ACL behavior |
| CLI | The production binary currently handles `--version`; `clap` is used by internal test/benchmark binaries | This is a genuine new surface, not a documentation-only rename |
| Site | `/agents/`, `/leader-election/`, `/coordination/`, and `/docs/` already exist; agent pages still teach through HTTP examples | Recompose the hierarchy and retain deep technical pages as reference |
| API contract | `openapi.yaml` exists, but some schema descriptions and implementation behavior have drifted | Contract alignment is an explicit acceptance gate |
| User changes | The checkout has an unrelated uncommitted `.gitignore` change and this untracked OpenSpec | Preserve `.gitignore`; keep implementation work isolated from the planning artifact until the user chooses to adopt it |

## Product thesis

OctoStore is a **coordination referee**, not an agent manager.

An agent remains responsible for:

- discovering or receiving work;
- deciding whether it should attempt the work;
- receiving a shared election ID or deriving a stable lock key;
- executing prompts, tools, code, and side effects;
- storing branches, results, artifacts, and durable task state;
- putting human or policy approval around irreversible actions; and
- making downstream writes idempotent and fencing-aware where possible.

OctoStore is responsible only for:

- electing one live coordinator or owner;
- issuing a short-lived lease and monotonic term;
- renewing or expiring that lease;
- making loss observable;
- exposing bounded coordination metadata; and
- providing enough state for a client or operator to decide what to do next.

The public phrase “agent orchestration” may be used for discovery only when immediately narrowed to “agent coordination.” The product must never imply that OctoStore schedules or executes agents.

## Primary users and jobs

### Primary: agent builder or operator

> “I want several agents to work at once without two of them touching the same task, and I want a process I can supervise.”

Needs a short skill, stable coordination rules, a CLI with machine-readable outcomes, and an example that works with two independent processes.

### Agent runtime

> “Before I create a branch, call a tool, or make a side effect, tell me whether I own the work and whether my authority is still alive.”

Needs no prose interpretation after onboarding, explicit exit/status semantics, no secret leakage, and a safe response to uncertainty.

### Human API user

> “Give me one leader or one temporary owner over HTTP, without forcing an SDK.”

Needs the existing API, stable OpenAPI contracts, curl or any HTTP client, and a clear explanation of leases and fencing.

### Self-hosting operator

> “Keep the coordination boundary inside my network and run the same primitives.”

Needs the single-binary install, authentication, persistence, health checks, limits, and an honest single-authority topology.

## Goals

### G1. Make the agent path usable without a human tutorial

An agent that reads the canonical skill must be able to:

1. decide between election and task lock;
2. derive a stable, bounded key;
3. acquire before side effects;
4. renew at the server-recommended interval;
5. stop treating the work as owned after loss or uncertainty; and
6. report the fencing term and coordination outcome to its supervisor.

The first successful path must not require understanding SQLite, consensus, OAuth, webhooks, or the complete API reference.

### G2. Demonstrate two agents coordinating on a real-shaped task

The public demo must show two independent agent processes receiving the same coordination address or key from a human or external task source. Exactly one may enter the protected critical section at a time. The losing process must receive an actionable outcome rather than an ambiguous HTTP failure. When the current holder is terminated, another process must be able to become leader or owner after lease expiry.

The demo may use a simulated side effect, a repository worktree, or a release-checklist task. It must state which parts are simulated and must not pretend that OctoStore itself assigns tasks, stores branches, merges code, or validates the side effect.

### G3. Add a thin, agent-supervisable CLI

The CLI must make the common lease loop easy to run in the background and easy for an agent supervisor to observe. It must not become a second API with different semantics, and it must not imply that heartbeat loss can stop an uncoupled worker.

The default server behavior remains backward compatible:

```text
octostore                 # starts the server as today
octostore --version      # prints the server version as today
```

The coordination subcommands are defined below.

### G4. Improve the API where agents currently have to infer behavior

The API must preserve its useful endpoint-specific outcomes while adding retry guidance, stable error codes, request correlation, and a bounded watch path for election state. Existing routes remain supported; additive fields are preferred over a breaking rewrite.

### G5. Reorder the public surface around outcome, then mechanism

The root site, `/agents/`, README, and skill must answer these questions in order:

1. What can my agents do?
2. Which primitive should they use?
3. What is the smallest runnable path?
4. What happens when a process dies?
5. Where are the HTTP and implementation details?

Curl remains available, but it is a later detail rather than the first impression.

### G6. Establish measurable adoption and safety evidence

The work is successful only when a new agent can use the product and a reviewer can verify the lease semantics, failure behavior, API contract, and public claims from artifacts and tests.

## Non-goals and hard boundaries

The following are explicitly out of this goal:

- executing prompts, tools, shell commands, or agent runtimes;
- task discovery, scheduling, delegation, queue ownership, retry queues, or DAG/workflow syntax;
- branch storage, merge automation, artifact storage, or durable task result storage;
- a general metadata database or arbitrary key/value store;
- a ZooKeeper replacement, consensus cluster, multi-writer SQLite topology, or automatic high availability;
- hosted anonymous task locks that imply private ownership or tenant isolation;
- mandatory language SDKs or framework-specific agent integrations;
- a new policy language for human-in-the-loop approvals;
- exactly-once execution or correctness of downstream side effects;
- a large endpoint family for every possible agent framework; and
- a release, deployment, or production-availability claim before those lanes are independently verified.

## Design principles

1. **Outcome before mechanism.** Say “one agent owns this task” before saying “distributed lock.”
2. **Two primitives are enough for this goal.** Election answers “who coordinates?”; lock answers “who owns this item?”
3. **A lease is authority with an expiry, not a completion guarantee.** The docs and CLI must make this impossible to miss.
4. **Acquire before side effects.** The skill, demo, CLI help, and API examples all follow this order.
5. **Loss is a first-class result.** A client that cannot confirm renewal must stop claiming authority.
6. **The CLI is a thin lifecycle adapter.** It may manage a heartbeat and process signals; it must not invent scheduling semantics.
7. **Hosted simplicity has a boundary.** No-login public elections are convenient capability rooms, not private authentication.
8. **Progressive disclosure is structural.** Agent skill and CLI first; API reference, curl, persistence, and architecture later.
9. **Metadata is bounded and non-secret.** It explains ownership; it is not a place to put prompts, tokens, or task payloads.
10. **Every claim has evidence.** “One leader,” “automatic recovery,” “fencing,” “hosted,” and “self-hosted” each need a corresponding test or clearly stated limit.

### Surface budget

The approved first release may add:

- zero product nouns;
- one HTTP route: election watch;
- additive retry, renewal, correlation, and error-code fields on existing responses;
- eight CLI leaf commands under `election`, `lock`, and the operational `serve` path; and
- one canonical agent artifact at `/agents/SKILL.md`.

Any additional endpoint, top-level CLI noun, background daemon, SDK, metadata model, or new hosted auth mode exceeds this goal and requires a new disposition.

## Target coordination model

```text
agent reads skill
    -> chooses election (one coordinator) or lock (one task owner)
    -> receives one shared election ID or derives one stable lock key
    -> starts a supervised hold process before protected work
    -> acts only after acquired/leader
    -> hold process renews at the recommended interval
    -> supervisor stops new work on hold loss/uncertainty/exit
    -> another waiting agent may acquire and decide its next work
```

OctoStore does not create a common election room independently inside each agent; the room ID must be created once and shared. It also does not decide what the final arrow means. It supplies the state transition; the agent system supplies the task source, process coupling, and policy.

## API surface proposal

### Keep and clarify the existing nouns

| Noun | Question | Primary paths | Audience |
| --- | --- | --- | --- |
| Election | Who coordinates this group? | `POST /elections`, `POST /elections/:id/campaign`, `GET /elections/:id`, `POST /elections/:id/renew`, `POST /elections/:id/resign` | Hosted agents and any HTTP client |
| Lock | Who owns this exact item? | `POST /locks/:name/acquire`, `GET /locks/:name`, `POST /locks/:name/renew`, `POST /locks/:name/release` | Authenticated hosted and self-hosted agents |
| Session | Which ephemeral locks belong to this process? | Existing `/sessions` paths | Long-lived workers |
| Watch | What changed? | Existing `/locks/:name/watch`; new election watch below | Supervisors and agents |
| Operations | Is the service healthy and how is it configured? | Existing health/status/metrics/admin paths | Operators only |

Webhooks, OAuth, ACLs, metrics, and admin operations remain advanced or operator surfaces. They should not appear in the first agent quickstart.

### Additive API contract improvements

#### 1. Stable machine-readable outcomes

Do not introduce a universal response envelope. Election and lock responses already have useful endpoint-specific `status` values, and normalizing them would create more concepts than it removes.

Preserve and document the existing server outcomes:

| Surface | Stable server outcomes | Required additive guidance |
| --- | --- | --- |
| Election campaign/status | `leader`, `follower`, `vacant` | Keep `term`, `expires_at`, and `renew_after_ms` or `retry_after_ms` where meaningful |
| Lock acquire/status | `acquired`, `held`, `delayed`, `free` | Add `renew_after_ms` to `acquired`; add `retry_after_ms` to `held` and `delayed` |
| Renew/resign/release | Endpoint-specific success body | Return current expiry plus `renew_after_ms` on renew and a stable error code when the supplied capability is no longer current |

`lost` and `uncertain` are client lifecycle events, not server truth. A server can reject a stale lease with a stable error code, but only the CLI knows that a request timed out or that it missed its renewal deadline. The CLI maps transport results, server outcomes, and local deadlines into the lifecycle events defined below.

Existing `fencing_token` and election `term` names remain. The OpenAPI document must explain that each is a monotonic lease generation in its own primitive; clients must not compare values across different locks, elections, or authorities.

#### 2. Stable errors and retry guidance

Extend the existing JSON error body additively with:

```json
{
  "error": "Rate limit exceeded",
  "code": "rate_limited",
  "details": "Retry after the admission window",
  "retry_after_ms": 12000,
  "request_id": "opaque-request-id"
}
```

Initial stable code registry:

| Code | Meaning | Retryable |
| --- | --- | --- |
| `authentication_required` | No supported credential was supplied | No |
| `authentication_failed` | A supplied credential was invalid | No |
| `forbidden` | Credential or capability lacks authority for this operation | No without changed authority |
| `invalid_input` | Request body, identifier, UUID, or JSON is invalid | No without changed input |
| `invalid_ttl` | TTL violates the documented range | No without changed input |
| `invalid_lock_name` | Lock key violates the documented grammar or bounds | No without changed input |
| `not_found` | The requested resource does not exist | Endpoint-dependent |
| `session_expired` | The referenced session is no longer live | No; create a new session |
| `lease_not_current` | Lease ID or leader capability is stale, expired, or not current | No; treat authority as lost |
| `conflict` | Current state prevents the requested mutation | Only when response guidance says so |
| `capacity_exceeded` | The server cannot admit another resource in the configured bound | Yes, only with response guidance |
| `lock_limit_exceeded` | Principal reached its active-lock limit | After releasing or expiry |
| `rate_limited` | Admission budget is exhausted | Yes, using retry guidance |
| `upstream_unavailable` | A required upstream failed | Yes under caller policy |
| `internal_error` | Server failed without exposing internal details | Yes under caller policy |

The implementation may map multiple internal error variants to one public code, but it may not emit undocumented codes. Existing HTTP status codes remain unchanged in this additive release unless a separate compatibility decision approves a correction.

Requirements:

- `code` is stable and documented; prose is not a parsing contract.
- `retry_after_ms` is present only when retrying is meaningful.
- HTTP `Retry-After` remains present where applicable and agrees with the JSON guidance.
- The server generates a request ID for every request, returns it in `X-Request-Id`, and includes the same value in error bodies. A caller-supplied ID may be recorded separately only after validation; it must not replace the server ID.
- CORS exposes `X-Request-Id` and `Retry-After` to allowed browser origins.
- `details` contains only safe client-action context. Database, upstream, stack, path, and credential-bearing internal errors are logged against the request ID and are never reflected to the caller.
- Error bodies, logs, metrics labels, and demo output never contain leader tokens, bearer tokens, webhook secrets, or full authorization headers.
- A transient network failure is not converted into `acquired` or `renewed`.
- The stable code registry is finite and contract-tested. The first release must at least distinguish invalid input, missing/invalid authentication, forbidden namespace or ACL, not found, held/conflict, stale lease capability, capacity, rate limit, and internal failure.

#### 3. Election watch

Add a best-effort SSE path with the same conceptual contract as the existing lock watch:

```text
GET /elections/:id/watch
```

`POST /elections` adds a `watch_path` alongside its existing `campaign_path` and `status_path` so clients do not synthesize the route.

The stream sends an initial current-state event and later state hints using one event schema:

```text
event: state
retry: 1000
data: {"schema_version":1,"election_id":"7mK...","status":"leader","leader":{"candidate_id":"agent-a","term":12,"expires_at":"..."},"retry_after_ms":14000,"observed_at":"..."}
```

`status` is `leader` or `vacant`; `leader` is the existing public leader object or `null`; `retry_after_ms` is always nonnegative; and `observed_at` is generated by the server. No event includes the leader token. Renewal may emit another `state` event with the same term and a later expiry. Expiry is represented as current `vacant` state rather than a durable historical `expired` event.

The handler admits and subscribes the connection before reading and emitting the initial state. Any queued notification is treated only as a trigger to re-read current authoritative state, so a queued stale event cannot regress the stream after the snapshot. Duplicate current-state events are allowed. Lag, dropped notifications, serialization failure, or internal stream failure closes the connection; the server never fabricates an `error` election state.

It is not a durable event log. `Last-Event-ID` replay is unsupported and SSE `id` is omitted. A reconnecting client must re-read `GET /elections/:id` before deciding what to do. Events are hints: clients reconcile after connect, reconnect, lag, or any ambiguity, and they still must campaign successfully before acting as leader.

Proposed hosted defaults for adversarial review are eight concurrent election streams per admission client, 1,024 globally, a 15-second keepalive, a 15-minute maximum connection lifetime, and a bounded 1–30 second reconnect hint. Limits are configurable, use the same trusted-proxy client identity rules as public election admission, and reject excess connections with `429`, `rate_limited`, and matching retry guidance. The load test must validate or revise these numbers before approval; they are not a hosted SLA.

This is intentionally a watch, not a directory database. A future directory/listing surface is out of scope unless a separate goal proves a concrete need.

#### 4. No new metadata model in this goal

Keep the existing bounded metadata string and document it as non-secret diagnostic context. The demo must work without structured labels. A `labels` object, arbitrary coordination records, directory listing, or task payload field requires a separate use case and goal.

#### 5. Contract alignment

Before implementation is called complete:

- every public route in `src/` is represented in `openapi.yaml`;
- request defaults, required fields, limits, response status codes, and error shapes match implementation;
- lock watch and election watch are documented with their actual best-effort semantics;
- generated or served OpenAPI contains the release version correctly; and
- an integration test validates representative examples against the contract.

### API invariants that must not change

- At most one live leader exists for an election and at most one live owner exists for a lock within one authority.
- A lease expires if the holder stops renewing.
- Terms/fencing generations remain monotonic across release and restart within the supported single-authority topology.
- Possession of a leader token authorizes only the corresponding election term; it is not an account or a global API key.
- A generated public room ID is difficult to guess but is not a privacy boundary.
- Public admission is rate- and capacity-limited.
- A lock or election is advisory coordination; downstream systems must enforce any stronger correctness boundary.
- Existing hosted election consumers do not need a CLI, SDK, account, or API key.

## CLI proposal

### Command shape

The CLI uses the two existing product nouns and only the verbs needed for the agent path. It is shipped as the existing `octostore` binary.

```text
octostore election create [--server URL] [--json]
octostore election hold <election-id> --candidate ID [--ttl SECONDS] [--acquire-timeout DURATION] [--allow-insecure-http] [--json]
octostore election status <election-id> [--json]
octostore election watch <election-id> [--json]

octostore lock hold <name> [--ttl SECONDS] [--acquire-timeout DURATION] [--json]
octostore lock status <name> [--json]
octostore lock watch <name> [--json]

octostore serve [server configuration flags]
```

First-release semantics:

- `hold` waits for ownership, keeps the capability in process memory, owns the heartbeat lifecycle, and releases or resigns on clean shutdown;
- `watch` is read-only and does not acquire anything; it performs an initial status read and reconciles current state after every SSE signal or reconnect;
- `status` is read-only and does not acquire anything;
- `serve` starts the server; no coordination command starts a hidden server.

One-shot campaign/acquire/renew/release/resign remain available through HTTP and are not duplicated in the first CLI release. Adding them later requires a demonstrated workflow, a secret-output contract, and a separate compatibility review. Do not collapse election and lock semantics into one ambiguous `agent` command merely to shorten help output.

### Configuration boundary

- `--server` overrides `OCTOSTORE_URL`.
- Election commands default to `https://api.octostore.io`; the selected server is present in startup output so an agent cannot silently coordinate against the wrong authority.
- Lock commands require an explicit server URL and bearer credential source. They do not silently fall back to the public hosted election service or a local process.
- The documented credential path is `OCTOSTORE_TOKEN_FILE` with owner-only permissions. `OCTOSTORE_TOKEN` is supported only as an explicit convenience with a same-user environment-inspection warning; no `--token` argument exists.
- Sending a credential or leader/lease capability to cleartext HTTP is refused unless the server is loopback or the caller supplies an explicit insecure-development override. Capability-bearing loopback clients bypass inherited proxy settings.
- Connection, request, acquisition, and shutdown timeouts are separate settings with documented defaults. A short request timeout must not be mistaken for the lease-expiry deadline.

### Primary agent path

The skill teaches long-running coordination through `hold`, because it keeps the secret capability or lease identifier in process memory and makes lifecycle events visible:

```bash
octostore-supervisor election "$OCTOSTORE_ELECTION" "$AGENT_ID" "$AGENT_WORKER" -- \
  octostore election hold "$OCTOSTORE_ELECTION" \
    --candidate "$AGENT_ID" --ttl 30 \
    --acquire-timeout 60 --json
```

For an authenticated task owner:

```bash
OCTOSTORE_URL=http://localhost:3000 \
OCTOSTORE_TOKEN_FILE=/run/secrets/octostore-token \
octostore-supervisor lock "repo/octostore/issue-1842" - "$AGENT_WORKER" -- \
  octostore lock hold "repo/octostore/issue-1842" \
    --ttl 120 --acquire-timeout 60 --json
```

The CLI does not execute the agent's work. The separately installed supervisor wraps the hold and worker, permits the critical section only after `leader` or `acquired`, and stops or fences the worker when the hold process emits `lost`/`uncertain` or exits unexpectedly.

### Required supervisor coupling

A separate heartbeat process can observe authority but cannot force an arbitrary worker to obey it. The public promise is therefore valid only with an explicit coupling contract:

1. The supervisor starts `hold` before protected work.
2. The worker does not begin until the supervisor receives `leader` or `acquired` for the expected key and candidate.
3. The supervisor continuously monitors both the JSON event stream and process exit.
4. On `lost`, `uncertain`, malformed output, or unexpected exit, the supervisor stops issuing new side effects, cancels the worker where possible, and reports that downstream effects may need reconciliation.
5. The worker passes the term/fencing generation to downstream systems that can reject stale writes.

OctoStore cannot revoke an already-issued external side effect or guarantee that a paused worker stops. A future child-process wrapper such as `--exec` could tighten process coupling, but it is out of this goal because it would make the CLI a process supervisor and materially expand its responsibility.

### Hold lifecycle

`hold` must:

1. validate all configuration before campaigning;
2. enter a pre-acquisition state that retries `follower`, `held`, `delayed`, rate-limited, and transient unavailable outcomes using server guidance plus bounded jitter;
3. wait indefinitely by default because `hold` is explicitly a supervised background lifecycle; when `--acquire-timeout` is set, stop at that monotonic deadline and exit not-owned;
4. emit `leader` or `acquired` exactly once when authority begins;
5. renew before a locally computed safety deadline, using server guidance when supplied and never scheduling at or after expiry;
6. attempt clean release/resignation on `SIGINT` and `SIGTERM` with a short bounded timeout;
7. emit `lost` if the server rejects renewal or reports a different current lease;
8. emit `uncertain` and exit nonzero if renewal cannot be confirmed before the safety deadline; and
9. never transition from post-acquisition `lost` or `uncertain` back to waiting or acquired in the same process.

Clock calculations use a monotonic local clock for deadlines and treat server timestamps as diagnostics. Retry and renewal jitter must remain bounded so the latest possible renewal attempt still precedes the safety deadline.

Proposed timing contract for review: record the monotonic request-start time; schedule renewal no later than the earlier of server guidance or 50% of the granted TTL; apply only one-sided earlier jitter of up to 10%; and declare uncertainty if renewal is not confirmed by 80% of the granted TTL measured from request start. Connect/request timeouts are capped by that safety deadline, and clean shutdown gets at most two seconds or the remaining safety window, whichever is shorter. These percentages must be covered by deterministic-clock tests and may change only through the blocking CLI contract review.

### Machine output

`--json` emits newline-delimited JSON events. The initial schema is intentionally small:

```json
{"schema_version":1,"sequence":1,"event":"waiting","kind":"lock","name":"repo/octostore/issue-1842","retry_after_ms":1200,"observed_at":"..."}
{"schema_version":1,"sequence":2,"event":"acquired","kind":"lock","name":"repo/octostore/issue-1842","term":42,"expires_at":"...","authority_remaining_ms":23984,"authority_observed_unix_ms":1785856000123,"authority_observed_continuous_ms":99500123,"observed_at":"..."}
{"schema_version":1,"sequence":3,"event":"renewed","kind":"lock","name":"repo/octostore/issue-1842","term":42,"expires_at":"...","authority_remaining_ms":23991,"authority_observed_unix_ms":1785856012123,"authority_observed_continuous_ms":99512123,"observed_at":"..."}
{"schema_version":1,"sequence":4,"event":"lost","kind":"lock","name":"repo/octostore/issue-1842","term":42,"reason_code":"lease_not_current","observed_at":"..."}
```

Normative event names are `waiting`, `leader`, `acquired`, `renewed`, `released`, `lost`, `uncertain`, and `error`. `leader`, `acquired`, and `renewed` include a positive `authority_remaining_ms`: a non-secret relative budget calculated from the CLI's monotonic safety deadline immediately before emission. Those authority events also include `authority_observed_unix_ms`, the non-secret wall-clock millisecond instant paired with that emitted budget, and `authority_observed_continuous_ms`, the non-secret same-host monotonic millisecond instant. A separate supervisor must launch and read the hold process on the same host and boot, subtract the greater of wall-clock and same-host monotonic queue age, reject future, expired, or cross-host/persisted events, and never restart the full relative budget at receipt. Every JSON line uses the documented schema; human diagnostics go to stderr and never corrupt stdout JSONL. Unknown additive fields must be ignored, while an unknown `schema_version` must fail closed.

Leader tokens and lease IDs are never printed by `hold`. The CLI must never place credentials in URLs, command arguments, metadata examples, logs, or telemetry. Supported bearer-token input is a permissions-restricted file, stdin where unambiguous, or an environment variable with an explicit same-user process-inspection warning; token-file input is the documented default.

### Exit codes

Exit codes are part of the CLI contract and must be documented and tested:

| Code | Meaning | Agent action |
| ---: | --- | --- |
| `0` | Acquired/leader and cleanly released, or read-only command succeeded | Enter the protected section only after the acquired event |
| `11` | Timed out before acquisition after retryable outcomes | Do not perform the work; retry only under supervisor policy |
| `20` | Lease lost, expired, rejected, or explicitly uncertain | Stop side effects and report loss |
| `64` | Invalid command, missing configuration, or invalid input | Fix configuration; do not retry blindly |
| `70` | Unexpected server or client failure | Treat as not-owned; inspect logs/status |

Signal termination before acquisition exits with the platform-conventional signal status. After acquisition, a signal-triggered clean release emits `released`; the final status remains documented and consistent across supported platforms. Exact signal status is a blocking review decision because scripts may rely on it.

### CLI safety and operability

- `--help` explains election versus lock in outcome language.
- Human output is concise; machine output is stable and line-oriented.
- No secret is written to logs, telemetry, shell history by default, or error messages.
- The process exits when it cannot prove authority; it does not silently extend a lease after a long network partition.
- Signals, timeout, retry, and clock-skew assumptions are tested.
- A backgrounded process can be supervised using only its exit status and JSON events; a same-host, same-boot supervisor subtracts the greater queue age derived from `authority_observed_unix_ms` and `authority_observed_continuous_ms` before starting a relative watchdog from `authority_remaining_ms`, caps silence at 20 seconds because lock sessions confirm on a 24-second safety bound, allows overrides only to shorten that cap, and treats every release/loss/error terminally. The tested reference requires `jq` and Perl with `Time::HiRes`; relayed or persisted events fail closed.
- Starting `hold` does not fork an unmanaged daemon. If daemonization is ever added, it requires a separate design and lifecycle contract.

## Canonical agent skill

Create a versioned, plain-text file at:

```text
https://octostore.io/agents/SKILL.md
```

The human page at `/agents/` links to it as the primary “Give this to your agent” action. The raw file must be concise enough to be read as agent context and complete enough to prevent unsafe inference.

The target is at most 1,200 words. Primitive selection, the first command, and the stop-on-loss rule must appear in the first 60 lines. Deep API, architecture, and operational detail is linked rather than repeated.

### Required skill content

The skill must include:

1. **When to use OctoStore:** only when multiple processes may act on the same coordinator or item.
2. **Primitive selection:** election for one coordinator; lock for one exact item.
3. **Key rules:** derive a stable, bounded key from durable work identity; never use a random key per retry; never put secrets in metadata.
4. **Shared bootstrap:** create a hosted election room once and pass the same room ID to every candidate; never let each agent create its own room for the same election.
5. **Safety loop:** acquire before side effects; renew while healthy; stop on loss or uncertainty; use the term/fencing token downstream where supported.
6. **Supervisor coupling:** explain that a background heartbeat cannot stop an uncoupled worker and require the host to gate and cancel work from the hold event stream.
7. **CLI path:** the tested `hold` examples and exit/event semantics.
8. **Human boundary:** ask for approval before irreversible or high-impact actions; OctoStore does not grant permission to perform those actions.
9. **System boundary:** OctoStore does not assign work, execute tools, merge branches, or prove downstream correctness.
10. **Hosted boundary:** public election room IDs are capabilities for coordination, not private authentication; hosted locks require OAuth authentication, and self-hosting keeps authenticated task traffic and identity on the operator's network.
11. **Failure behavior:** treat missing, stale, or ambiguous responses as not-owned.
12. **Install integrity:** ask before installing software; use a pinned release and verified checksum path; never silently pipe an unreviewed network response into a shell.
13. **Versioning:** a skill version, compatible CLI/API range, and command for reporting what the agent read.

The file must contain no hidden instructions, arbitrary downloads, credential collection, or commands that mutate a user's repository without the user's own task context. It is public documentation and must be reviewed as an untrusted input by agent hosts.

## Public messaging and information architecture

### Message hierarchy

Primary headline:

> **Stop two agents from doing the same work.**

Supporting promise:

> One shared lease tells one agent “go” and everyone else “wait” before side effects.

Short boundary:

> OctoStore does not run agents or undo writes. Your agent host must stop or fence work when the lease is lost.

Primary actions:

1. **Create one shared room** — one click or one CLI command, no account, producing a room ID and copyable agent instruction.
2. **Give two agents the skill** — raw `SKILL.md` plus the same room ID and task objective for both agents.
3. **Watch one lead and one wait** — a visible, bounded demo with simulated or explicitly labeled side effects.
4. **Use the API** — full HTTP and OpenAPI details.

Secondary message for non-agent users:

> Pick one leader. Everyone else waits.

This retains the strong current election message without letting it obscure the agent outcome.

### Page roles

| Surface | First job | What belongs above the fold | What moves later |
| --- | --- | --- | --- |
| `/` | Explain the outcome in seconds | Agent collision, two-agent demo, skill CTA, election/lock choice | Curl, architecture, full feature inventory |
| `/agents/` | Onboard an agent builder and an agent | Skill link, primitive choice, runnable CLI path, live state transition | Raw HTTP and implementation details |
| `/leader-election/` | Teach the hosted primitive | Two-call explanation, lease/loss behavior, no-login boundary | Complete curl and OpenAPI reference |
| `/coordination/` | Teach authenticated task ownership | Hosted sign-in, claim/hold loop, self-host option, fencing | Sessions, webhooks, ACLs, admin |
| `/docs/` | Retrieve exact details | Navigation by task and complete endpoint tables | Long rationale and internals |
| `README.md` | Convert a technical visitor | One-sentence value, skill link, CLI quickstart, API link | Full endpoint inventory and configuration |
| `/agents/SKILL.md` | Supply agent context | Dense, normative, versioned instructions | Marketing prose and visual demo |

### First-viewport proof

The first viewport must contain one live coordination wire, not a feature grid:

```text
MERGE COORDINATOR · room 7mK…

agent-a  ── leader · term 12 · renews in 15s
agent-b  ── waiting for agent-a · retries in 15s

Tell both agents:
Read https://octostore.io/agents/SKILL.md and coordinate the merge-coordinator
role in election 7mK… using a unique candidate ID. Do not merge until leader.
```

The room is created once and reused by both agents. The interface must make copying two instructions easier than reading an API tutorial. It may reveal the underlying CLI command immediately below the proof, while curl remains in the deeper API path.

In production, room creation and leader/follower state must come from the live hosted election API; the page may simulate the repository side effect but may not preselect or animate a fake winner. API failure produces an explicit unavailable state with a request ID when present, never a successful-looking animation.

### Copy rules

Use:

- “coordinate agent work”;
- “one temporary owner”;
- “lease expires after heartbeats stop”;
- “stop when authority is lost or uncertain”;
- “hosted election without login; authenticated task locks hosted or self-hosted”;
- “small HTTP primitive.”

Avoid or qualify:

- “safe agents” without naming the fencing/downstream boundary;
- “exactly once”;
- “automatic orchestration”;
- “private” for an anonymous room ID;
- “consensus” or “high availability” unless the topology actually provides it;
- “prevents all duplicate work”;
- “your agents will collaborate” when OctoStore only provides ownership state; and
- a wall of curl before the user sees a result.

The primary headline is an outcome statement, not an exactly-once guarantee. The short boundary above must appear in the same viewport, not in a buried safety page.

### Two-agent demo acceptance shape

The canonical demo is the product's original real use case:

> Two agents work in separate repository subtrees. Both receive one shared election ID for the merge-coordinator role. One becomes coordinator and may enter the simulated merge gate. The other waits. When the leader exits, the waiting candidate may become coordinator after resignation or lease expiry.

An authenticated lock variant may then show two agents receiving the same durable work-item key, with only one exact task owner. It is secondary because it requires an owner-only bearer credential and an OAuth-authenticated hosted or authenticated self-hosted authority, unlike the no-login hosted election path.

The demo must visibly show:

- the shared election ID or stable lock key;
- how that shared room ID or key reached both agents;
- the winning process;
- the losing process and its next action;
- the lease expiry/renewal loop;
- the fencing term or equivalent generation; and
- the supervisor stopping or fencing protected work when the holder loses authority; and
- the explicit boundary that branches, work queues, tools, and merges live elsewhere.

## Phased implementation and goal gates

### Phase 0 — baseline and decision lock

Record the current checked-out commit, `origin/main` relationship, route inventory, API/OpenAPI mismatches, CLI help behavior, and existing site first-viewport copy. Reconcile the working tree before implementation.

**Exit evidence:** a checked-in baseline note or issue comment with commands, commit SHAs, and explicit local/remote/deployed boundaries.

### Phase 1 — API and state-machine contract

Implement the additive retry fields, stable error registry, request correlation, election watch contract, and OpenAPI alignment. Preserve existing endpoint-specific election and lock outcomes and current clients. Do not add a universal response envelope, queue, or orchestration concept.

**Exit evidence:** unit/integration tests for leader/follower, acquire/held, renew, expiry, fencing monotonicity, watch reconnect and initial lock snapshots, rate limiting, webhook event validation and bounded fan-out admission, identity-source migration and OAuth-proven administration, redaction, and representative OpenAPI examples.

### Phase 2 — CLI MVP

Implement the reviewed `create`, `hold`, `status`, `watch`, and `serve` command set. Keep one-shot lifecycle mutations in HTTP and keep bare server startup backward compatible.

**Exit evidence:** two-process integration run, pre-acquisition retry/timeout test, signal handling test, forced expiry/loss test, post-loss no-reacquire test, supervisor-cancels-worker test, stable exit/event schema test, secret-redaction test, and install/version smoke test for every published platform binary.

### Phase 3 — agent skill and two-agent experience

Publish the versioned `SKILL.md`, build the two-agent demo, and ensure the skill points only at commands and API behavior covered by tests. The demo must be runnable without a private account for the hosted election path and must clearly identify the owner-only bearer requirement for task locks on an OAuth-authenticated hosted or authenticated self-hosted authority.

**Exit evidence:** raw skill retrieval, word/first-60-line budget check, skill version evidence, demo transcript, shared-room bootstrap test, and an independent review confirming that no side effect occurs before acquisition or continues intentionally after observed loss.

### Phase 4 — public surface rewrite

Recompose `/`, `/agents/`, README, and docs navigation around the message hierarchy. Move curl and API detail later without removing the reference surfaces. Keep the existing visual language unless a separate design decision changes it.

**Exit evidence:** responsive/accessibility/content checks, route/link checks, rendered first-viewport review, copy claim audit, static and rendered proof that every protected-work quickstart couples a worker to `octostore-supervisor` with `--acquire-timeout 60`, and live host verification of the exact deployed content.

### Phase 5 — release and measurement

After merge, run the repository's full documented checks against the exact tagged SHA, stage a draft release, publish and verify every native asset, publish the crate, finalize the release, deploy that immutable SHA, and then run live verification. Publication is recoverable but not atomic: crates.io publication is irreversible, so a same-tag rerun must detect and skip an already-published crate and resume release finalization/deployment. Instrument the acquisition funnel without logging coordination secrets.

**Exit evidence:** clean final tree, local CI, merged commit, exact stable release tag, locked release-tool execution, deployment evidence, live API/OpenAPI version, live skill, live pages with safe served-file modes, an installer-produced exact tagged CLI/supervisor pair, a concurrent supervised two-agent production canary inside the rollback window, and a separate statement of any unverified hardware/availability/scale lane.

## Goaling handoff

This section is the launch contract for the active execution goal. The user-authorized goal is recorded as `OCTO-AGENT-COORDINATION-01`; it does not broaden the product scope or waive any evidence gate.

### Entry gate disposition

Implementation proceeds only because the entry gate is now resolved as follows:

1. every row in “Decisions required before implementation” has the accepted disposition recorded below;
2. this branch-local document is approved for the active goal;
3. implementation is isolated at `/Users/daaronch/code/second-brain/.codex-work/2026-08-02/octostore-agent-first` from `origin/main` at `36db47f`;
4. stale PR #40 is inventoried and must be reconciled before merge without restoring its obsolete homepage; and
5. the active goal owns release and live-verification execution, with David Aronchick retaining final product acceptance.

### Goal payload

**Objective:** Implement the approved agent-first OctoStore experience so two independently supervised agents can share one election or lock, exactly one can hold current authority in the supported topology, authority loss is machine-observable, and a new visitor can reach that result through the skill and CLI before reading raw HTTP details.

**Required work packages, in dependency order:**

| Package | Output | Depends on |
| --- | --- | --- |
| `W0 Baseline` | Current SHA/route/contract/site inventory and reconciled PR state | Approved decisions |
| `W1 API contract` | Retry fields, error registry, request IDs, bounded election watch, corrected OpenAPI/tests | `W0` |
| `W2 CLI lifecycle` | Minimal command tree, hold state machine, JSONL/exit contract, secret-safe config, supervisor harness | `W1` |
| `W3 Skill and demo` | Versioned raw skill, shared-room two-agent merge-coordinator demo, install integrity path | `W2` |
| `W4 Public surface` | Root, `/agents/`, README, docs hierarchy, claim audit, responsive/accessibility proof | `W3` |
| `W5 Release and live proof` | Local gates, PR/CI, merge, tag/release, deployment, exact-version live API/site/skill/canary evidence | `W1`–`W4` |

**Hard constraints:** preserve the two nouns, bare server startup, existing HTTP clients, current auth/ACL boundaries, single-authority topology, and user-owned dirty files. Do not add queues, DAGs, task assignment, prompt execution, arbitrary metadata, anonymous hosted locks, an SDK program, or a child-process executor.

**Terminal condition:** all acceptance criteria are evidenced, all repository-owned checks are green locally and in CI, no started process remains running, the change is merged and released, and the exact deployed version passes live API, skill, website, install, and two-agent canary verification. Report implementation, local validation, CI, merge, release, deployment, and live proof as separate lanes.

**Re-review triggers:** pause the goal and return to adversarial review if implementation would change a public noun/verb, break an existing response, alter auth or capability semantics, add a durable event/history model, run child processes, expose anonymous task locks, or weaken the stop-on-uncertainty rule.

## Acceptance criteria

The goal is not complete until all applicable criteria pass and are evidenced.

### Agent usability

- A fresh agent can read the skill and select election versus lock without reading the architecture document.
- The agent can reach a first successful coordination result in under five minutes when the documented prerequisites are available.
- The README installs and verifies the pinned CLI before its first `octostore` invocation; a clean-machine reader never reaches a command-not-found dead end.
- The first example does not require a login or API key for the hosted election path.
- Both candidates receive the same externally created election room; the quickstart never creates one room per candidate.
- The skill tells the agent what to do on held, delayed, lost, expired, rate-limited, and uncertain outcomes.

### API correctness

- Existing public election paths continue to work for current request/response shapes.
- Stable error codes and retry guidance are tested and documented.
- Election watch begins with a current snapshot, remains bounded, and documents dropped-event/reconnect behavior.
- Election watch has tested per-client/global admission limits, idle cleanup, and a race-free subscribe/snapshot sequence.
- Lock watch emits a capability-free current snapshot as its first SSE frame, documents that behavior exactly, models `text/event-stream` as a string wire frame containing a `LockWatchEvent`, closes on lag, and requires GET reconciliation.
- Webhook creation accepts exactly `acquired`, `released`, `renewed`, `expired`, and `*`; invalid event names never persist. Best-effort lock-event fan-out has a shared hard concurrency bound acquired before task creation, drops saturated events without an unbounded queue or per-drop log amplification, and recovers capacity after deliveries finish.
- Durable users carry an explicit `legacy`, `local`, `static`, or `oauth` identity source. Only identities valid for the configured auth mode enter the bearer cache; mode switches do not reactivate stale credentials. `ADMIN_USERNAME` requires OAuth configuration and authorizes only a matching durable OAuth identity. Same-name bootstrap promotion is limited to a proven local/static ID and rotates the old token; conflicting OAuth GitHub IDs fail closed.
- Terms/fencing generations remain monotonic across release and restart tests.
- No server-generated secret appears in response metadata, logs, errors, SSE payloads, site examples, or CLI default output. Caller-supplied metadata is explicitly non-secret and remains the caller's responsibility.
- Public room IDs are described as hard-to-guess coordination addresses, not access controls.
- `lost` and `uncertain` remain CLI lifecycle events; the HTTP API never claims knowledge of a client-side transport timeout.

### CLI correctness

- `octostore` still starts the server when invoked without a coordination subcommand.
- `--help`, `--version`, exit codes, JSONL events, retry behavior, and signal behavior have automated tests.
- `hold` never continues to claim authority after loss or uncertainty.
- Pre-acquisition waiting backs off using server guidance and jitter, honors the acquisition timeout, and does not busy-loop.
- A `hold` process never reacquires after it has emitted `leader` or `acquired` and then loses authority.
- A backgrounded `hold` process can be supervised without parsing prose.
- The reference supervisor runs the hold and worker on one host, gates worker start on acquisition, subtracts the greater wall-clock or same-host monotonic queue age from every emitted authority budget, and cancels or fences both protected process groups on release, loss, uncertainty, malformed output, unexpected hold-process exit, or supervisor death. A buffered stale event or civil-clock rollback cannot extend authority, a dead supervisor cannot disable an already armed watchdog's independent containment, and terminal events cannot restart work.
- CLI output and process arguments do not expose bearer credentials by default.

### Public surface

- `/` and `/agents/` show the outcome, skill CTA, and two-agent path before curl/API depth.
- `/agents/SKILL.md` is a stable, versioned, directly retrievable artifact.
- The first viewport creates or displays one shared room and produces a copyable instruction for two agents before presenting curl.
- Every copyable example that protects real work defines an explicit worker, wraps it with `octostore-supervisor`, nests the corresponding `hold` command, and retains `--acquire-timeout 60`; bare holds appear only as protocol/reference detail.
- Every CTA resolves to a tested route; no page describes an unimplemented CLI command.
- The message says what OctoStore does not do and distinguishes no-login hosted elections from authenticated locks on hosted or self-hosted authorities.
- The API reference remains available to human users and is linked from the progressive-disclosure path.

### Release truth

- Local validation, CI, merge, release, deployment, live content, live API, and production canary are reported as separate states.
- No generated demo transcript or local test is presented as production proof.
- Any remaining scale, HA, downstream-fencing, or physical-environment boundary is called out explicitly.
- Release entry is an explicit `stable-release` repository dispatch whose workflow is loaded from `refs/heads/main`; its untrusted `client_payload.tag` and every pre-deploy validation accept only `^v[0-9]+\.[0-9]+\.[0-9]+$`, and the tag must resolve exactly to the trusted main SHA before any secret is referenced. After the immutable tag is pushed, the operator invokes `gh api --method POST repos/octostore/octostore.io/dispatches -f event_type=stable-release -F 'client_payload[tag]=v0.14.0'`. Prerelease, preview, nightly, malformed, build-suffixed, retargeted, off-main, or non-head refs fail before publication or production SSH.
- Every release-owned Node executable, including skill installation, resolves through `npm ci` and the committed lockfile; release scripts contain no ephemeral package runner.
- Deployment scopes private state-file permissions without changing the checkout umask, normalizes and verifies served directories as `0755` and files as `0644` on deploy and rollback, and keeps rollback armed through the supervised canary.
- Production acceptance installs the exact tag's CLI and supervisor with the exact verified public installer, then runs two concurrent supervised workers against the live API and proves leader/waiter behavior, cancellation, takeover, both worker start/stop records, secret-free evidence, and no remaining tracked processes.

## Success measures

Measure these before and after the change; do not fabricate a baseline.

| Measure | Initial target | Evidence |
| --- | --- | --- |
| Skill-to-first-result | A first-time agent reaches a valid election result in ≤ 5 minutes in a clean documented environment | Reproducible transcript with versions and prerequisites |
| Two-agent collision demo | Two concurrent candidates never both enter the protected section under the supported single-authority test | Integration test and demo transcript |
| Shared bootstrap correctness | 100% of quickstart/demo runs give all candidates the same election ID or lock key | Harness assertion and UX review |
| Lease-loss response | A killed or expired holder stops emitting ownership and exits within the documented detection bound | Timed failure test; report local and hosted separately |
| Supervisor coupling | Reference worker begins only after acquisition and is canceled or fenced after observed loss | End-to-end process test with event/exit evidence |
| API contract drift | Zero undocumented route/schema mismatches in the reviewed scope | OpenAPI validation and integration checks |
| CLI secret leakage | Zero tokens in default stdout, stderr, argv, structured event fields, and repository examples | Automated redaction scan plus manual review |
| Public comprehension | Test participants can answer “what can my agents do?” and “which primitive do I use?” after the first viewport | Small qualitative review, not a vanity pageview claim |
| Adoption funnel | Record skill fetch, demo start, first successful election, and self-host install separately | Privacy-safe metrics with no room IDs, tokens, or metadata |

Performance targets should be measured rather than invented. At minimum, record local p50/p95 for acquire, renew, and first watch event under a stated concurrency and database configuration; do not present those numbers as a hosted SLA until the hosted lane is independently measured.

## Risks and mitigations

| Risk | Failure mode | Mitigation |
| --- | --- | --- |
| Scope creep | “Agent orchestration” turns into a queue, planner, or workflow product | Keep the non-goals and two-primitive model in the skill, docs, and review checklist |
| Public abuse | Anonymous election rooms are created or campaigned at high volume | Preserve per-client limits/capacity, bounded IDs/metadata, monitoring, and explicit hosted limits |
| False safety | Users treat a lease as exactly-once side-effect protection | Teach fencing, idempotency, expiry, and uncertainty; demo a downstream boundary |
| Split coordination domain | Each agent creates its own election room, so both become leader | Create the room once outside candidates; pass the same ID to both; assert this in the skill and demo harness |
| Lost response | A successful campaign response is lost and the client cannot recover its leader token | CLI treats the attempt as uncertain/not-owned; add reattachment only as a separately reviewed capability, never by trusting candidate ID alone |
| Network partition | A client keeps acting after it can no longer renew | Deadline-aware heartbeat, `uncertain` outcome, process exit, and supervisor-visible event |
| Uncoupled heartbeat | The hold process exits but the separate worker continues side effects | Make supervisor coupling normative, test worker cancellation, and avoid claiming OctoStore itself stops arbitrary processes |
| Watch exhaustion | Anonymous clients hold many SSE connections or reconnect aggressively | Per-client/global stream limits, keepalive and idle cleanup, bounded retry hints, and hosted monitoring |
| Webhook amplification | A credentialed caller renews rapidly while callbacks stall, creating unbounded detached delivery work | Validate a finite event set and acquire a shared bounded fan-out permit before task creation; drop best-effort events when saturated |
| Identity provenance confusion | A persisted local or static username becomes an OAuth administrator after a mode switch | Persist identity source, activate only the configured mode, rotate credentials on proven bootstrap promotion, and require OAuth provenance for username-based admin |
| CLI sprawl | Every API endpoint becomes a command with divergent flags | Ship only create/hold/status/watch/serve; keep one-shot lifecycle mutations in HTTP |
| API drift | OpenAPI says one thing and Rust behavior does another | Contract tests and an explicit mismatch inventory before release |
| Skill injection | A public skill causes an agent to execute unrelated or destructive instructions | Keep the skill normative and narrow, review it as untrusted input, and never collect credentials or issue unrelated commands |
| Message overreach | “Agents collaborate” implies task assignment and merge correctness | Say “agents coordinate authority”; show the external boundary in every demo |
| Availability overclaim | A single SQLite authority is marketed as a distributed HA service | Keep the supported topology and no-cluster warning prominent |

## Implementation decisions

These choices are accepted for the active goal and form public compatibility constraints:

| Decision | Accepted disposition | Compatibility reason |
| --- | --- | --- |
| Long-running verb | Use `hold`; use “heartbeat” as explanation, not a second command | CLI names become a compatibility surface |
| Waiting behavior | `hold` waits by definition and indefinitely by default; documented quickstarts use `--acquire-timeout 60`; no first-release one-shot CLI mutations | Supports daemon takeover while keeping first-run examples bounded |
| Worker coupling | Keep work execution outside the CLI; require the tested supervisor to validate kind, name, candidate, sequence, and term and to stop the worker on authority loss; defer `--exec` | A hold process alone cannot stop an unrelated worker |
| Election watch | Include best-effort, bounded SSE with initial reconciliation, versioned snapshots, reconnect guidance, capacity limits, idle pruning, and close-on-lag behavior | Long-lived anonymous connections must fail observably and remain bounded |
| Hosted task claims | Keep hosted anonymous locks out; retain the existing hosted OAuth lock path and support the same bearer-authenticated locks when self-hosted | Preserves the auth, abuse, tenant, and private-deployment boundaries without contradicting the shipped dashboard |
| Signal exit status | On Unix, a clean release caused by SIGINT exits `130`; a clean release caused by SIGTERM exits `143`. A release event does not convert either signal into success | Supervisors depend on exact, conventional signal status |

The following are already dispositioned as out of scope: structured labels, one-shot token reveal in the CLI, arbitrary metadata records, directory semantics, and a child-process executor.

## Adversarial review disposition

Three independent reviewers returned `REQUEST_CHANGES` against the first implementation candidate. Their blocking findings are accepted as release gates, not waived:

| Review area | Accepted disposition | Current evidence state |
| --- | --- | --- |
| Session and lease authority | Fail closed on the earlier independent monotonic lease or session-confirmation deadline; long lock TTLs never extend a short session | Implemented; stalled-I/O and long-TTL regressions passed in the final local gate |
| Initial authority acceptance | Reject successful campaign/acquire responses that arrive after the request's monotonic safety budget, validate election/candidate identity, and never emit authority for a late or mismatched response | Implemented; delayed-success and mismatched-identity process tests passed in the final local gate |
| Capability secrecy | Lock watch uses a capability-free DTO; CLI output, stderr, argv, URLs, metadata, and logs must not reveal tokens, lease IDs, or session IDs | Implemented; same-token sibling and SSE leakage tests passed in the final local gate |
| Public holder privacy | Preserve the UUID-shaped `holder_id` compatibility field as a stable one-way public pseudonym; never expose an actionable session selector through held/status/list responses | Implemented; same-token inspect/terminate rejection tests passed in the final local gate |
| Watch resource safety | Bound and prune the channel registry; close on lag or serialization failure; surface capacity errors | Implemented; churn, capacity, lag, and cross-namespace tests passed in the final local gate |
| Watch transport safety | Bound response headers/error bodies by the configured request timeout, keep shutdown signal-aware, cap each SSE frame/pending buffer at 64 KiB, and distinguish global capacity from per-client rate limits | Implemented; stalled-response, oversized-frame, SIGTERM, and admission-shape tests passed in the final local gate |
| Deadline integrity | Acquisition and renewal deadlines cover send, headers, body, decode, and post-response acceptance | Implemented; election and lock stalled-header/body tests passed in the final local gate |
| Retry clock separation | Session keepalives may run while waiting but never shorten server acquisition guidance or authorize an early acquisition retry; retryable session creation remains inside the acquisition state machine | Implemented; long-guidance/short-keepalive and session-503 process tests passed in the final local gate |
| Session semantics | Only ephemeral session locks are released on expiry; startup removes orphaned ephemeral locks; persistent locks remain persistent | Implemented; runtime-expiry and restart tests passed in the final local gate |
| Session cleanup identity | Snapshot and revalidate lease, term, holder, session, and ephemeral state while holding the per-name entry before deleting a session lock | Implemented; deterministic release/reacquire cleanup race coverage passed in the final local gate |
| Same-holder reacquisition | Treat an idempotent same-holder acquire as an atomic renewal that preserves lease/term, never shortens expiry, persists before response, broadcasts renewal, and derives guidance from the actual expiry even at the holder limit | Implemented; persistence, watch-event, TTL, and holder-limit regressions passed in the final local gate |
| Read isolation | Default lock listings to the caller namespace and require session ownership with indistinguishable not-found responses | Implemented; cross-token isolation tests passed in the final local gate |
| OAuth browser handoff | Bind GitHub OAuth to five-minute single-use cookie state; redirect with only a 60-second one-time fragment code; exchange that code once through a no-store POST; never place an API bearer token in `Location` | Implemented; replay/location tests plus dashboard, OpenAPI, and site behavior contracts passed in the final local gate |
| Security configuration | Apply defaults only when variables are absent; reject malformed booleans, partial OAuth credentials, empty token/admin sources, invalid limits, and unreadable or malformed configured token files at startup | Implemented; negative configuration and checked-seeding tests passed in the final local gate |
| Supervisor safety | Validate candidate, sequence, positive stable term, relative monotonic safety budget, wall observation, and same-host monotonic observation; use the greater age so clock rollback cannot extend authority; detect silent loss and worker exit; make release/loss terminal; control hold and worker groups with bounded TERM-to-KILL escalation even after supervisor death | Implemented locally; initial/renewal rollback, watchdog-arm, handoff, supervisor-crash, and resistant-descendant regressions pass; exact frozen-candidate rereview remains pending |
| Staged, recoverable release publication | Validate the exact tagged SHA on protected `origin/main`, run local-equivalent checks and release fixtures, execute every native asset, publish the irreversible crate stage, finalize the draft release, then deploy and prove production. A same-tag rerun must detect an already-published crate and resume later stages; no lane is called complete until all stages pass | Workflow and release contracts pass locally; exact hosted CI, tagged release, native assets, crate publication, deployment, rollback, and production proof remain pending |
| Public experience | Browser creates one vacant shared room outside candidates and only observes; skill/CLI precede curl; mobile navigation remains complete and accessible; release links are pinned | Implemented; desktop/mobile rendered review and local site/behavior contracts passed; live proof remains pending |

The first hardened candidate's local gate passed on 2026-08-02: formatting and diff hygiene; 154 library tests; 173 binary tests; 19 CLI/supervisor end-to-end tests; five OpenAPI correspondence tests; three skill-contract tests; benchmark targets; package, shell, release-contract, OpenAPI, HTML, site, and behavior checks; and the checksum-install, two-agent failover, supervisor, and release fixture. That evidence is now superseded: a second independent review found additional blockers, so it is not evidence for the revised candidate.

### Second implementation review

All three reviewers returned `REQUEST_CHANGES` against the first hardened candidate. Every correctness, contract, installation, public-claim, and deployment finding below is accepted. None may be waived by a passing test in another lane.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Durable session keepalive | Commit the new expiry to SQLite before mutating memory or returning success; failed persistence must leave the old authority deadline intact | Deterministic persistence-failure test plus expiry/restart reconciliation |
| Session identity in the CLI | Accept a keepalive only when the response `session_id` equals the requested session | Mismatched-success response process test |
| SSE transport allocation | Reject a transport chunk that would exceed 64 KiB before appending or allocating the combined pending buffer | Oversized single-chunk regression |
| Positive authority budgets | A successful authority event must report `authority_remaining_ms > 0`; sub-millisecond remaining authority is rejected rather than rounded to zero | Delayed sub-millisecond response regression |
| Same-holder metadata | Idempotent reacquisition preserves current lease metadata and returns the exact committed snapshot, never the conflicting request field | Acquire response, status, and SQLite restart regression |
| GitHub upstream status | Explicit GitHub non-success responses map to the documented redacted `502 upstream_unavailable`, not `500` | Implementation/error-shape and OpenAPI correspondence tests |
| Installer first run | Create an absent install directory and print a loopback-only, non-placeholder first-run path that does not normalize `local:change-me` | Missing-directory install and output contract tests |
| Dashboard lease capability | Never infer or submit `lease_id` from `/locks`; list output intentionally omits that capability | Browser behavior contract and rendered control review |
| Live-demo response trust | A malformed HTTP 200 body is a failed/uncertain coordination result, never visual proof of coordination | Malformed-success browser contract |
| Legacy orchestration claim | Remove unqualified claims that OctoStore orchestrates or assigns work; describe authority arbitration and the external worker boundary | Site-wide claim scan |
| Interruption-safe deployment | On error or signal, retain a known-good backup, restore it, restart it, and verify rollback health; a partial rerun must be safe | Failure-injection, signal, restore-health, and rerun fixtures |
| Complete exact-site proof | Deployment must prove all release-critical pages and assets, not only `/` and the skill | Exact-SHA/version checks for home, agents, docs, dashboard, skill, installer, API health, and OpenAPI |
| Manual deploy provenance | A manual redeploy independently proves the requested release commit is contained in current protected `origin/main` | Release-contract negative tests for stale/non-main commits |
| Unicode lock-name contract | Preserve Rust's Unicode-alphanumeric compatibility; remove the ASCII-only schema regex and document actual UTF-8 byte ceilings | Validator/OpenAPI correspondence tests |
| Hosted formatting gate | Hosted CI runs `cargo fmt --check` in addition to the local equivalent | Workflow contract check |

The revised candidate must rerun the complete local gate after all dispositions land. Hosted CI, merge, release, publication, deployment, rollback exercise, and live acceptance remain distinct later gates. No reviewer has approved the revised candidate; fresh explicit `[AGREE]` from all three reviewers is mandatory before commit or merge.

### Third implementation review

The second hardened candidate passed its complete local gate, but all three reviewers again returned `REQUEST_CHANGES` after inspecting the exact API, CLI, skill, site, installer, release, and deployment surfaces. The green gate is therefore superseded. The following distinct findings are accepted as blockers:

| Finding | Required disposition | Current evidence state |
| --- | --- | --- |
| Session acquire/teardown race | Serialize active-session validation plus durable lock acquisition with explicit and expiry teardown; teardown cannot complete cleanup before a new session-bound lock appears. Add deterministic both-order regressions. | Implemented; both acquisition-first and teardown-first races pass, and explicit teardown refuses success when durable lock cleanup fails. |
| Supervisor deadline containment | Reject unusable budgets before worker launch and reserve watchdog plus TERM-to-KILL escalation entirely inside the authority deadline; no resistant worker may run after expiry. | Implemented; sub-second/no-start and tight-budget TERM-resistant worker regressions pass. |
| Credential-bearing dashboard supply chain | Execute no untrusted third-party runtime JavaScript in a page that reads or stores bearer credentials; enforce the boundary with a site contract. | Implemented across every discovered credential-bearing page. `dashboard.html` and `admin.html` use hash-pinned script CSPs and no third-party runtime JavaScript; the admin surface rejects legacy fragment tokens, uses DOM-safe rendering, and replaces CDN charts with native canvas. The duplicate credential-bearing `status.html` runtime is retired behind a no-script compatibility redirect to the hardened admin page. The contract auto-discovers credential use in inline scripts and rejects external runtime JavaScript site-wide. Static contracts and a mocked authenticated admin render passed with no external request or page/CSP error. |
| Privileged installer ownership | A system-path installation must create a root-owned `0755` binary rather than moving a user-owned temporary file under `sudo`; test ownership and mode through a safe privilege fixture. | Implemented; the privilege fixture validates root ownership, exact `0755` mode, and cleanup of installer temporary files. |
| SSE pre-mutation bound | Reject any append that would make the current unterminated frame exceed 64 KiB before mutating the pending buffer, while still accepting multiple bounded frames in one transport chunk. | Implemented; pre-mutation oversized-append and multi-frame transport regressions pass. |
| RFC 3339 browser validation | HTTP and SSE timestamps must satisfy the documented RFC 3339 shape and represent a real instant; permissive `Date.parse` acceptance is insufficient. | Implemented; strict shape-plus-round-trip validation is exercised for create, status, SSE, and lock responses. |
| Token rotation persistence | Validate the rotation response and atomically replace persisted dashboard auth so reload uses the new token rather than the revoked one. | Implemented; malformed replacement records fail closed and the validated complete record is persisted before in-memory replacement. |
| Live installer and health proof | Production and rollback proof must separately compare the public installer to the expected commit and validate the documented `/health` response, not infer either from service state or the site tree. | Implemented in the deployment contract; success, proof failure, checked rollback, and rollback-proof failure fixtures pass. Live production proof remains pending. |
| Negative provenance behavior | Behavioral fixtures must prove tag/commit mismatch and a release commit outside current `origin/main` fail before checkout, service, or binary mutation. | Implemented; both mismatch cases are rejected before mutation in the release-contract fixture. |

The main-agent pre-rereview pass found that legacy `admin.html` also carried credentials while loading unpinned Chart.js, then found the same class of issue in the duplicate D3-backed `status.html`; each discovery invalidated the candidate. The security boundary now covers every credential-bearing public page, the contract discovers those pages rather than relying on a hand-maintained list, and external runtime JavaScript is rejected site-wide. After those repairs, the revised candidate's complete local gate passed on 2026-08-02: formatting and diff hygiene; 162 library tests; 181 binary tests; 23 CLI/supervisor end-to-end tests; seven OpenAPI correspondence tests; three skill-contract tests; benchmark targets; warning-free package dry-run; Clippy with warnings denied; ShellCheck; deployment provenance and failure-injection contracts; OpenAPI and HTML validation; static and browser behavior contracts; and the checksum-install, privileged-install, two-agent failover, supervisor, and release fixture. Fresh unanimous rereview is still required. Hosted CI, merge, release, publication, deployment, rollback exercise, and live acceptance remain distinct later gates.

### Fourth implementation review

The third hardened candidate also passed its then-current local gate, but Curie, Tesla, and Russell each returned `REQUEST_CHANGES`. That green result is superseded. All eight distinct findings were accepted as release blockers and repaired:

| Finding | Final disposition | Current evidence state |
| --- | --- | --- |
| RFC 3339 leap-second over-acceptance | Browser validators reject second `60`; HTTP status, SSE state, and dashboard lock records all fail closed on that value | Focused browser behavior regressions and the complete site-behavior gate pass |
| Unsupported admin endpoints and unchecked responses | The admin page uses only `/admin/status`, `/admin/metrics/timeseries`, and `/metrics`; every response must be HTTP-successful and match a bounded schema before rendering. Unsupported `/admin/metrics` and `/admin/telemetry` calls and unimplemented OpenTelemetry claims are removed | Static endpoint contracts plus success, non-success, malformed snapshot, and misaligned time-series browser regressions pass |
| Installer exact-target confusion | Reject symlinks and every existing non-regular `.../octostore` target before placement; require the final target to be a regular executable file | Both ordinary and simulated privileged existing-directory regressions pass, alongside checksum, ownership, mode, and temporary-file cleanup proof |
| Durable locks correlated to sessions | A non-ephemeral lock uses the authenticated user as its durable holder even when `session_id` supplies lifecycle correlation; only ephemeral locks are session-held | Teardown followed by ACL update, renew, and release passes; post-teardown quota accounting remains attached to the user |
| Failed ephemeral cleanup abandonment | Retain expired session rows and orphaned ephemeral locks as durable retry state until checked SQLite deletion succeeds; startup and periodic reconciliation remain authority-fail-closed and retryable | Runtime lock-delete, session-row-delete, keepalive, startup/restart, and concurrent durable reacquisition regressions pass |
| Long-TTL supervisor false loss | Lock supervision retains the 20-second session-confirmation cap; election supervision instead arms from each event's relative authority budget and rearms after renewal, with shutdown reserve inside the deadline | Synthetic healthy 60- and 300-second election regressions pass, as do tight-budget and TERM-resistant worker tests |
| Private/corporate CA regression | Reqwest retains WebPKI roots, adds native OS roots, and accepts an additional PEM bundle through `OCTOSTORE_CA_BUNDLE` for both CLI HTTPS and webhook delivery | A generated private-CA TLS authority is accepted by the actual CLI child and an actual server webhook delivery; the test harness runs the child asynchronously so it cannot starve its own TLS server |
| Mutable third-party API-doc JavaScript | `/docs` and `/` are static, read-only API indexes with a restrictive HTTP CSP, no script, no credential input, and links to the versioned OpenAPI document and agent skill | Binary response tests assert no Scalar/CDN script and verify CSP/nosniff headers; README, site docs, and OpenAPI no longer claim an interactive console |

The optional security polish from review is also implemented: token-rotation responses are `no-store` and documented as such. The exact revised candidate passed `scripts/ci-local.sh` on 2026-08-02 with formatting and diff hygiene; 167 library tests; 187 binary/API tests; 25 CLI/supervisor end-to-end tests; seven OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, and exact-site contracts; OpenAPI and HTML validation; static and browser behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero and left no candidate server, supervisor, smoke directory, or worker process.

This is local candidate evidence only. Fresh literal `[AGREE]` from all three reviewers is still mandatory. Hosted CI, commit, push, PR replacement, merge, tag, release assets, crates.io publication, deployment, production version/content, clean-machine installation, and hosted two-agent failover remain separate pending gates.

### Fifth implementation review

The fourth candidate's complete local gate passed, but Curie, Tesla, and Russell returned `REQUEST_CHANGES` on a fresh rereview. That earlier green result is superseded. Their 12 distinct findings were accepted as release blockers and repaired; the independently duplicated webhook-log finding is counted once.

| Finding | Final disposition | Current evidence state |
| --- | --- | --- |
| Synthetic admin bearer escalation | The nil-UUID admin remains an in-process authorization sentinel only; startup deletes legacy nil-user rows, preload/database/cache/rotation paths reject nil users, and admin-key use never persists a fixed bearer | Before-use, after-use, injected-legacy-row, rotation, and restart regressions pass |
| Webhook redirect bypass | Webhook URLs are parsed structurally and must have an HTTPS scheme and host; the delivery client follows no redirects | Actual private-CA HTTPS callbacks returning 307 downgrade and 308 loopback-internal redirects are retried only at the original URL; neither target is reached |
| Webhook credential logging | Transport failures log only webhook ID and a bounded error class, never a formatted request error or URL | Captured-log regression uses a path/query sentinel and proves neither the secret nor target address appears |
| CORS preflight correlation | Request-ID middleware is outermost in production and the correspondence test router | Authorized preflight returns CORS headers plus a valid `req_` correlation ID |
| Read-only API messaging | Public surfaces call `/docs` a read-only API index; no interactive API or console is promised | Site contract rejects those claims across every public HTML file and README |
| Administrative OpenAPI drift | `/admin/status`, `/metrics`, and `/admin/metrics/timeseries` use finite named response schemas; supported windows are exactly `1h`, `12h`, `24h`, and `7d`; invalid values return the structured validation error | Runtime invalid-window test, binary route/schema assertions, eight external correspondence tests, and warning-free Redocly validation pass |
| Published native asset proof | Every matrix job records asset SHA, exact version output, and successful execution evidence; the draft-release job enumerates the exact seven assets, downloads them, checks `SHA256SUMS`, and links each native byte hash back to its matrix evidence before crates.io publication or finalization | Static release contract and ShellCheck pass; hosted four-platform execution and draft-download proof remain a later release gate |
| Webhook holder/tenant isolation | Delivery is restricted to the authenticated owner of a durable or active session-held lock; holder IDs use the same one-way public pseudonym as HTTP and the callback body is an exact OpenAPI callback schema | Same-token session-capability and cross-user selection regressions pass; private-CA delivery remains green |
| Session renewal race | Ephemeral renew and release mutations execute inside the same active-session lifecycle guard as acquisition and teardown | Deterministic in-flight renewal-versus-teardown barrier proves teardown cannot be crossed or strand the renewed lock |
| Supervisor HUP containment | The supervisor traps HUP, exits 129, and checks both detached worker and hold process groups after TERM-to-KILL escalation | Process-group regression proves a HUP removes TERM-resistant worker, descendant, and hold processes |
| Failed expired-lock startup cleanup | Startup propagates any expired-row deletion failure instead of omitting that row from memory and abandoning it | Injected expired session/lock deletion failure blocks startup, retains durable rows, and succeeds after repair with both rows reconciled |
| Racy 100-lock limit | Principal-wide active-lock count and admission share the store admission guard across durable and active session-held locks; same-holder renewal remains exempt | Concurrent 99-to-100 boundary regression admits exactly one user/session contender and rejects the other |

The exact revised candidate then passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 174 library tests; 195 binary/API tests; 27 CLI/supervisor end-to-end tests; eight OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, and exact-site contracts; warning-free OpenAPI validation; static and browser behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero and left no candidate server, supervisor, worker, or smoke directory running.

This is still local candidate evidence only. Fresh literal `[AGREE]` from Curie, Tesla, and Russell is mandatory against this exact fifth candidate. No commit, push, replacement PR, hosted CI, merge, tag, release, crates.io publication, deployment, or production acceptance has occurred.

### Sixth implementation review

The fifth candidate passed its complete local gate, but Russell found one additional credential-transport blocker during the exact-candidate rereview. That green result is superseded.

| Finding | Final disposition | Current evidence state |
| --- | --- | --- |
| Credentialed CLI redirect downgrade | The CLI follows no HTTP redirects. API redirects are outside the CLI contract, and disabling them prevents a bearer token from crossing any scheme or authority boundary that `validate_server` did not approve. | The actual CLI child connects through a generated private CA, sends its token only to the validated HTTPS origin, receives a 307 redirect to cleartext HTTP on the same host and port, exits with the software-error contract, and delivers neither a target request nor an authorization header. The focused and complete-gate regressions pass. |

The exact revised candidate then passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 174 library tests; 195 binary/API tests; 28 CLI/supervisor end-to-end tests; eight OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, and exact-site contracts; warning-free OpenAPI validation; static and browser behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero and left no candidate server, supervisor, worker, or smoke directory running.

No previously returned reviewer disposition counts as approval of this revised candidate. Fresh literal `[AGREE]` from all three reviewers is required. All hosted and production gates remain separately pending.

### Seventh implementation review

The sixth candidate passed its complete local gate and Russell and Tesla returned `[AGREE]`, but Curie found an unauthenticated credential-disclosure path. Those agreements and that green result are superseded.

| Finding | Required disposition | Current evidence state |
| --- | --- | --- |
| Existing-token disclosure through local registration | Local registration is disabled by default; explicit enrollment requires `LOCAL_REGISTRATION=true`, an explicit numeric loopback bind, and no OAuth or static-token source. Each username may be enrolled once. Repeated registration or a static-user collision must never return the existing token. Successful one-time token responses are `no-store`. Unmatched routes return structured `404` rather than the API index. | Focused unit, configuration, OpenAPI-correspondence, and actual child-server HTTP regressions pass, as does the complete revised-candidate gate. |

The exact revised candidate then passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 177 library tests; 198 binary/API tests; 29 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, and exact-site contracts; warning-free OpenAPI validation; static and browser behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero and left no candidate server, supervisor, worker, or smoke directory running.

Fresh literal `[AGREE]` from Curie, Tesla, and Russell is required against the revised candidate. No commit, push, replacement PR, hosted CI, merge, tag, release, crates.io publication, deployment, or production acceptance has occurred.

### Eighth implementation review

The seventh candidate passed its complete local gate, but Tesla found that case-sensitive local-ID derivation and case-insensitive ACL principals allowed `alice` and `ALICE` to become separate bearer identities with the same ACL authority. That green result is superseded.

| Finding | Required disposition | Current evidence state |
| --- | --- | --- |
| Case-variant identity and ACL impersonation | Usernames are unique case-insensitively at the SQLite layer across local, static, and OAuth identities. Registration checks both local ID and case-insensitive username under the DB mutex; static seeding fails closed on any conflicting identity or token; ambiguous legacy databases fail startup. Mixed-case and legacy OAuth collisions return `409` without a token. | Focused unit, actual child-server HTTP, startup-migration, and OpenAPI correspondence regressions pass, as does the complete revised-candidate gate. |

The exact revised candidate then passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 179 library tests; 200 binary/API tests; 29 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, and exact-site contracts; warning-free OpenAPI validation; static and browser behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero and left no candidate server, supervisor, worker, or smoke directory running.

Fresh literal `[AGREE]` from all three reviewers is required. All hosted and production gates remain separately pending.

### Ninth implementation review

The eighth candidate passed its complete local gate, but Russell found that static credentials were still seeded entry by entry and that the exact-match check omitted the stored username and multiple-row case. That green result is superseded.

| Finding | Required disposition | Current evidence state |
| --- | --- | --- |
| Partial static-token persistence after failed startup | Validate the complete combined static-token set and every database uniqueness/conflict match before insertion; insert all new users in one SQLite transaction; mutate the token cache only after commit. A late conflict must leave database and cache unchanged across restart. | Focused all-or-nothing database/cache/restart regression and the complete gate pass. |
| Incomplete persisted-identity comparison | Query all rows matching local ID, case-insensitive username, or token; accept exactly one row only when ID, username, token, and non-nil UUID all identify the same principal. Reject hash collisions and multi-row `OR` matches. | Focused legacy hash-collision, multiple-row, ordinary idempotent-seeding regressions, and the complete gate pass. |

The exact revised candidate then passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 181 library tests; 202 binary/API tests; 29 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, and exact-site contracts; warning-free OpenAPI validation; static and browser behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero and left no candidate server, supervisor, worker, or smoke directory running.

Fresh literal `[AGREE]` from all three reviewers is required. All hosted and production gates remain separately pending.

### Tenth implementation review

The ninth candidate passed its complete local gate, but Curie identified two storage-boundary races. That green result is superseded. Both findings are accepted as release blockers; neither may be repaired only in the HTTP handler because the store is the authority shared by every caller.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Stale former-holder ACL overwrite | Authorize and persist an ACL update while retaining the occupied per-name entry guard; revalidate the exact holder, lease ID, and fencing generation so a holder that loses and then fails to reacquire the lock cannot overwrite the new holder's sticky ACL | A barrier-controlled release/reacquire regression proves the stale generation is rejected and the new generation's ACL remains durable |
| Non-atomic initial lock and ACL | Pass the requested initial ACL into the store acquisition path; enforce sticky-ACL compatibility and persist a newly acquired lock plus its initial ACL in one SQLite transaction; publish the in-memory lock and acquisition event only after commit | Deterministic ACL-write-failure regressions prove no lock or acquisition event is published, while restart proves no unprotected durable lock survives |

Both findings are repaired at the shared store boundary. Acquisition carries the ACL snapshot authorized by the caller, revalidates it under the per-name entry guard, and commits a first lease plus ACL in one SQLite transaction before publishing memory or events. Holder updates retain that guard through persistence and compare holder, lease ID, and fencing generation; session-held updates also retain the active-session lifecycle guard. The injected ACL-write-failure test leaves no SQLite lock/ACL row, in-memory lease, or acquisition event and remains empty after restart. A barrier-controlled former-holder release/reacquire test rejects the stale generation and preserves the replacement ACL across restart.

The exact repaired candidate passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 184 library tests; 205 binary/API tests; 29 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; and checksum install, absent/existing-target/privileged install, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up process and filesystem inspection found no candidate server, supervisor, worker, or smoke directory.

No earlier reviewer disposition counts as approval of this repaired candidate. Fresh literal `[AGREE]` from Curie, Tesla, and Russell is required against the exact candidate. Commit, push, replacement PR, hosted CI, merge, release, deployment, and production acceptance remain pending.

### Eleventh implementation review

The tenth candidate passed its complete local gate and Curie returned `[AGREE]`, but Tesla and Russell returned `REQUEST_CHANGES`. Curie's approval and the green result are superseded. All four findings are accepted as release blockers:

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Ambient proxy captures loopback bearer traffic | Force credentialed loopback CLI requests to use direct transport regardless of inherited `HTTP_PROXY`, `HTTPS_PROXY`, or `ALL_PROXY` settings | An actual child-process regression points every proxy variable at a malicious listener, removes `NO_PROXY`, successfully reaches the intended loopback API, and proves the proxy receives no request or token |
| Lock delay bypass during release/acquire | Publish cooling state before relinquishing the per-name lock entry; revalidate cooling after acquisition obtains that same entry; prevent stale cooling cleanup from deleting a newer delay | A barrier-controlled release/acquire regression proves a caller that passed the early check cannot persist until release finishes and then receives the authoritative delayed outcome |
| Fallback installer prints a broken bare command | Print a safely shell-quoted exact installed path in the first-run recipe rather than relying on `PATH` | The release fixture executes the complete printed startup recipe with the custom destination deliberately absent from `PATH`, proves `/health`, and cleans up the server |
| ACL race regression does not enforce the repaired boundary | Reacquire with the same holder UUID but a new lease and fencing generation; pause after generation validation while the per-name entry guard remains held | A deterministic hook proves release/reacquire cannot pass until ACL persistence releases the guard, rejects the old generation afterward, and preserves the replacement ACL across restart |

All four findings are repaired. Credentialed loopback CLI clients disable inherited proxy routing. Release, expiry, and session cleanup publish cooling state before removing the occupied entry, and acquisition revalidates the authoritative delay after obtaining the vacant entry. The installer prints the exact shell-quoted installed binary path and a backgrounded, health-probed first-run recipe. The ACL race hook now pauses after exact same-principal generation validation while retaining the per-name guard, so release/reacquire remains blocked until persistence completes.

The exact eleventh candidate passed its focused regressions and `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 185 library tests; 206 binary/API tests; 30 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry-run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up process, port, and filesystem inspection found no candidate server, supervisor, worker, or smoke directory.

No tenth-round disposition applies to this eleventh candidate. Fresh literal `[AGREE]` from Curie, Tesla, and Russell is mandatory against these exact bytes before commit or hosted work. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, and production acceptance remain separately pending.

### Twelfth implementation review

The eleventh exact candidate passed its complete local gate, but Russell returned `REQUEST_CHANGES` after finding that delay enforcement depended on periodic-cleanup timing. That green result and every in-progress eleventh-candidate review are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Expired occupied lock bypasses configured delay | When acquisition encounters an expired occupied generation with a nonzero `lock_delay_seconds`, durably remove that exact generation, publish cooling before removing the per-name entry, emit expiry, and return the authoritative delayed outcome rather than replacing it immediately | Deterministic store coverage proves identical delayed behavior when acquisition arrives before periodic cleanup and after periodic cleanup; an API regression proves the pre-cleanup request receives structured `409 delayed` and cannot install a successor lease |

The store's expired-entry branch now performs durable deletion and cooling publication under the same per-name guard before removing the expired generation. It returns `AcquireLockOutcome::Delayed`, so the HTTP handler reports the same machine-readable delay regardless of cleanup timing. Deterministic store and API regressions pass for both cleanup orderings.

The exact twelfth candidate passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 187 library tests; 208 binary/API tests; 30 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up process, port, and filesystem inspection found no candidate server, supervisor, worker, or smoke directory.

Fresh literal `[AGREE]` from Curie, Tesla, and Russell is mandatory against the final evidence-bearing bytes. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, and production acceptance remain separately pending.

### Thirteenth implementation review

The twelfth exact candidate passed its complete local gate, but Curie returned `REQUEST_CHANGES` with two additional release blockers. That green result and every in-progress twelfth-candidate review are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Election hold treats a later bearer capability as public transport | Separate hosted-default server selection from capability-bearing transport policy; protect `election hold` from its first campaign request through renew/resign; require HTTPS or loopback unless an explicit development override is supplied; bypass ambient proxies on loopback | An actual hold child rejects non-loopback cleartext HTTP without override, points every proxy variable at a malicious listener with no bypass environment, reaches the intended loopback authority, becomes leader, resigns, and proves the proxy receives no campaign, response, renewal, resignation, or token |
| Cooling tombstones disappear on restart | Persist release/expiry cooling in SQLite atomically with exact-generation deletion; restore active tombstones and prune expired ones at startup; consume expired tombstones durably before successor acquisition | Release-to-restart and expiry-to-restart regressions preserve `available_at`, return `Delayed`, and install no successor lease; injected tombstone-write failure rolls back lock deletion and leaves the original generation authoritative |

The CLI now distinguishes whether a command requires an explicitly selected server from whether its transport will carry a secret capability. Hosted election hold retains its default HTTPS authority, but custom non-loopback cleartext HTTP requires `--allow-insecure-http`, and loopback hold clients use direct transport. The store now owns a durable `lock_cooling` table. Every release, runtime expiry, startup expiry, and ephemeral-session cleanup atomically replaces the lock row with any required cooling tombstone before publishing memory; startup restores active tombstones and removes expired ones. Focused CLI, restart, transactional-failure, and prior race regressions pass.

The exact thirteenth candidate passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 189 library tests; 210 binary/API tests; 31 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up process, port, and filesystem inspection found no candidate server, supervisor, worker, or smoke directory.

That thirteenth green result is superseded by the fourteenth review below. No thirteenth-candidate disposition applies to later bytes.

### Fourteenth implementation review

The thirteenth candidate passed its complete local gate, but Curie returned `REQUEST_CHANGES` after finding that its new `lock_cooling` table was invisible to the v0.13.2 rollback binary. A failed v0.14 deployment could therefore restore a healthy old process that admitted a successor before the promised cooldown ended; a later v0.14 retry could then encounter incompatible durable states. The thirteenth green result and every in-progress review are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Durable cooling is not downgrade-safe | Do not add a separate persistence noun. Represent cooling as an impossible sentinel row in the existing `locks` table. The sentinel retains the prior fencing term, expires at `available_at`, stores zero in `lock_delay_seconds` so v0.13.2 cannot add a second delay, and encodes the original delay only in a reserved marker. v0.14 recognizes the complete impossible shape, never marker text alone; malformed impossible rows fail startup. Exact-generation release/expiry transitions and exact-sentinel cleanup remain transactional. | Store regressions prove release and expiry persistence, restart restoration, exact sentinel shape, marker-spoof resistance, malformed-sentinel fail-closed behavior, and rollback on an injected sentinel update failure. A child-process fixture builds the pinned v0.13.2 rollback commit and fresh v0.14 candidate bytes, proves v0.13.2 reports the name held before `available_at`, proves v0.13.2 can acquire after cooldown without a second delay, then proves v0.14 restarts and retains that exact successor lease. |

The separate cooling table and every runtime query to it are removed. Cooling now updates the existing durable lock generation in place to a sentinel whose holder, lease, and session UUIDs are nil, whose `acquired_at` equals `expires_at`, whose ephemeral flag and stored delay are zero, and whose metadata carries the versioned delay marker. Identification requires that entire structural identity plus a valid positive marker. Release, runtime expiry, startup expiry, and session cleanup publish the in-memory delay only after the exact durable generation becomes that sentinel. Expired cleanup conditionally deletes only the matching sentinel.

The exact fourteenth candidate passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 191 library tests; 212 binary/API tests; 31 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; the pinned real-v0.13.2 downgrade/forward storage fixture; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up process, port, worktree, and filesystem inspection found no candidate server, supervisor, worker, smoke directory, or temporary compatibility checkout.

Fresh literal `[AGREE]` from Curie, Tesla, and Russell is mandatory against the final evidence-bearing bytes. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, and production acceptance remain separately pending.

### Fifteenth implementation review

The fourteenth candidate passed its complete local gate twice, but Codex's independent review found that the behavioral downgrade fixture allowed up to 20 seconds for a configured 10-second cooldown. Although the sentinel stored zero in v0.13.2's delay field, that upper bound could under favorable scheduling admit an accidental second cooldown without disproving the fixture. The fourteenth green results and the just-started reviewer round are superseded before any disposition counts.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Downgrade fixture does not strictly disprove a second cooldown | Anchor the deadline when v0.14 releases the lease, require v0.13.2's held response to expose the exact same expiry as v0.14's `available_at`, and require v0.13.2 to acquire within a bound that permits one configured cooldown plus process jitter but cannot permit two. | The tightened pinned-binary fixture fails if the rollback process changes the deadline or applies a second 10-second delay, while retaining the held-before-deadline and exact forward-lease checks. |

The tightened fixture now anchors its upper bound at v0.14 release, proves v0.13.2 reports the exact same deadline, and admits only six seconds of process jitter beyond the single configured 10-second cooldown. It still proves held-before-deadline behavior and exact forward preservation of the v0.13.2 successor lease.

The exact fifteenth candidate passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 191 library tests; 212 binary/API tests; 31 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; the tightened pinned-v0.13.2 one-cooldown downgrade/forward fixture; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero.

Fresh literal `[AGREE]` from all three reviewers is required only after these evidence-bearing bytes pass once more unchanged and are frozen and resubmitted. Every hosted and production gate remains separately pending.

### Sixteenth implementation review

The fifteenth candidate was frozen at `407454ff480bd01e5ef25d72e8702cfd55249ed942dba6c7774eab1931e02ad3`, but Tesla returned `REQUEST_CHANGES` with two P0 findings. That candidate, its green gate, and every fifteenth-candidate disposition are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Protected work can run before its process group is published to the watchdog | Launch the isolated worker group behind a start gate. Arm and handshake with the authority watchdog, validate that the launcher PID is the process-group ID, atomically publish and reread that ID, and only then release the gate. An independent deadline marker must prevent a paused supervisor from releasing work after watchdog expiry. | A deterministic test pauses the supervisor at the published-but-unreleased handoff, waits beyond the authority deadline, and proves both that the worker produced no side effect and that its process group no longer exists before the supervisor resumes. Existing signal, release, renewal, tight-budget, resistant-descendant, and uncertainty supervisor tests remain green under the repository's serialized gate. |
| A self-hosted GitHub OAuth callback sends its one-time code to the hosted dashboard, which exchanges and continues against the hosted API | Validate the exact callback and dashboard authorities at startup; derive the issuing API origin from the callback; require an explicit dashboard for non-hosted OAuth; redirect with both the one-time code and issuer; admit the configured dashboard origin through CORS; validate and persist the issuer in the dashboard; and use it for exchange, lock, rotation, and authenticated admin requests. | A real loopback HTTP flow with a mock GitHub upstream proves begin, browser-bound state, callback, configured dashboard target, exact issuer, configured-origin preflight, successful token retrieval, and rejected code reuse. Browser-behavior tests prove URL scrubbing before exchange, fail-closed missing/unsafe issuers, self-hosted lock and rotation requests, durable issuer reload, and the authenticated admin handoff. Configuration tests reject unsafe URLs and missing self-host dashboard configuration. |

The reference supervisor now starts the watchdog sleep before acknowledging readiness, launches the worker's new session behind a filesystem gate, verifies the process-group identity with `ps` and a group signal probe, atomically publishes and rereads the group ID, and checks the independent expiry marker immediately before creating the start gate. The watchdog writes that marker before notifying the supervisor and retains its direct KILL fallback against the published group. The deterministic handoff fixture uses a test-only pause point after publication, sends `SIGSTOP` to the supervisor, and confirms that no worker instruction executes across expiry.

GitHub OAuth configuration now accepts only HTTPS authorities or explicit loopback HTTP development authorities, requires the exact `/auth/github/callback` path, forbids URL credentials, queries, and fragments, derives a pathless API issuer, and requires `OAUTH_DASHBOARD_URL` for every non-hosted issuer. The callback includes that issuer beside the short-lived one-time code. The credential-bearing dashboard and authenticated admin handoff validate the origin, retain it with the bearer credential, and construct every API request from it; their hash-pinned CSP permits arbitrary HTTPS self-hosts and only explicit loopback HTTP hosts. The OpenAPI contract, environment example, README, changelog, CORS policy, static contracts, and behavioral harness describe and enforce the same flow.

Focused validation passes: shell syntax and ShellCheck; all nine serialized reference-supervisor tests including the paused handoff; 16 configuration tests; the real self-hosted OAuth HTTP flow; callback capability-leak and single-use coverage; all-target/all-feature compilation; OpenAPI lint; credential-surface CSP hashes; HTML validation; and site/browser behavior.

The first complete-gate attempt passed all Rust and CLI/supervisor suites but exposed that the revised OpenAPI callback example had moved outside the schema location asserted by the correspondence test. That attempt is superseded. The example and correspondence assertion now jointly require both `exchange_code` and the encoded issuer.

The corrected sixteenth candidate then passed `scripts/ci-local.sh` on 2026-08-02: formatting and all-target checks; 194 library tests; 216 binary/API tests including the real self-hosted OAuth flow; 32 CLI/supervisor end-to-end tests including the paused launch handoff; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry run; ShellCheck; deployment provenance, failure-injection, checked-rollback, health, installer, release, site, OpenAPI, HTML, and browser-behavior contracts; the pinned real-v0.13.2 one-cooldown downgrade/forward fixture; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up process, listener, worktree, and filesystem inspection found no candidate server, supervisor, worker, listener, smoke directory, or temporary compatibility checkout.

The required unchanged-byte rerun exposed a timing-sensitive test fixture before the candidate could be frozen: `lock_hold_jsonl_distinguishes_held_from_delayed` allowed only 300 milliseconds for session creation, the first acquire request, and observation of the server's waiting state, so a loaded host could reach the acquisition deadline and emit only `acquire_timeout`. This did not change the CLI contract or its production default. The fixture now grants two seconds for acquisition while still requiring the distinct `held` or `delayed` event before the terminal timeout. Twenty consecutive serialized focused runs passed on 2026-08-04. The prior candidate and its unchanged-byte claim are superseded; a complete gate and a second complete gate against unchanged evidence-bearing bytes are still required.

The next complete-gate attempt cleared every Rust, CLI, benchmark, Clippy, and packaging stage, then exposed that the deployment failure fixture inherited the operator's global Git commit-signing and hook configuration. A concurrent GPG key-database lock made the fixture's first synthetic commit time out before the deployment checks began. The synthetic repository now disables commit and tag signing and points `core.hooksPath` at an empty fixture-owned directory. ShellCheck and the complete behavioral release contract pass with the operator's global signed-commit policy still enabled. This was a test-isolation failure, not passing product evidence, so that complete-gate attempt is superseded too.

The repaired sixteenth candidate passed `scripts/ci-local.sh` on 2026-08-04: formatting and all-target checks; 194 library tests; 216 binary/API tests including the real self-hosted OAuth flow; 32 CLI/supervisor end-to-end tests; nine OpenAPI correspondence tests; three skill-contract tests; benchmark targets; Clippy with warnings denied; a warning-free crates.io publish dry run; ShellCheck; behavioral deployment provenance, failure-injection, rollback, public-health, exact-site, and installer contracts; the pinned real-v0.13.2 one-cooldown downgrade/forward fixture; OpenAPI lint; HTML, static-site, and browser-behavior contracts; and checksum install, absent/existing-target/privileged install, complete printed first-run recipe, two-agent failover, supervisor cancellation, and full release-fixture smoke. The run exited zero. Follow-up inspection found no candidate server, supervisor, worker, smoke listener, smoke directory, or temporary compatibility worktree; only the four pre-existing repository worktrees remained.

Because this evidence changes the candidate bytes, the complete local gate must pass once more against unchanged bytes before the digest is frozen. Fresh literal `[AGREE]` from Curie, Tesla, and Russell is mandatory against only that frozen digest. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, and production acceptance remain separately pending.

### Seventeenth implementation review

The sixteenth exact candidate was frozen at `a88924d448cbdfb08aca885c4cc5648bc4fc1ae0469c9757d2d803f9f42cfa97`, but Tesla returned `REQUEST_CHANGES` with one P0 supervisor finding. That digest, its green gates, and every prior disposition are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Renewal stopped and waited for the active authority watchdog before arming its replacement, so a paused supervisor could leave a resistant worker without an independent deadline | Keep the old watchdog live while the replacement starts and acknowledges readiness. Track both watchdogs during the handoff so cleanup and signals stop both. Retire the old watchdog only after the replacement is proven live, and fail closed if either prior expiry or replacement failure is observed. | A deterministic renewal-handoff test starts a resistant worker under a long initial budget, supplies a short renewed budget, pauses the supervisor after the replacement watchdog is ready but before the old watchdog retires, waits beyond the replacement deadline, and proves both that no post-deadline side effect occurred and that the worker process group no longer exists. All existing launch-handoff, renewal, signal, uncertainty, and resistant-descendant tests remain green. |

The supervisor now retains the prior watchdog in a separately tracked retiring slot, arms and handshakes with the replacement first, and only then terminates the prior monitor. Signal, cleanup, expiry, and replacement-failure paths stop both slots. The shared expiry marker makes a prior deadline reached during overlap terminal rather than allowing the new relative budget to resurrect work. A test-only renewal handoff pauses only after the replacement is independently live.

Focused validation passes: POSIX shell syntax; ShellCheck; eleven consecutive runs of the deterministic renewal-handoff regression; and all ten serialized supervisor-focused end-to-end tests, including initial gated launch, healthy long-TTL renewal, tight-budget TERM-resistant teardown, detached descendants, signal semantics, and renewal overlap. The repaired evidence-bearing candidate must pass the complete local gate twice unchanged before its digest is frozen and submitted for fresh literal `[AGREE]` from all three reviewers. No hosted or production gate has run.

### Eighteenth implementation review

The seventeenth exact candidate was frozen at `5d613d32e0e0097df8a21e98c7f5647b4f14e39a6ef32635f575fb952940ecf1` and passed the complete local gate twice unchanged. Curie nevertheless returned `REQUEST_CHANGES` with a P0 rollback-compatibility finding. That digest, both green gates, and every disposition against it are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| The durable cooldown sentinel used the nil UUID as its holder, but v0.13.2 authenticates the supported `ADMIN_KEY` principal as that same nil UUID. After a rollback, admin acquisition could therefore be treated as idempotently acquired, and nil-lease renewal or release could mutate or delete the cooldown before its deadline. | Use a sentinel holder identity that cannot equal any principal v0.13.2 can issue. Keep the existing on-disk `locks` representation and all one-cooldown semantics. | Extend the pinned real-v0.13.2 fixture with `ADMIN_KEY`. Before `available_at`, prove admin acquisition remains `held`, nil renewal cannot mutate the sentinel, nil release cannot delete it, the exact deadline remains unchanged, and the existing ordinary-token acquisition, single-cooldown, successor, and v0.14 forward-restart assertions still pass. |

The sentinel holder is now a non-nil, non-v4 UUID. The rollback binary issues only nil for its configured admin and `Uuid::new_v4()` identities for ordinary static, local, and OAuth users, so no supported authenticated v0.13.2 principal can equal the sentinel holder. The lease and session sentinel fields remain nil and the metadata/deadline shape remains unchanged. The pinned rollback fixture now starts both versions with an admin key and performs the required admin acquire, renew, release, exact-deadline, one-cooldown, successor, and forward-restart checks before reporting success.

Focused validation passes: Rust formatting; POSIX shell syntax; ShellCheck; a unit invariant proving the holder is neither nil nor version 4; and the complete pinned v0.13.2 downgrade/forward fixture with admin collision attempts. The repaired evidence-bearing candidate must pass the complete local gate twice unchanged before a new digest is frozen and submitted for fresh literal `[AGREE]` from Curie, Tesla, and Russell. No hosted or production gate has run.

### Nineteenth implementation review

The eighteenth exact candidate was frozen at `782854dd9e13edcaf1bc0fdd17854021c8bcb7db9796c4dc63680c2a73a2308c` and passed the complete local gate twice unchanged. Russell nevertheless returned `REQUEST_CHANGES` with two P1 release-provenance findings. That digest, both green gates, and every disposition against it are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Cargo's default package discovery admitted ignored `.gstack` runtime files, including a credential-bearing filename, even though those files were outside the frozen Git manifest. The publish dry run checked warnings but not exact archive contents. | Replace broad default discovery with a strict package allowlist. Compare the generated archive with the reviewed package list and reject every unexpected path, including hidden runtime and credential files. Never inspect or print credential contents. | The final package list and `.crate` archive contain only the explicitly intended Rust, OpenAPI, license, README, benchmark, test, skill, and reference-supervisor files plus Cargo's generated metadata; package verification is warning-free and rejects any extra path. |
| A rerun treated any existing crates.io version as sufficient and did not prove checksum or yank state before skipping the irreversible publish step. | Build the exact candidate archive first. Skip publication only when crates.io metadata reports the same version, an exact SHA-256 match, and `yanked: false`. After a new upload, poll until the same exact proof is visible before finalizing the release. | Deterministic fixtures cover absent, exact, mismatched, and yanked metadata; workflow and release-contract checks require the exact verifier before and after publication. |

`Cargo.toml` now allowlists the package surface. The package gate independently enumerates and validates that surface, executes the publish dry run without unexpected warnings, lists the generated archive, normalizes its versioned root, and requires an exact list match. The resulting local candidate archive contains 31 intended files and no `.gstack` or other unreviewed runtime path. The crates.io verifier compares the candidate archive SHA-256, version, and yank state against the version metadata; it returns only `absent` or `exact`, and any mismatch, malformed response, unexpected status, or yanked version is terminal. The release job packages first, uses that verifier for resume, publishes only when absent, and requires the exact checksum to become visible before continuing.

Focused validation passes: Rust formatting; POSIX shell syntax; ShellCheck; the strict package-list/archive gate; warning-free publish dry run; absent/exact/mismatched/yanked crates.io resume fixtures; the release workflow contract; deployment failure/provenance/rollback fixtures; and diff hygiene. The repaired evidence-bearing candidate must pass the complete local gate twice unchanged before a new digest is frozen and submitted for fresh literal `[AGREE]` from Curie, Tesla, and Russell. No hosted or production gate has run.

### Twentieth implementation review

The nineteenth repaired candidate passed one complete local gate on 2026-08-04. Its required unchanged-byte rerun cleared formatting, 195 library tests, 217 binary/API tests, 33 CLI/supervisor end-to-end tests, nine OpenAPI correspondence tests, three skill-contract tests, benchmarks, Clippy with warnings denied, the strict package/publish contract, release provenance and rollback fixtures, the pinned v0.13.2 downgrade fixture, OpenAPI lint, HTML, static-site, and browser-behavior checks, and two-agent failover. The final supervisor smoke then exited before its worker-start marker and erased the subprocess evidence during cleanup. That rerun is a failure, not frozen-candidate evidence, and the prior digest is superseded.

The smoke wrapper now reports the supervisor exit status and bounded supervisor, hold, and server diagnostics before cleanup on this early-exit path. This does not weaken the worker gate, timeout, cancellation, or secret-shape assertions. The focused supervisor smoke passed once in isolation, twenty additional concurrent-contention runs, and the complete released-binary install/two-agent/supervisor fixture. The original early exit did not reproduce, so no product-correctness cause is claimed.

Because the diagnostic repair changes the evidence-bearing bytes, this candidate must pass the complete local gate twice unchanged before a new digest is frozen. Only then may Curie, Tesla, and Russell perform fresh read-only review, and each must return a literal standalone `[AGREE]`. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, and production acceptance remain separately pending.

### Twenty-first implementation review

The twentieth candidate was frozen at `3c3250ad6caabb2b5dd660fb196a861f69fcd7a9e2763e6b426e6e981e278d59` and passed the complete local gate twice unchanged. Tesla nevertheless returned `REQUEST_CHANGES` with one P0 supervisor finding. That digest, both green gates, and every disposition against it are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| If a renewal watchdog replacement failed to arm or failed post-arming validation, the failure path stopped both the replacement and still-valid prior watchdog before EXIT cleanup synchronously contained the worker. A paused, killed, or crashed supervisor in that interval could leave resistant protected work without an independent deadline. | A failed replacement may stop only its own candidate process. Preserve the prior watchdog, or both watchdogs after successful replacement arming, until the worker process group has been synchronously stopped and checked. Retire monitors only after containment. | Deterministically fail replacement arming, pause the supervisor after it detects that failure, and prove the prior watchdog independently prevents a TERM-resistant worker and descendant from surviving or producing a post-deadline side effect. Keep the successful replacement-overlap, initial gated-launch, signal, and uncertainty regressions green. |

The replacement arming path now stops only the failed candidate monitor. On failure it restores the prior watchdog as active, exposes a test-only pause point, synchronously stops the published worker group, and only then retires the monitor. Post-arming validation failure likewise contains the worker while both watchdog slots remain live before stopping either monitor. The repaired candidate passed ten consecutive failure-injection runs, all eleven supervisor-focused tests, and two complete unchanged-byte local gates at digest `735620c74c7e55b0ee3d43b53e4c707cac38a246f27879e585b33b47b188a950` on 2026-08-04. No hosted or production gate ran for that superseded digest.

### Twenty-second implementation review

The twenty-first candidate above passed its required exact-byte gates, but Curie and Russell returned `REQUEST_CHANGES`. That digest, both green gates, and every disposition against it are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| Public pages and the skill described locks as self-host-only while the shipped dashboard, production API origin, and hosted OAuth callback advertised authenticated hosted locks. | Keep anonymous hosted locks out, but state the actual supported topology: no-login hosted elections; bearer-authenticated hosted locks through GitHub sign-in; the same authenticated locks when self-hosted for private traffic or identity. | Config coverage binds the exact hosted callback to the hosted dashboard and origin; site contracts require consistent topology copy and the dashboard's hosted lock requests; skill, homepage, docs, README, OpenAPI, and examples agree. |
| Election renew and resign documented bodyless `404` responses while real stale-capability handlers returned structured `lease_not_current` errors; unsigned retry and renewal fields omitted their zero lower bound. | Reference the reusable `LeaseNotCurrent` response for both mutations and add `minimum: 0` to all three election delay fields. | Real handler correspondence releases one term, retries renew and resign with the stale capability, proves structured `404 lease_not_current` bodies, and asserts the exact OpenAPI refs and numeric bounds. |
| The OpenSpec header still named the nineteenth candidate and described the twenty-first exact-byte evidence as future work. | Make the latest implementation round and lane state canonical without collapsing local, hosted, release, deployment, or production proof. | The header names the twenty-second candidate; the twenty-first section records its exact digest and completed local gates; this evidence-aligned candidate is frozen and passes its own unchanged-byte gates before rereview. |

The hosted-lock choice preserves the existing authenticated production dashboard rather than inventing a new auth mode. Lock CLI commands still require an explicit server plus an owner-only credential source, so a bearer is never silently sent to the public election default. The OpenAPI and correspondence repairs are additive documentation/test changes to existing structured behavior.

Focused validation passed for the hosted OAuth binding, real stale-capability renew/resign handlers, exact OpenAPI response references and unsigned bounds, the skill contract, the site contract and behavior harness, Redocly lint, and HTML validation. The repaired pre-evidence candidate then passed `scripts/ci-local.sh` twice without byte changes at mode-and-content digest `fc939320181e6e41c038cad395f89de3a263d21625ab8fc334fdd629a3d76ca3` across 67 changed or new files on 2026-08-04: formatting and all-target checks; 197 library tests; 219 binary/API tests; 34 CLI/supervisor tests; 10 OpenAPI correspondence tests; 3 skill tests; benchmarks and warning-free Clippy; the strict 31-file package and publish dry run; release, rollback, crates.io resume, downgrade, site, installer, two-agent, and supervisor gates.

This evidence record changes the candidate bytes. The resulting evidence-aligned candidate must therefore pass the same complete gate twice unchanged before its final mode-and-content digest is frozen. That frozen digest and the reviewers' literal standalone responses are external acceptance evidence because inserting them here would alter the reviewed bytes. Fresh literal `[AGREE]` from Curie, Tesla, and Russell remains mandatory. Commit, push, replacement PR, hosted CI, merge, tag, release, deployment, and production acceptance remain separately pending.

### Twenty-third implementation review

The twenty-second evidence-aligned candidate passed two complete unchanged-byte local gates and was frozen at `ee64b252914ff995c217026ba3ba3224fe187da1364ce15cd8edd5aaa2a1858e`, but Curie, Tesla, and Russell each returned `REQUEST_CHANGES`. That digest, its green local gates, and every disposition against it are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| The armed watchdog exited when notifying a dead supervisor failed, before its independent KILL escalation, so a crashed supervisor could strand TERM-resistant protected work and descendants. | Supervisor notification is best effort; an already armed watchdog always continues through its own grace interval and independently kills the published worker group. | Deterministically fail replacement arming, SIGKILL only the paused supervisor, and prove the worker group, resistant descendant, and post-deadline side effect are gone. Keep the paused-supervisor regression green. |
| `authority_remaining_ms` could sit in the FIFO while the supervisor was paused and then start a fresh full watchdog, allowing stale initial authority to launch protected work after the original deadline. | Pair each authority budget with a numeric emission instant, subtract queue age before both initial and renewal watchdogs, and fail closed on missing, future, expired, oversized, or unusable budgets. | Pause after opening the event pipe but before consuming the initial event, queue authority plus a terminal event, wait past the emitted budget, resume, and prove neither the worker instruction nor a descendant starts. Exercise real CLI emission and every supervisor fixture with the versioned field. |
| Normative OpenSpec demo text, the README opening, and the crate description still implied self-host-only locks despite the accepted hosted OAuth path. | State one topology everywhere: account-free hosted elections; bearer-authenticated hosted locks through GitHub OAuth; the same authenticated locks self-hosted for private traffic or identity. | A semantic copy contract covers the OpenSpec, README, crate description, skill, homepage, docs, dashboard behavior, and forbidden self-host-only claims. |
| The README invoked `octostore election create` before installing the CLI on its primary clean-machine path. | Place the pinned, inspected, checksum-verifying CLI installation and version check before the first CLI command. | A deterministic ordering contract proves installer execution and `octostore --version` precede `octostore election create`; package and released-binary smoke remain green. |

Focused validation passed on 2026-08-04: Rust formatting; POSIX shell syntax and ShellCheck; the malformed freshness exact test; all 13 `reference_supervisor_` regressions; the separately named supervisor-crash containment regression; all 37 CLI end-to-end tests; all 3 skill tests; warning-free Clippy; strict package and release contracts; Redocly and HTML validation; both site harnesses; a fresh debug build; and a real supervisor smoke proving work starts only under authority and stops on uncertainty. The deterministic stale-FIFO regression proves that buffered authority cannot start either the worker instruction or its descendant, while real CLI tests prove positive integer `authority_remaining_ms` and `authority_observed_unix_ms` emission.

The repaired pre-evidence candidate then passed `scripts/ci-local.sh` twice without byte changes at canonical mode-and-content digest `38b99d6e246a92fe7a5604d00ebebf51b45d964ea271542fdf839a5c41e43377` across 67 changed or new files on 2026-08-04: formatting and all-target checks; 197 library tests; 219 binary/API tests; 37 CLI/supervisor tests; 10 OpenAPI correspondence tests; 3 skill tests; benchmarks and warning-free Clippy; the strict 31-file package and publish dry run; release, rollback, crates.io resume, downgrade, OpenAPI, HTML, site, installer, two-agent, and supervisor gates. Both release fixtures built and ran the candidate binary, verified checksums and reported version, observed two-agent election failover from term 1 to term 2, and stopped supervised work when authority became uncertain.

This evidence record changed the candidate bytes. The resulting evidence-aligned candidate passed the same complete local gate twice unchanged and canonically hashes to `324bcb1f404da1a929bc36686cf0a48444548f570a4099e4b608182e8b2b00b2`. It was mistakenly submitted to Curie, Tesla, and Russell under `e79ee5231ab044a719c07d779be7bcd5632144015633cf222dae50df1d91e8d2`, which used abbreviated modes and Git blob SHA-1 values instead of the established manifest's canonical modes and per-file SHA-256 values. All three reviewers independently returned `REQUEST_CHANGES` for that identity mismatch and performed no substantive source review. No reviewer disposition applies to those bytes.

### Twenty-fourth implementation review

Codex's required independent adversarial pass found a further authority-freshness gap before resubmission, and the three-reviewer provenance rejection exposed an evidence-reproducibility gap.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| `emit_authority` sampled monotonic `authority_remaining_ms` before wall-clock `authority_observed_unix_ms`. A scheduler pause between those calls could preserve the earlier, larger budget while publishing a later timestamp, hiding the pause from the supervisor's queue-age subtraction. | Sample the transferable wall clock first and the remaining monotonic budget second, so any pause between samples can only shorten the budget and increase the age deducted by the supervisor. | A deterministic clock-order unit regression proves wall-before-monotonic sampling and the shortened budget; real CLI JSONL and the stale-FIFO supervisor regression remain green. |
| The candidate was frozen with an undocumented noncanonical digest recipe, so independent reviewers could not reproduce its identity. | Use one explicit manifest: bytewise-sorted changed/new relative paths, canonical modes `100644`/`100755`/`120000`, per-file SHA-256 values, and one newline-delimited record per path; SHA-256 the complete manifest. | Two complete local gates bracketed by the same canonical digest and changed/new-file count; reviewers independently reproduce both before source review. |

Authority emission now obtains the wall-clock observation before the monotonic deadline sample through an injected clock-order helper. The test makes call reversal fail deterministically and proves a one-second delay shortens a three-second deadline to two seconds. The next candidate digest uses the canonical manifest recipe above.

Focused validation passed on 2026-08-04: Rust formatting; the deterministic clock-order test in both library and binary targets; all 37 CLI end-to-end tests, including real election/lock authority-event emission and the stale-FIFO supervisor regression; the semantic site contract; and warning-free Clippy.

The repaired pre-evidence candidate then passed `scripts/ci-local.sh` twice without byte changes at canonical mode-and-content digest `fd6e431180af330b097650bae8f2962eaa2d925b56e8319ffba92d14e94c8f27` across 67 changed or new files on 2026-08-04: formatting and all-target checks; 198 library tests; 220 binary/API tests; 37 CLI/supervisor tests; 10 OpenAPI correspondence tests; 3 skill tests; benchmarks and warning-free Clippy; the strict 31-file package and publish dry run; release, rollback, crates.io resume, downgrade, OpenAPI, HTML, site, installer, two-agent, and supervisor gates.

This evidence record changes the candidate bytes. The resulting evidence-aligned candidate must therefore pass the same complete local gate twice unchanged before its final canonical mode-and-content digest is frozen for rereview. That frozen digest and the reviewers' literal standalone responses remain external acceptance evidence because inserting them here would alter the reviewed bytes. Fresh literal `[AGREE]` from Curie, Tesla, and Russell remains mandatory. Commit, push, replacement PR, hosted CI, merge, tag, release, deployment, and production acceptance remain separately pending.

### Twenty-fifth implementation review

The twenty-fourth candidate passed two complete unchanged-byte local gates and was frozen across 67 changed or new files at canonical mode-and-content digest `f255ccc26fd64387a574cce2bc13af01ef00801e7aad2c1d0ade645813a6668b`. Curie, Tesla, and Russell each returned `REQUEST_CHANGES`; that digest, its green gates, and every approval claim against it are superseded.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| A detached, capability-bearing hold process group could survive supervisor `SIGKILL`, while only the worker group remained under the independent watchdog. | Publish and contain both detached process groups. Once armed, the watchdog must independently TERM then KILL the hold and worker groups after supervisor death. | Crash only the supervisor while TERM-resistant hold, worker, and descendants remain; prove neither group, descendant, nor post-deadline side effect survives. |
| Initial authority could age after the supervisor's first validation but before watchdog acknowledgement and worker release. | Recompute the adjusted authority budget inside the watchdog before readiness, then revalidate after its acknowledgement and before worker start. | Pause deterministically in the pre-monitor interval until the budget expires and prove neither the worker instruction nor descendant starts. |
| Civil-clock rollback could understate FIFO age and extend authority beyond the CLI's monotonic deadline. | Emit a same-host monotonic observation with every authority budget, run hold and supervisor on the same host and boot, subtract the greater wall-clock or monotonic age, and fail closed on missing, invalid, relayed, or persisted observations. | Regress both initial and renewal authority under partial wall-clock rollback, kill the supervisor, and prove the original monotonic deadline still contains hold, worker, descendants, and side effects. |
| The modified authentication fuzz target no longer compiled because it did not construct the database-backed authentication service, and neither local nor hosted gates compiled the fuzz package. | Build the fuzz service with an in-memory database, make token seeding checked, lock the fuzz dependency graph, and compile/format the fuzz package in local and hosted CI. | Root and fuzz formatting/checks pass with locked dependencies; the complete local and hosted gates execute the fuzz package checks. |

Authority events now include `authority_observed_continuous_ms`. The CLI samples wall time, then the transferable monotonic observation, then its local monotonic deadline budget so pauses can only shorten authority. The reference supervisor requires `jq` and Perl `Time::HiRes`, compares the greater civil and same-host monotonic age at every validation point, revalidates before and after watchdog acknowledgement, and gives the independent watchdog both protected process groups to terminate after supervisor death. The public README, both byte-identical skill artifacts, `/agents/`, semantic contracts, and this OpenSpec require same-host, same-boot direct supervision and reject cross-host or persisted event reuse. The fuzz package now constructs the database-backed authentication service and is included in both local and hosted checks.

Focused validation passed on 2026-08-04: root and fuzz formatting and locked all-target checks; shell syntax and ShellCheck; all 40 serialized CLI/supervisor end-to-end tests; the three deterministic reviewer-specific regressions after the public-contract update; all three skill-contract tests; both site contract harnesses; and focused HTML validation. Follow-up inspection found no hold, worker, watchdog, CLI fixture, or supervisor process left running.

The repaired pre-evidence candidate passed `scripts/ci-local.sh` twice without byte changes at canonical mode-and-content digest `511a3d25ad51a269a79d012e5e98e1695e263c6ad6663e8e3d8864a7be2d3a9b` across 70 changed or new files on 2026-08-04. Each complete pass covered root and fuzz formatting and locked all-target checks; 198 library tests; 220 binary/API tests; 40 serialized CLI/supervisor tests; 10 OpenAPI correspondence tests; 3 skill tests; benchmarks and warning-free Clippy; the strict 31-file package and warning-free publish dry run; ShellCheck; release, rollback, crates.io resume, real-v0.13.2 downgrade compatibility, OpenAPI, HTML, static-site, and browser-behavior contracts; and the released-binary checksum/install, two-agent failover, supervisor, and complete release fixture. Both passes exited zero at the same digest and file count, and follow-up inspection found no hold, worker, watchdog, server, CLI fixture, or supervisor process left running.

This evidence record changes the candidate bytes. The resulting evidence-aligned candidate must pass the same complete local gate twice unchanged before its final digest is frozen and resubmitted. Only that final frozen candidate may receive review. Fresh whole-candidate review and literal standalone `[AGREE]` from Curie, Tesla, and Russell remain mandatory. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, rollback exercise, and authenticated production acceptance remain separate pending lanes.

### Twenty-sixth implementation review

The twenty-fifth evidence-aligned candidate passed two complete pre-evidence gates and two complete evidence-aligned gates, then froze 70 changed or new files at canonical mode-and-content digest `583d00424055d073a2f255a14218b2c5779cec1814bd6092d35a65cb8a98358f`. Curie, Tesla, and Russell independently reproduced that digest and returned `REQUEST_CHANGES`. The frozen bytes, their green local gates, and any approval claim against them are superseded; repairs began only after rejection.

| Finding | Required disposition | Evidence required before rereview |
| --- | --- | --- |
| The authority watchdog could spend one full TERM grace on the worker and then a second full grace on the capability-bearing hold, extending teardown beyond the original authority budget. | At expiry or supervisor death, signal both protected groups concurrently, enforce one shared TERM deadline, then KILL and verify both within the existing reserve. Treat supervisor notification as advisory rather than the teardown mechanism. | Use TERM-resistant hold and worker descendants; prove both groups and every descendant disappear without a post-deadline side effect and within the original authority deadline. |
| A successful watchdog `USR1` sent while the supervisor was stopped could remain queued; killing the stopped supervisor before its handler ran could strand protected groups and watchdog state. | Poll supervisor liveness from the watchdog, keep direct group containment independent of signal handling, keep the watchdog alive while a stopped supervisor still exists, and let it remove state after supervisor death. | Stop the supervisor, prove `USR1` was successfully queued, kill only the supervisor, and prove hold, worker, resistant descendants, side effects, and temporary state are contained by the original deadline. |
| The detached worker wrapper was briefly unaddressable between process-group creation and parent-side PID publication. | Make the wrapper atomically publish and verify its own process-group PID before readiness or work release, and make the gated wrapper fail closed when its supervisor or authority disappears. | Kill the supervisor after child PID publication but before readiness/start; prove the wrapper group and state disappear and the worker instruction never executes. |
| Terminal cleanup could retire the final watchdog before both protected groups were proven contained. | Stop and verify worker and hold before retiring any active or retiring watchdog in terminal, signal, trap, and replacement-arm failure paths. Keep the watchdog armed through a deterministic post-containment pause. | Pause after protected-group containment but before monitor retirement, kill the supervisor, and prove no group, descendant, side effect, or temporary state is stranded. |
| A credentialed server could reflect the exact bearer into lock status/watch success fields or into error details, body/header request IDs, and unknown error codes, causing CLI stdout or stderr to disclose the credential. | Treat every authenticated response field as tainted: accept only finite error codes and strict lowercase `req_` request IDs, redact the active credential recursively from success JSON and human text, and redact every authenticated diagnostic. | A malicious loopback server reflects one exact credential through success metadata/status/ACL, error details, unknown code, and body/header request IDs; human status, JSON status, watch JSONL, stdout, and stderr remain credential-free. |

The watchdog now polls for supervisor death, signals hold and worker groups against one shared TERM grace, and remains independently responsible for final KILL and orphaned-state removal. The worker wrapper atomically publishes its own process-group PID before readiness and checks supervisor liveness while gated. Terminal and failure cleanup contain both protected groups before monitor retirement. Authenticated status, watch snapshots, and diagnostics recursively redact the active token; request IDs and server error codes are normalized before use.

Focused validation passed on 2026-08-04: Rust formatting; POSIX shell syntax; ShellCheck; all four reviewer-specific exact regressions; and the repository's documented serialized CLI/supervisor suite, 44 of 44 tests in 138.96 seconds. The malicious-server regression covers both human and JSON status, watch JSONL, error details, an unknown code, and body/header request-ID reflection. Follow-up inspection found no hold, worker, watchdog, server, CLI fixture, or supervisor process and no `octostore-supervisor.*` state directory left running. An exploratory parallel override passed 43 of 44 tests but overloaded the pre-existing 2.5-second tight-budget process fixture; both `scripts/ci-local.sh` and hosted CI explicitly set `RUST_TEST_THREADS=1` for deterministic signal/deadline fixtures, and the required serialized suite passed completely.

The repaired pre-evidence candidate then passed `scripts/ci-local.sh` twice without byte changes at canonical mode-and-content digest `592c40b9cc97e3701018dfa22b395fa4fa1d556eacbf60f8a986793743861b26` across 70 changed or new files on 2026-08-04. Each complete pass covered root and fuzz formatting and locked all-target checks; 198 library tests; 220 binary/API tests; 44 serialized CLI/supervisor tests; 10 OpenAPI correspondence tests; 3 skill tests; benchmarks and warning-free Clippy; the strict 31-file package and warning-free publish dry run; ShellCheck; release, rollback, crates.io resume, real-v0.13.2 downgrade compatibility, OpenAPI, HTML, static-site, and browser-behavior contracts; and the released-binary checksum/install, two-agent failover, supervisor, and complete release fixture. Both passes exited zero at the same digest and file count, and follow-up inspection found no hold, worker, watchdog, server, CLI fixture, supervisor process, or temporary supervisor state left running.

This evidence record changes the candidate bytes. The resulting evidence-aligned candidate must pass the same complete local gate twice unchanged before its final canonical manifest is frozen and resubmitted. Only that final frozen candidate may receive review. Fresh whole-candidate review and literal standalone `[AGREE]` from Curie, Tesla, and Russell remain mandatory before any staging or landing step. Commit, push, replacement PR, hosted CI, merge, tag, release assets, crates.io publication, deployment, rollback exercise, and authenticated production acceptance remain separate pending lanes.

### Twenty-seventh implementation review

The twenty-sixth repair still published supervisor lifecycle events before the worker launch or replacement authority deadline was acknowledged, and the acquisition request itself could outlive `--acquire-timeout` while stalled. The supervisor now emits `waiting` immediately, emits `leader` or `acquired` only after authority and worker-launch acknowledgement, and emits `renewed` only after the replacement deadline is independently armed. An optional `OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY=1` handshake lets an application touch `OCTOSTORE_SUPERVISOR_READY_FILE` after initialization before authority is published. The acquisition deadline now bounds the in-flight request as well as retry waits.

Focused lifecycle evidence passes: all 51 serialized CLI/supervisor end-to-end tests, including required worker readiness and acquisition-request timeout; ten consecutive zero-settle-delay two-agent smoke runs; POSIX shell syntax; and ShellCheck. No candidate server, supervisor, worker, or demo process remained after the runs.

The public surfaces now lead with “Stop two agents from doing the same work,” then skill, pinned CLI, and the exact supervised two-agent merge-coordinator demo before HTTP detail. The homepage instruction requires the reference supervisor or an equivalent fail-closed host. The demo contract creates one room once, exports it to Atlas and Comet, launches both supervisors concurrently with a 60-second acquisition timeout, proves waiting, cancels the leader, and proves takeover and worker cancellation. The source and published skills remain byte-identical.

The site now has a committed npm integrity graph and a real Playwright/Chromium gate using pinned Playwright `1.62.1` and axe-core `4.12.1`. Seven tests cover `/` and `/agents/` at `1440x1000` and `390x844`: first-viewport coordination proof, horizontal overflow, WCAG 2.0/2.1 A/AA axe scans, visible keyboard focus, keyboard copy, reduced motion, deterministic shared-room handoff, and runtime errors. The first run found that horizontally scrollable command blocks were not keyboard-focusable in Safari; those regions now have keyboard access, and the complete browser gate passes. Screenshots and checksums are retained under `evidence/browser/2026-08-04/`. The mocked shared-room screenshot is browser evidence, not hosted production proof.

Release-owned Node tools now execute only through `npm ci` and the committed lock; Redocly was advanced to `2.44.1` after the prior pin exposed a high-severity transitive PostCSS advisory, and `npm audit --audit-level=high --omit=optional` reports zero vulnerabilities. CI and release validation use the exact local gate, install the package-pinned browser, pin Node `24.11.1` and Rust `1.92.0`, pin every referenced action to an immutable commit, and declare minimal permissions per release job. The release contract rejects mutable action references, mutable `npx` execution in release-owned checks, unpinned toolchains, or a missing npm integrity graph. Workflow YAML parsing, deployment failure injection, rollback proof, crates.io resume fixtures, OpenAPI lint, HTML validation, static site contracts, and the seven browser tests pass.

The first attempted complete gate reached the strict crate-package check and then failed safely: Cargo's unanchored `README.md` include admitted the retained browser-evidence README and dependency READMEs from the ignored `node_modules` tree. No publication occurred. Every package include is now anchored to the repository root; the focused package gate again proves the exact 31-file allowlist, exact archive contents, successful archive compilation, and a warning-free publish dry run.

Webhook delivery remains HTTPS-only with redirects disabled. Documentation now states the implemented default-deny policy for localhost plus private, link-local, and reserved literal or DNS-resolved destinations on every delivery attempt. Only an intentionally private self-hosted network should set `OCTOSTORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS=true`; the environment example, README, human docs, OpenAPI, and static contract use that exact opt-in.

The repaired pre-evidence candidate then passed `scripts/ci-local.sh` twice without byte changes at canonical mode-and-content digest `0bfabc516f03f79694cb18125d05a8ee1242ab54f5f0b374a241179e8e4801cc` across 86 changed or new files on 2026-08-04. Each pass covered root and fuzz formatting and locked all-target checks; 201 library tests; 223 binary/API tests; 51 serialized CLI/supervisor tests; 10 OpenAPI correspondence tests; 3 skill tests; benchmark targets and warning-free Clippy; the strict 31-file crate package and warning-free publish dry run; immutable release-workflow, provenance, failure-injection, rollback, crates.io resume, and real-v0.13.2 downgrade/forward contracts; locked OpenAPI and HTML lint; static site and 7-test Chromium accessibility/behavior gates; pinned skill installation; and released-binary checksum/install, two-agent failover, supervisor cancellation, and full release-fixture smoke. Both passes exited zero at the same digest and file count.

This evidence changes candidate bytes. The complete `scripts/ci-local.sh` gate must pass twice without byte changes before the canonical manifest is frozen. Fresh literal `[AGREE]` from Curie, Tesla, and Russell and an independent Codex agreement are then required against only that digest. No commit, push, replacement PR, hosted CI, merge, tag, release, crates.io publication, deployment, or production acceptance has occurred.

### Twenty-eighth implementation review

The twenty-seventh candidate was frozen at canonical digest `04e59fdb909e647969f9c08468fdd2452ba67d9b8c335373af8367904032685a` after two complete local passes, but all three adversarial reviewers returned `REQUEST_CHANGES`. That digest, its local gates, and every review disposition are rejected and cannot authorize staging or release.

The accepted blockers covered four boundaries: supervisor containment after wrapper or guardian death, terminal events before verified containment, and cooperative process-group escape limits; authority cleanup and mutation-response validation; authentication, session, webhook, delay, dashboard, package, installer, and skill correctness; and native release provenance bound to the exact repository, release workflow, tag ref, source commit, and deployed asset digest. Each blocker now has a behavioral regression or explicit testable contract. Native binaries use GitHub attestations with self-hosted builders rejected; automated and manual deployment verify the attested Linux digest before host mutation and again after deployment. The installer checksum-verifies and atomically installs both `octostore` and `octostore-supervisor` from same-directory staging.

Supervisor repairs add a detached guardian monitor and main-process fallback, retain signalable process-group identities after wrapper-leader death, contain a direct worker `setsid` escape, and publish `released`, `lost`, `uncertain`, or `error` only after protected groups are verified contained. The documented portable boundary remains cooperative: descendant daemonization, session escape, or work handed to another service requires a platform sandbox such as a container or cgroup.

Focused validation on the repaired bytes passes Rust formatting; POSIX shell syntax; ShellCheck; changed-workflow Actionlint; the deterministic late-election response resignation test; webhook quota concurrency; supervisor smoke; the behavioral release/provenance/rollback/crates.io-resume contract; and all 55 serialized CLI/supervisor integration tests in 153.92 seconds. The suite was followed by a process audit; no server, hold, guardian, supervisor, or protected worker remained. Five stale temporary supervisor directories from killed-process tests were moved to Trash and verified absent.

These focused results are not a replacement for the complete gate. Public wording and evidence bytes changed after the rejected freeze. The final candidate must pass `scripts/ci-local.sh` twice without byte changes, receive a new canonical mode-and-content digest, and obtain fresh whole-candidate literal `[AGREE]` from three independent reviewers plus Codex before staging. Commit, push, replacement PR, exact-SHA hosted CI, merge, release, crates.io publication, native asset verification, deployment, rollback proof, and production canaries remain separate pending lanes.

### Twenty-ninth implementation review

Two independent pre-freeze audits of the twenty-eighth repair found four additional release blockers. The installer replaced `octostore-supervisor` and `octostore` with separate renames but did not restore the first target if the second rename failed. Manual redeploy accepted draft releases even though the automated release remains draft until crates.io verification succeeds. The canonical skill described one process group while the implementation tracks separate hold, worker, and direct-escape groups. Protected-lock quickstarts invoked a bare hold instead of the installed supervisor and, in the OpenSpec, omitted the mandated 60-second acquisition bound.

The installer now preserves same-directory backups of an existing executable pair, keeps rollback armed through both renames and post-install validation, and restores the previous pair on a failed or trapped commit. Its fixture permits the supervisor rename, fails the binary rename, observes the supervisor-backup restore, proves both old executables remain byte-for-byte, and proves no stage or backup file remains. Manual redeploy now requires GitHub's release object to report both `isDraft: false` and `isPrerelease: false` before resolving or downloading assets. The release contract statically requires this guard.

The canonical and hosted skills remain byte-identical and now describe cooperative hold and worker process groups. README, skill, homepage, agent guide, human docs, and the primary OpenSpec examples use `octostore-supervisor` for protected election and lock work; every quickstart retains `--acquire-timeout 60`. Bare hold commands remain documented only as the underlying CLI protocol or reference surface, not as the protected-work path.

Focused validation passes POSIX shell syntax, ShellCheck, changed-workflow Actionlint, the behavioral release/provenance contract, transactional release/install smoke, exact package/archive validation, three skill contracts, strict OpenAPI lint, static site behavior/CSP validation, HTML validation, and all seven Chromium accessibility/interaction tests. Targeted re-reviewers returned `[AGREE]` for these four dispositions. Those targeted responses are not whole-candidate approval and do not survive any subsequent edit or replace the required final freeze review.

The first attempted complete gate then failed in `locks::tests::acquire_rejects_lock_delay_above_documented_maximum`: the handler returned HTTP `400` with `invalid_input`, while this one stale assertion expected HTTP `422` with the unregistered code `validation_error`. The handler, centralized error mapping, CLI retry classifier, OpenAPI error enum and example, and this OpenSpec's error registry consistently define an out-of-range `lock_delay_seconds` value as HTTP `400` with `invalid_input`; HTTP `422` remains available for framework-level extraction failures. The assertion is aligned to that existing contract. The failed run is not a gate, and this evidence-aligned repair must pass the complete local gate twice on unchanged bytes before freeze.

For reproducible identity, the canonical manifest contains every tracked changed and untracked non-ignored path relative to `origin/main`, sorted by raw path bytes. Each record is exactly `<canonical-mode> TAB <lowercase-file-sha256> TAB <relative-path> LF`; canonical mode is `100644`, `100755`, or `120000`, and a symlink hashes its link-target bytes. The candidate digest is SHA-256 of the complete manifest bytes.

### Thirtieth implementation review

The twenty-ninth candidate was frozen across 86 manifest records at canonical digest `5abef4e627ce1864591872cf323936404ce3f77b8ec4a3175e29ca31c8d8ad6c`. Anscombe, Socrates, and Carver independently reproduced the same paths, modes, hashes, and digest, then all returned `REQUEST_CHANGES`. That digest, its prior local gates, and every targeted approval are rejected and cannot authorize staging, release, or deployment.

The accepted blockers are now permanent acceptance requirements for any replacement candidate:

1. document and contract-test the capability-free initial lock-watch snapshot and model the SSE response as a string wire frame;
2. reject webhook event names outside `acquired`, `released`, `renewed`, `expired`, and `*`, and bound delivery fan-out before spawning work;
3. persist identity provenance, keep stale local/static tokens inactive in OAuth mode, and require a matching OAuth identity for `ADMIN_USERNAME`;
4. couple every copyable protected-work quickstart, including generated dashboard copy, to an explicit worker and `octostore-supervisor` while retaining `--acquire-timeout 60`;
5. execute the skill installer only from the committed npm integrity graph;
6. accept only an exact stable SemVer tag at the first release gate and again immediately before production deployment;
7. scope private deployment-state permissions and prove safe served-site modes after both checkout and rollback; and
8. replace sequential HTTP deployment probes with the exact installer-produced CLI/supervisor pair and concurrent protected workers that prove cancellation, takeover, worker teardown, secret-free evidence, and process cleanup while rollback remains armed.

This section records requirements and rejection history, not successful repair. The replacement bytes require focused validation, two complete unchanged-byte local gates, a new manifest and digest, three fresh whole-candidate literal `[AGREE]` reviews, and an independent Codex agreement before any staging or landing action.

### Thirty-first implementation review

The repaired thirtieth candidate did not reach freeze. Its first complete local gate passed 217 library tests, 240 binary/API tests, all 55 serialized CLI/supervisor tests, 12 OpenAPI correspondence tests, 3 skill tests, benchmarks, package validation, release and rollback fixtures, downgrade compatibility, OpenAPI lint, HTML validation, and static-site contracts, then failed safely in the browser suite. The keyboard-copy assertion still expected the superseded mutable `skills@1.5.21` command after the public installer had moved to a tagged clone, locked `npm ci`, and the repository-local skills binary. That run is failed evidence and cannot authorize release.

Three independent whole-candidate reviewers also returned `REQUEST_CHANGES`. Every blocker is accepted:

| Finding | Required disposition | Current evidence state |
| --- | --- | --- |
| A partial scheduling pause after watchdog-delay calculation could re-anchor the old relative delay to a later clock sample and extend protected work beyond the emitted authority deadline. | Preserve the absolute continuous-clock deadline from the authority event, cap watchdog TERM/KILL deadlines to it, and reject publication or acknowledgement after that bound. | The reference supervisor now carries the original absolute deadline through arming and acknowledgement. A deterministic partial-pause regression proves fresh authority may still start work but TERM-resistant hold and worker processes cannot act past the original deadline. |
| The public installer trusted executable assets and `SHA256SUMS` from one release, while release immutability was disabled and publication occurred before immutable-release verification. | Require immutable GitHub release metadata and independent per-asset digests before executing either downloaded artifact; verify the repository policy before work, draft creation, and publication; remove a public mutable release if post-publication verification fails. | Repository immutable releases are enabled. The tag workflow has an encrypted administration-read policy credential, checks the setting at all three boundaries, publishes only from a private draft, verifies the resulting immutable release and assets, and has a tested cleanup path. The installer requires the immutable release object and agrees its per-asset digests with both downloaded bytes and `SHA256SUMS` before version or dependency execution. |
| Hosted deploy and manual redeploy resolved rollback authority from the live OpenAPI endpoint before SSH, so an interrupted deployment with production already down could not reach durable recovery. | Resolve the last successful production tag from GitHub deployment records, with the independently attested v0.13.2 bootstrap only when no durable record exists. | Both workflows derive the previous tag and attested binary digest from durable deployment/release state without requiring production availability, then record a successful production deployment only after the checked deploy returns. The behavioral contract covers the bootstrap and recorded-success paths. |
| The self-hosted monitor could append through a branch-controlled symlink. | Reject symlinked checkout, parent, or target paths; resolve every parent inside the checkout; rebuild the target from the committed blob through a private temporary regular file; verify mode before commit and push. | The monitor now fails closed on external, symlinked, non-regular, or untracked targets. A malicious monitoring branch pointing at an external sentinel is rejected without altering the sentinel; the regular-file path still commits and pushes. |
| The release fixture parsed but did not execute the installed supervisor, and the fresh-machine prerequisites omitted Bash and `seq` required by the flagship demo. | Pass the exact installed supervisor through both runtime smokes and list every demo dependency. | The release fixture runs the installed CLI/supervisor pair through two-agent waiting/takeover/cancellation and authority-loss cancellation. The agent guide names Bash and `seq` in addition to Git, Node/npm, POSIX shell, curl, jq, checksum tooling, Perl/`Time::HiRes`, and the platform clock. |
| The browser keyboard-copy assertion described a removed install path. | Assert the actual tagged clone, locked install, local binary, tagged skill source, explicit Codex target, and absence of `npx`. | All eight Chromium behavior/accessibility tests pass with the corrected assertion. |

Codex's independent pass also accepted the reviewers' non-blocking hardening suggestion: malformed JSON or unsupported event names in persisted webhook rows now fail startup rather than silently broadening delivery to wildcard. Both library and binary regression suites cover that restart boundary. The v0.14.0 changelog date is aligned to 2026-08-09.

Focused validation passes root and fuzz formatting; POSIX/Bash syntax; ShellCheck; Actionlint; the partial-pause supervisor regression; corrupt-webhook restart regressions; the immutable publication, durable recovery, symlink, provenance, rollback, and crates.io-resume contract; static site contracts; all eight Chromium tests; and the full installed-pair release fixture. Follow-up smoke output proves one shared room, one waiting candidate, takeover after cancellation, worker teardown on uncertainty, and no remaining supervisor processes.

These findings and evidence changed candidate bytes. The complete `scripts/ci-local.sh` gate must pass twice without byte changes before a new canonical manifest is frozen. Fresh literal `[AGREE]` from three independent whole-candidate reviewers and an independent Codex agreement are required against only that digest. Commit, push, replacement PR, exact-SHA hosted CI, merge, tag, immutable release, crates.io publication, deployment, rollback readiness, and live production acceptance remain separate pending lanes.

### Thirty-second implementation review

The thirty-first candidate subsequently passed two complete unchanged-byte local gates and was frozen across 92 manifest records at canonical digest `0ef54a983275b7480f2a0d05a355cfab2d83323d18a46eca33da588190f985a5`. Three independent whole-candidate reviewers reproduced that digest and all returned `REQUEST_CHANGES`. That digest, both gates, and every earlier focused or targeted approval are rejected and cannot authorize staging, release, or deployment.

All three blocking findings are accepted:

| Finding | Required disposition | Current focused evidence |
| --- | --- | --- |
| The supervisor could validate a numeric process group, lose every known member, and then signal the same reused PGID. | Keep a TERM-immune identity anchor in each protected group; bind it to PID, PGID, and process start time; revalidate that identity before every TERM or KILL; perform only one final KILL and never retry against a bare PGID. | The supervisor now uses dedicated in-group anchors and a deterministic stale PID/PGID regression proves an unrelated sentinel receives neither TERM nor KILL. The combined serial CLI/supervisor suite passes 57 of 57 tests in 182.54 seconds, with syntax, ShellCheck, formatting, and diff hygiene green. |
| Release jobs did not rebind the remote tag target to the immutable workflow event SHA at every irreversible boundary. | Check out `github.sha`, fetch and peel the remote tag, and require it to equal `GITHUB_SHA` before build, draft creation, crates.io publication, release finalization, and deployment. Bind the draft release target to the same SHA and prove a retargeted-tag fixture cannot reach publication. | Actionlint and the full behavioral release contract pass. The contract rejects a tag-commit mismatch before mutation or `cargo publish`, verifies the exact draft target, and preserves the existing immutable-publication, provenance, rollback, durable-recovery, and crates.io-resume checks. |
| The README's advertised clean-machine skill/install path omitted dependencies needed before the first command. | List and install Git, Node.js, npm, curl, jq, checksum tooling, Perl with `Time::HiRes`, Bash, `seq`, and the required suspend-inclusive platform clock for Debian/Ubuntu, Alpine, and macOS/Homebrew. Exercise the documented path with a clean `PATH`. | Static site/README contracts pass. The release fixture starts from a clean dependency path, verifies immutable installer metadata, installs the exact CLI/supervisor pair transactionally, runs the server, proves two-agent waiting/takeover/cancellation and uncertainty teardown, and leaves no supervisor process behind. |

These focused results apply only to the current unfrozen bytes. The OpenSpec evidence update itself changed the candidate after those checks. The complete `scripts/ci-local.sh` gate must therefore pass twice against unchanged bytes, with matching manifests before, between, and after the runs. Only a newly frozen manifest and digest may be sent to three fresh independent whole-candidate reviewers. Each reviewer must independently reproduce the digest and return literal `[AGREE]`; early agreement is pressed under the adversarial-spec rule. Codex must then perform and record an independent whole-candidate agreement before staging.

Commit, push, replacement PR, exact-SHA hosted CI, merge, reconciliation of stale PR #40, immutable `v0.14.0` release, crates.io checksum verification, deployment, rollback readiness, live site/API/skill/installer acceptance, installed two-agent canaries, process cleanup, and release-policy credential cleanup remain distinct pending lanes.

### Thirty-third implementation review

The first complete gate after the thirty-second repairs passed every repository-owned check, but the required process-table audit found dozens of detached hold/worker wrappers and TERM-resistant fixture children from an earlier failed E2E invocation. That run and its matching manifest are rejected. A passing top-level test result is not cleanup evidence, and no candidate may freeze while any process started by its tests remains alive.

The root cause was a lifecycle gap exposed by the new identity anchors. Detached wrappers can remain blocked in `wait` after an assertion or timeout removes the temporary state directory. The anchor previously exited when that directory disappeared, while the guardian could remove state even after containment failed. The harness then treated a missing state directory and top-level supervisor exit as success without proving the detached groups or fixture children were dead.

The replacement behavior is fail closed:

- unexpected state-directory removal makes the still-verified in-group anchor kill its own exact process group before exiting;
- guardian startup or containment failure retains identity, result, and recovery records instead of deleting them;
- identity-substitution and stale-PGID regressions retain the original hold/worker identities and fixture child PIDs, prove every protected process is gone, and separately prove the unrelated sentinel survives;
- a deterministic state-removal regression deletes the fixture state while TERM-resistant hold/worker descendants are live and proves both groups are contained; and
- all 58 process-heavy CLI/supervisor E2E tests use the pinned `serial_test` harness, so the README's plain `cargo test --locked` path is deterministic instead of depending on an ambient `RUST_TEST_THREADS` setting.

Focused evidence passes shell syntax, ShellCheck, formatting, and Rust test checking. `cargo test --test cli_e2e` passes 58 of 58 in 184.84 seconds; `RUST_TEST_THREADS=1 cargo test --test cli_e2e` passes 58 of 58 in 182.46 seconds. Both are followed by an independent process audit with zero remaining top-level supervisors, guardians, hold wrappers, worker wrappers, anchors, CLI test binaries, or recorded fixture processes. The expected `Terminated: 15` lines are deliberate signal-containment fixtures, not residual-process evidence.

These repairs and this evidence record changed candidate bytes. Both complete local gates, the canonical manifest, and every whole-candidate review must restart from a clean process baseline. The required manifest comparison now includes a zero-process audit before the first gate, between gates, and after the second gate.

### Thirty-fourth implementation review

The thirty-third candidate passed two complete unchanged-byte gates and was frozen across 92 records at canonical digest `65e04ab3e19865270f52af29be1008a3af52d77112bd396b683207f79751a3cf`. Galileo returned `[AGREE]`, but Curie and Heisenberg independently reproduced the same digest and returned `REQUEST_CHANGES`. Consensus was not reached; that freeze, both gates, and the single approval are rejected.

Both blockers are accepted:

| Finding | Required disposition | Current focused evidence |
| --- | --- | --- |
| A trapped HUP, INT, or TERM could roll back an in-progress executable-pair replacement, return to the interrupted installer, accept the restored old pair as merely executable, and print a false successful v0.14.0 installation. | A signal during any transactional phase must rollback and exit with the conventional nonzero signal status. Success requires exact requested CLI version plus exact CLI and supervisor digests after both renames and before rollback is disarmed. Exercise every supported signal after each irreversible rename. | The POSIX installer now exits 129, 130, or 143 from dedicated signal handlers and cannot resume into success output. The release fixture injects real HUP, INT, and TERM after the supervisor rename and after the CLI rename; all six cases restore both old files byte-for-byte, return the expected status, omit success claims, and leave no stage or backup file. The complete installed-pair fixture remains green. |
| The release and manual-redeploy jobs could access repository SSH credentials and mutate production without a GitHub `production` environment or required reviewer. Deployment API records created after SSH were audit metadata, not authorization. | Bind both SSH-capable jobs to `production`; fail closed before SSH unless the environment has an enabled nonempty required-reviewer rule; authenticate that inspection with administration-read authority; and migrate the three SSH credentials into environment-scoped secrets without deleting repository rollback copies until all environment names verify. | Both workflows bind to `production`. The workflow-only contract proves missing policy credential, absent environment, unprotected environment, missing secret value, set failure, incomplete verification, delete failure, and partial environment state all reject before SSH; complete migration and already-migrated states pass without exposing values. GitHub now has a live `production` environment with `aronchick` as required reviewer. The first approved deployment will copy `DEPLOY_HOST`, `DEPLOY_SSH_KEY`, and `DEPLOY_SSH_KNOWN_HOSTS` into that environment, verify all names, remove repository copies, verify their absence, and only then set up SSH. |

The live protection rule is external release configuration, not proof that a deployment occurred. The first production job must still receive explicit environment approval, emit the protection and migration evidence, pass the checked deployment, create a successful durable deployment record, and complete live acceptance. The broad temporary release-policy credential must be removed or replaced with a fine-grained administration-read credential after the release path no longer needs it.

These repairs and this evidence record changed candidate bytes. Both complete local gates, pre/mid/post manifests, zero-process audits, freeze, three fresh whole-candidate reviews, and independent Codex agreement must restart. No prior `[AGREE]`, gate, digest, or freeze may authorize staging.

### Thirty-fifth implementation review

The thirty-fourth candidate passed two complete unchanged-byte gates and was frozen across 92 records at canonical digest `4f4821e409757df680f6ce59e7d8059b9723541cfb17a30b03ba41572b2b7782`. All three independent reviewers reproduced that exact digest and returned `REQUEST_CHANGES`. Consensus was not reached; the freeze, both gates, and all prior approvals are rejected.

Every finding is accepted:

| Finding | Required disposition | Current focused evidence |
| --- | --- | --- |
| A `v*` tag push loaded workflow code from the tagged commit and exposed the repository policy credential before proving the tag was on trusted `main`. A tag creator could therefore execute modified release code and exfiltrate the credential before the later ancestry check. | Secret-bearing release execution must load only the default-branch workflow. Treat the requested stable tag as untrusted data, prove the event is a `repository_dispatch` on `refs/heads/main`, prove the tag resolves exactly to that trusted `GITHUB_SHA`, and only then reference any secret. | The release trigger is now `repository_dispatch` type `stable-release`; the first post-checkout step rejects any other event/ref, validates the exact stable tag, fetches `main` and the tag, requires exact SHA equality and ancestry, and emits a trust-boundary evidence event before the first secret reference. The behavioral fixture rejects wrong event, wrong ref, retargeted tag, and off-main ancestry before accepting the trusted case. |
| `OCTOSTORE_RELEASE_POLICY_TOKEN` was described as administration-read but was also used to create environment secrets and delete repository secrets. The write-capable credential crossed the production approval boundary. | Keep immutable-release/environment inspection on a distinct read-only policy credential. Use a separately named mutation-capable credential only inside reviewer-approved environment jobs, and prove migration code cannot reference the policy token. | SSH and crates.io secret migrations now use environment-scoped `OCTOSTORE_MIGRATION_TOKEN`; policy checks continue to use `OCTOSTORE_RELEASE_POLICY_TOKEN`. Static and behavioral contracts require the split, reject a missing mutation credential, preserve repository rollback copies until environment-name verification, and exercise partial/already-migrated states. The live `production` environment contains the migration credential by name. |
| The homepage called mutable `/agents/SKILL.md` a stable release artifact and made it the primary CTA while the CLI and supervisor were pinned to v0.14.0. | Point primary immutable messaging to the tagged release asset. Label the hosted skill as a current mutable copy when it is mentioned. | Homepage CTAs and README now point to `releases/download/v0.14.0/octostore-agent-skill.md`; README explicitly says the hosted copy follows the site and may change. The site contract rejects a homepage `/agents/SKILL.md` CTA and requires the immutable-versus-current distinction. |
| `cargo publish` and `gh release edit --draft=false` were reachable before any human-approved environment gate, so provenance checks did not provide release authority. | Bind both irreversible publication jobs to the reviewer-protected environment, keep the crates.io token there, and prove repository-to-environment migration is transactional before either publication path can run. | Both `publish` and `finalize`, as well as `deploy`, bind to `production`. Each publication job rechecks that required reviewers remain configured after approval. The crates.io credential migrates into `production`, verifies the environment secret name, deletes the repository copy only after verification, and supports safe resume. Structural contracts require all three bindings; behavioral fixtures reject absent credentials and failed set, verify, or delete operations. |

Focused validation passes Actionlint, ShellCheck, the site contract, and the full behavioral release contract, including trusted default-branch dispatch, immutable-publication cleanup, publication authorization structure, policy/migration credential separation, transactional crates.io and SSH migration, tag-retarget rejection, provenance, rollback, and public proof. These are local candidate checks, not hosted approval or production evidence.

These findings and their evidence record changed candidate bytes. The complete local gate must pass twice against unchanged bytes from a zero-process baseline. A new canonical manifest and freeze must then receive three fresh whole-candidate reviews and independent Sol agreement. No commit, push, merge, tag, crate publication, release, deployment, or live acceptance is authorized by the rejected `4f4821e4…` candidate.

### Thirty-sixth implementation review

The thirty-fifth candidate passed two complete unchanged-byte local gates and was frozen across 92 records at canonical digest `bcae7200a30f0939e9be2a17774024368f4285a92b5794e6d1d37d8024bc114d`. One Terra-high reviewer reproduced the manifest and archive, completed the anti-laziness verification, and returned `[AGREE]`; a second returned `REQUEST_CHANGES`, so consensus was not reached and that freeze is rejected. A third review later observed the intentional post-rejection workflow edits and correctly refused to approve stale bytes.

The valid authorization findings are accepted:

| Finding | Required disposition | Current focused evidence |
| --- | --- | --- |
| Branch-selected `workflow_dispatch` could execute altered manual-deploy workflow code, while the `production` environment had no deployment-branch policy. Human approval alone did not prove the approved workflow came from protected `main`. | Limit the environment to protected branches, require no custom branch policies, and fail closed unless the live environment API reports that policy before publication or SSH. | Live `production` now reports `protected_branches=true` and `custom_branch_policies=false`; `main` is the repository's only protected branch. Every publication/deploy preflight and its behavioral fixture now require that exact policy. An unprotected branch cannot enter the environment job or receive its secrets. |
| The sole required reviewer could approve their own dispatch because `prevent_self_review=false`. | Require `prevent_self_review=true` in provider configuration and every API preflight. A different eligible user or team must approve each irreversible publication/deploy job. | Workflow and behavioral contracts now reject self-review-enabled configurations. Live activation is pending an explicit reviewer identity: `aronchick` is currently the repository's only collaborator and the `octostore` organization has no teams, so enabling the rule before adding an eligible reviewer would make release impossible. |

The other review observations are dispositioned without weakening the gate:

- Missing v0.14.0 tag, release artifact, crate, production version, and live acceptance are expected pending completion lanes, not evidence that pre-merge candidate bytes are defective. They remain hard completion blockers and no release claim is made.
- A concurrent whole-review gate reported two 3-second fixture-startup timeouts while both tests passed individually. The main-agent gates passed 58/58 twice, and an additional isolated 58/58 suite passed in 179.94 seconds. Because host contention can still make a short readiness wait masquerade as a lifecycle failure, the two evidence-backed readiness waits are raised to 10 seconds while retaining immediate child-exit detection and every original authority/containment deadline.
- Manifest divergence reported after the first `REQUEST_CHANGES` is expected: the rejected freeze was intentionally edited. No stale digest or approval applies to the replacement candidate.

Focused validation passes Actionlint, ShellCheck, the complete behavioral release contract, and the isolated serialized 58-test CLI/supervisor suite. These repairs and this evidence record changed candidate bytes. After a non-self reviewer is configured, the complete local gate must pass twice on unchanged bytes, followed by a new freeze, three fresh explicit Terra-high whole-candidate reviews, and independent Sol agreement.

## Round 0: independent adversarial critique

This draft is not considered safe merely because its direction is appealing. The authoring pass identifies the following objections before an external review:

### Objection A — the “agent” framing can still create an orchestration promise

The phrase “agent collaboration” is attractive but inaccurate if the service only arbitrates authority. The two-agent demo must show the external work/branch/tool boundary and must not depict OctoStore assigning the second agent a task. The acceptance criteria and copy rules above make that a release blocker.

### Objection B — the proposed CLI could become a second product

`election`, `lock`, `watch`, and `hold` are already a meaningful surface. The revised MVP leaves one-shot mutations in HTTP, keeps capabilities in the hold process, and adds only create/status/watch around the lifecycle. Any additional alias must remove friction measured in the demo, not merely make the API look complete.

### Objection C — public election convenience can be mistaken for security

No login is a good first-run property, but the room identifier is a shared capability-like address and the leader token is a bearer capability. The site, skill, OpenAPI, and CLI must all repeat the distinction. Task ownership remains authenticated: the hosted path uses GitHub OAuth, while self-hosting is the private-traffic and private-identity boundary.

### Objection D — lease correctness is weaker than side-effect correctness

A process can pause, a response can be lost, and a downstream system may not honor a fencing term. Worse, a separate heartbeat process cannot stop an uncoupled worker. The hold process must stop after uncertainty, the reference supervisor must cancel or fence protected work, monotonic terms must persist, and the copy must say plainly that downstream idempotency/fencing is required. “No duplicate work” is a coordination objective, not a universal guarantee.

### Objection E — watchability can smuggle in a state-store roadmap

An SSE watch is enough to react to current state transitions, but an anonymous long-lived endpoint also creates resource-exhaustion risk. Durable event history, directory listings, arbitrary metadata, and a work queue are separate products. The draft limits watch events, requires reconciliation on reconnect, requires connection admission/cleanup, and removes structured labels from this goal.

### Objection F — the current contract is not clean enough to build a new client blindly

The local OpenAPI and implementation contain drift in defaults, required fields, and response descriptions. API contract alignment is therefore a prerequisite, not polish after the CLI. The release gate must fail on undocumented behavior.

### Objection G — “skill-first” can still be verbose

A skill is not a replacement for product focus if it becomes another manual. It should be short, normative, and runnable, with deep API explanation linked elsewhere. Its length and first-result time should be measured in a clean context.

### Objection H — independently created rooms defeat coordination

Hosted elections use random room IDs rather than a stable human name. If each agent follows “create an election” independently, both can become leader in different rooms. The first viewport, skill, CLI example, and harness must therefore create one room outside the candidates and pass the same ID to both.

### Objection I — a universal result model would add abstraction without leverage

The earlier draft attempted to normalize elections and locks into one lease response and mixed server outcomes with client-side `uncertain` state. That would create a second semantic layer and let the server appear to know transport facts it cannot know. The revised API keeps endpoint-specific success shapes, adds only missing retry/error/correlation fields, and defines `lost`/`uncertain` in the CLI.

## Adversarial review packet

The next review should ask independent reviewers to attack this document from distinct roles. Each reviewer must list concrete objections, affected requirements, and a proposed disposition; “looks good” is not sufficient in the first two rounds.

### Review roles

- Agent developer: Can an agent use the skill and CLI without guessing?
- Distributed-systems engineer: Are lease, expiry, fencing, retries, and uncertainty honest?
- Security engineer: Are public capabilities, tokens, metadata, logs, and skill content bounded?
- CLI/on-call engineer: Can a background process be supervised at 3 a.m. using exit codes and events?
- API maintainer: Is the change additive, contract-tested, and small enough to maintain?
- Product/marketing reviewer: Does the first viewport communicate an outcome without overpromising orchestration?
- Scope skeptic: Does any requirement introduce a queue, workflow, directory database, or SDK program?

### Required attack questions

1. What happens if the campaign succeeds but the response is lost?
2. What happens if renewal times out just before expiry?
3. Can two stale processes both enter the downstream critical section, and what prevents or limits that?
4. What exact mechanism stops or fences a worker when its separate hold process loses authority?
5. Can either candidate accidentally create a different election room, and how is shared bootstrap asserted?
6. Can a public room expose sensitive metadata or be used as a private task channel?
7. Can an agent tell “held,” “rate limited,” “server unavailable,” and “lost lease” apart without parsing prose?
8. Does pre-acquisition waiting busy-loop, hang forever by default, or reacquire after post-acquisition uncertainty?
9. Does the CLI preserve the server's election/lock semantics or create a new one?
10. Is election SSE worth a new unauthenticated long-lived connection surface versus client polling, and are its limits sufficient?
11. Could a visitor understand and try the product before seeing curl?
12. Does the demo prove coordination with the live primitive, or does it merely animate two fake agents?
13. Which proposed field, endpoint, command, or page can be removed without weakening the first result?
14. What is the exact supported topology, and where does the public copy say it?
15. Which claims are local-test evidence, hosted evidence, or only a design intention?

### Review disposition rules

- A valid safety or contract objection blocks the affected phase until addressed.
- A scope objection removes or defers a feature unless its user value is demonstrated by the two-agent path.
- A copy preference does not override a correctness boundary.
- A product choice that materially changes the user journey is recorded in the decision section; it is not silently guessed in code.
- Consensus is not declared from an early “agree.” The reviewer must confirm that it inspected the API, CLI, skill, demo, and release gates.

## Completion boundary

This OpenSpec is complete as a planning artifact when the adversarial review resolves the decisions above and the resulting goal has an owner, a dependency-ordered implementation plan, and measurable acceptance evidence. The product work is complete only after the implementation, local validation, merge, release, deployment, live content/API verification, and any requested production canary are separately evidenced.
