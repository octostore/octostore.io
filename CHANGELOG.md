# Changelog

All notable changes to Octostore will be documented in this file.

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
