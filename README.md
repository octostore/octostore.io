<p align="center">
  <img src="site/assets/octostore-mascot.svg" alt="OctoStore's cartoon octopus referee juggling leader-election signals" width="340">
</p>

<h1 align="center">OctoStore</h1>

<p align="center"><strong>Pick one leader. Everyone else waits.</strong></p>

OctoStore is simple leader election over HTTP. Open a room, campaign from any process, and get one short-lived leader without creating an account, distributing an API key, installing an SDK, or operating a consensus cluster.

Use the hosted API for the smallest path to one leader. Run the single Rust binary inside your network when you also need durable task ownership, sessions, webhooks, and private coordination. Agent fleets are one use case, not a requirement.

> Alpha software. APIs may change before 1.0.

## What it coordinates

- **Leader election:** choose one current scheduler, controller, worker, dispatcher, or maintenance leader.
- **Task claims:** give every issue, ticket, queue item, deploy, or migration one temporary owner.
- **Crash recovery:** leases expire when a worker stops renewing.
- **Stale-writer defense:** monotonic fencing terms survive process restarts.
- **Fleet liveness:** sessions can own many ephemeral locks and release them together.
- **Operator visibility:** metadata, status endpoints, SSE watches, webhooks, and metrics use plain HTTP.

## Remote leader election, no signup

Create a collision-resistant room. No login, API key, SDK, or request body is required:

```bash
ROOM=$(curl -s -X POST https://api.octostore.io/elections \
  | jq -r .election_id)
```

Share the room ID with every candidate and campaign from any language that can send HTTP:

```bash
curl -s -X POST \
  "https://api.octostore.io/elections/$ROOM/campaign" \
  -H "Content-Type: application/json" \
  -d '{
    "candidate_id": "worker-a",
    "ttl_seconds": 30,
    "metadata": "fleet=support-agents"
  }'
```

Exactly one live candidate receives:

```json
{
  "status": "leader",
  "election_id": "...",
  "leader": {
    "candidate_id": "worker-a",
    "term": 1842,
    "expires_at": "..."
  },
  "leader_token": "...",
  "renew_after_ms": 15000
}
```

Followers receive the same leader plus `retry_after_ms`. The opaque `leader_token` is the bearer capability required to renew or resign the current term.

Public election leases are 5 to 300 seconds. Generated room IDs contain 192 bits of entropy. The API stores no user account.

## Self-host in one minute

The installer verifies the release checksum and binary version:

```bash
curl -fsSL \
  https://raw.githubusercontent.com/octostore/octostore.io/main/install.sh \
  | sh
```

Pin a specific release by setting the version on the installer process:

```bash
curl -fsSL \
  https://raw.githubusercontent.com/octostore/octostore.io/main/install.sh \
  | OCTOSTORE_VERSION=v0.13.0 sh
```

Start with a static token and one local SQLite file:

```bash
STATIC_TOKENS='ops:change-me' \
DATABASE_URL='./octostore.db' \
octostore

curl http://localhost:3000/health
```

OctoStore listens on `0.0.0.0:3000` by default. Put TLS and network policy in your existing reverse proxy.

## Claim a task

Use a deterministic lock name before an agent starts side effects:

```bash
curl -s -X POST http://localhost:3000/locks/github-issue-1842/acquire \
  -H 'Authorization: Bearer change-me' \
  -H 'Content-Type: application/json' \
  -d '{
    "ttl_seconds": 120,
    "metadata": "agent=atlas run=https://trace.example/7f2a"
  }'
```

An `acquired` response includes a `lease_id`, `fencing_token`, and `expires_at`. A `held` response means another worker owns the task. Renew around half the TTL and release on clean completion.

## API surface

| Primitive | Endpoints | Authentication |
| --- | --- | --- |
| Leader elections | `/elections`, `/elections/:id/*` | None; leader mutations use the returned capability |
| Locks | `/locks`, `/locks/:name/*` | Bearer token |
| Sessions | `/sessions`, `/sessions/:id/*` | Bearer token |
| Webhooks | `/webhooks`, `/webhooks/:id` | Bearer token |
| Health and status | `/health`, `/status` | None |
| Metrics and admin | `/metrics`, `/admin/*` | Admin credential |

Interactive API documentation is published at [https://api.octostore.io/docs](https://api.octostore.io/docs). The human guide lives at [https://octostore.io/docs/](https://octostore.io/docs/).

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `BIND_ADDR` | `0.0.0.0:3000` | HTTP listen address |
| `DATABASE_URL` | `octostore.db` | SQLite database path |
| `STATIC_TOKENS` | unset | Comma-separated `user:token` credentials |
| `STATIC_TOKENS_FILE` | unset | Newline-delimited static token file |
| `PUBLIC_ELECTIONS` | `true` | Enable account-free election endpoints |
| `MAX_PUBLIC_ELECTIONS` | `10000` | Maximum simultaneous public rooms |
| `PUBLIC_ELECTION_REQUESTS_PER_MINUTE` | `600` | Per-client admission limit for room creation and campaigns |
| `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET` | unset | Enable optional GitHub OAuth |
| `ADMIN_KEY` | unset | Protect metrics and admin endpoints |

Set `PUBLIC_ELECTIONS=false` when a private installation should expose only authenticated coordination.

## Safety model

- A lease proves time-bounded authority, not permanent ownership.
- Every successful acquisition reserves its fencing term in SQLite before returning it.
- Terms remain monotonic after all locks are released and the server restarts.
- Generated election room IDs are hard to guess, but they are not access controls.
- Leader tokens are bearer capabilities. Keep them out of URLs and logs.
- Public room creation and campaigns are rate limited per client; status, renewal, and resignation remain available during admission pressure.
- Locks are advisory. Downstream writes should be idempotent and reject stale fencing terms where possible.

## Development

```bash
./scripts/ci-local.sh
cargo build --release
```

Useful paths:

- `src/elections.rs` account-free leader election
- `src/locks.rs` authenticated HTTP lock handlers
- `src/store.rs` in-memory coordination plus SQLite durability
- `src/sessions.rs` agent liveness and ephemeral locks
- `openapi.yaml` API contract
- `site/` marketing site and guides

## License

MIT
