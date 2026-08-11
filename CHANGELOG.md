# Changelog

All notable changes to Octostore will be documented in this file.

## v0.14.4 - 2026-08-10

### Fixed

- Retry the complete isolated supervisor-containment proof when a transient loopback listener startup race occurs; each attempt must still prove worker gating and shutdown on authority loss.

## v0.14.3 - 2026-08-10

### Fixed

- Isolate release-build caches by pinned runner image and Rust target so a build script compiled against a newer glibc cannot be restored on the Ubuntu 22.04 Linux asset runner.

## v0.14.2 - 2026-08-10

### Fixed

- Retry a full isolated two-agent coordination fixture when its concurrent startup exits before proving authority; release validation still requires a complete successful run.

## v0.14.1 - 2026-08-10

### Fixed

- Make the supervisor containment integration test deterministic when a worker-group failure and a supervisor termination signal race; both proven-containment exit paths are now accepted.
- Publish the next immutable agent-skill and installer release after the failed `v0.14.0` candidate rather than moving its tag.

## v0.14.0 - 2026-08-09

OctoStore now gives agents a direct, supervised coordination path while keeping the HTTP API as the source of truth. Create one shared account-free hosted election for a coordinator or hold one authenticated hosted or self-hosted lock for an exact work item.

### Added

- `election create|hold|status|watch`, `lock hold|status|watch`, and explicit `serve` CLI commands without changing bare `octostore` server startup.
- Versioned JSONL lifecycle events with queue-age-adjustable authority budgets, bounded acquisition and renewal deadlines, exact signal exits, secret-safe token-file input, and fail-closed authority loss.
- A canonical installable `octostore` agent skill and a tested supervisor that gates and cancels protected workers.
- End-to-end two-agent failover, supervisor, stalled-I/O, shutdown-budget, session-deadline, leakage, namespace, watch-capacity, and release-install fixtures.
- Bounded best-effort election and lock watches with initial reconciliation, reconnect guidance, close-on-lag behavior, and capability-free lock events.

### Changed

- Rebuilt the homepage, agent guide, docs, election guide, lock guide, and README around “stop two agents from doing the same work,” with skill and CLI before curl.
- Standardized public API error codes, request IDs, retry guidance, and OpenAPI route/error correspondence.
- Scoped namespaced reads and session status to their owners; aligned ephemeral session-lock expiry across runtime cleanup and restart.
- Hardened release publication around an exact tagged commit on `origin/main`, local-equivalent checks, portable Rustls/MUSL assets, native execution, a draft release, and deployment only after publication.

### Security

- Lock watch events no longer expose lease IDs, session IDs, holder metadata, or other mutation capabilities.
- Token files are opened once without symlink following and validated from the opened descriptor for owner, type, and mode.
- Lease and session authority fail closed on the earlier monotonic confirmation deadline, including stalled response bodies and long lock TTLs.
- Watch-channel storage is bounded and pruned, and lag or serialization failure closes the stream so clients reconcile.
- Local registration is disabled by default, restricted to an explicit numeric loopback bind when enabled, incompatible with other identity sources, and rejects case-insensitive local/static/OAuth username collisions without returning an existing bearer token; ambiguous legacy identities fail startup.
- GitHub OAuth handoffs now bind the configured dashboard, CORS origin, one-time exchange, persisted credential, and subsequent browser API requests to the issuing deployment; unsafe or incomplete self-hosted authorities fail startup.
- Buffered authority events cannot restart a stale safety budget, and a detached guardian plus main-process fallback contain protected worker groups even if the supervisor or guardian dies; terminal lifecycle events are withheld until containment is verified.
- The portable supervisor documents its cooperative process-group boundary. A direct worker `setsid` escape is detected and contained, but untrusted or daemonizing descendants require a platform boundary such as a container or cgroup.
- Crate publication uses a strict file allowlist and exact archive inspection, and resumable crates.io publication accepts an existing version only when its unyanked checksum matches the exact candidate archive.

## v0.13.2 - 2026-07-18

### Changed

- Reframe the homepage as an introduction to OctoStore before presenting individual capabilities.
- Put the totally free hosted service and the open-source self-hosted server on equal footing in the hero.
- Move leader-election proof and detailed value propositions below the product introduction.

## v0.13.1 - 2026-07-18

### Fixed

- Restore authenticated lock acquisition for static-token and local users without a namespace. Namespace checks now treat a SQL `NULL` as the intended unrestricted namespace instead of returning `500 Internal Server Error`.

## v0.13.0 - 2026-07-18

OctoStore now leads with its smallest useful promise: elect one leader from any process in two HTTP calls, without an account, API key, SDK, or cluster to operate. Agent fleets and self-hosted task coordination remain dedicated paths rather than prerequisites.

### Added

- A dedicated 60-second leader-election guide for generic workers, scheduled jobs, controllers, migrations, and agent dispatchers.
- A dedicated task-coordination guide for named locks, renewal, expiry, sessions, watches, and webhooks.
- A failure-focused leader-election essay covering crash recovery, retry timing, stale leaders, leases, capabilities, and fencing terms.
- Configurable per-client admission limiting for public room creation and campaigns through `PUBLIC_ELECTION_REQUESTS_PER_MINUTE`.
- `429 Too Many Requests` responses with `Retry-After` for clients that exceed the public-election admission budget.

### Changed

- Rebuilt the homepage around “Pick one leader. Everyone else waits.” and a process-neutral production election.
- Reworked the agent page around duplicate side effects, task ownership, and the boundary between coordination and execution.
- Reframed package metadata, README, OpenAPI, docs, architecture, roadmap, social metadata, and navigation around simple leader election first.
- Keep status, renewal, and resignation outside admission limiting so load cannot strand an existing leader.

### Security

- Bound in-memory rate-limit tracking and collapse excess unrecognized clients into a shared overflow budget.
- Trust Cloudflare's client IP header only through a loopback reverse proxy and fall back to the direct socket address elsewhere.

## v0.12.0 - 2026-07-18

OctoStore now coordinates agent fleets directly: self-host the complete lease service, or use account-free remote leader election when distributed candidates need one current leader immediately.

### Added

- Public `POST /elections` room creation with 192-bit opaque room IDs and no account, API key, or request body.
- Campaign, status, renew, and resign endpoints for capability-based remote leader election.
- Monotonic election terms, follower retry timing, leader renewal guidance, and operator-readable candidate metadata.
- Native Linux ARM64 release binaries alongside Linux AMD64 and macOS builds.
- `PUBLIC_ELECTIONS` and `MAX_PUBLIC_ELECTIONS` controls for self-hosted operators.
- A production-backed three-agent race on the homepage that demonstrates the anonymous election API live.
- A complete agent orchestration guide, self-host guide, election API documentation, and the launch essay “Agents are cheap. Collisions are expensive.”
- SHA-256 checksum publication and verification for release binaries.

### Changed

- Reframed OctoStore as an open coordination plane for agent fleets while preserving the focused HTTP lease model.
- Persist fencing-term allocation before returning successful authority changes.
- Persist renewals and releases before changing in-memory state or reporting success.
- Preserve the next fencing term even when every lock is released before restart.
- Reserve the `__election/` namespace from authenticated lock routes and listings.
- Replace the duplicate automatic deployment workflow with an explicit manual redeploy workflow for existing stable tags.
- Update the package metadata, architecture, roadmap, OpenAPI contract, README, and environment template for v0.12.

### Fixed

- Fix `install.sh` to install the `octostore` server binary instead of looking for the unpublished `octostore-test` asset.
- Fix the reported next fencing token for vacant locks.
- Include election traffic in endpoint metrics.

## v0.11.0 - 2026-05-31

This release frames Octostore's public site and documentation around distributed locking over HTTP, with clearer guidance for hosted agents, developers, and release automation.

### Changed

- Reframed the website around distributed locking over HTTP.
- Added an agents use-case page for hosted agent coordination.
- Added a blog post explaining distributed locking for hosted agents.
- Refreshed the README and getting-started documentation with current lock API examples.
- Hardened the release workflow after v0.10.2 by verifying release tags match `Cargo.toml` and publishing from a clean tree.

## v0.10.2 - 2026-05-31

### Fixed

- Corrected release versioning after v0.10.1 built binaries reported `0.10.0`.
- Updated the release workflow to verify the tag matches `Cargo.toml` before building and publishing, and to publish from a clean tree.

## v0.10.1 - 2026-05-31

This release updates the public-facing site and docs to describe Octostore as distributed locking over HTTP, with clearer paths for hosted agents and developers getting started with the lock API.

### Changed

- Repositioned the website around distributed locking over HTTP.
- Added a GitHub link on the homepage for easier access to the source repository.
- Added a new agents use-case page for hosted agent coordination.
- Added a new blog post about distributed locking for hosted agents.
- Refreshed the README and getting-started material with accurate lock API examples.
- Refreshed the docs landing page to better introduce the current product direction.
