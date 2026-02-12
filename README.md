# 🐙 OctoStore

**Leader election and distributed locking as a service. That's it.**

No Kubernetes. No etcd cluster. No Consul agents. Just a single binary you can self-host, or use the free hosted version at [octostore.io](https://octostore.io).

Sign up with GitHub → get a bearer token → start locking in 30 seconds.

## Why?

Every existing solution for distributed locking requires you to run your own consensus cluster (etcd, ZooKeeper, Consul) or use a cloud vendor's proprietary service. OctoStore is:

- **One binary** — `./octostore-lock` and you're running
- **Simple HTTP API** — no client libraries needed, `curl` works fine
- **Fencing tokens** — actually safe distributed locking (not Redlock)
- **Self-hostable** — MIT licensed, zero external dependencies at runtime
- **Free hosted** — [api.octostore.io](https://api.octostore.io/docs) if you don't want to run your own

## Quick Start

### Use the hosted version

```bash
# Sign in with GitHub
open https://api.octostore.io/auth/github

# Acquire a lock
curl -X POST https://api.octostore.io/locks/my-service/acquire \
  -H "Authorization: Bearer YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds": 60}'

# Response:
# {"status":"acquired","lease_id":"...","fencing_token":1,"expires_at":"..."}
```

### Self-host

```bash
# Download
curl -fsSL https://octostore.io/install.sh | bash

# Or build from source
cargo build --release

# Configure
cp .env.example .env
# Edit .env with your GitHub OAuth credentials

# Run
./target/release/octostore-lock
```

## API

All lock endpoints require `Authorization: Bearer <token>`.

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/auth/github` | Start GitHub OAuth flow |
| `POST` | `/auth/token/rotate` | Rotate your bearer token |
| `POST` | `/locks/{name}/acquire` | Acquire a lock |
| `POST` | `/locks/{name}/release` | Release a lock |
| `POST` | `/locks/{name}/renew` | Extend a lock's TTL |
| `GET` | `/locks/{name}` | Check lock status |
| `GET` | `/locks` | List your active locks |
| `GET` | `/docs` | Interactive API docs (Scalar) |
| `GET` | `/openapi.yaml` | OpenAPI spec |

### Acquire a lock

```bash
curl -X POST https://api.octostore.io/locks/leader/acquire \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds": 60}'
```

**Won the lock:**
```json
{"status": "acquired", "lease_id": "uuid", "fencing_token": 42, "expires_at": "2026-02-12T01:00:00Z"}
```

**Someone else has it:**
```json
{"status": "held", "holder_id": "other-user-uuid", "expires_at": "2026-02-12T00:55:00Z"}
```

### Release / Renew

```bash
# Release
curl -X POST https://api.octostore.io/locks/leader/release \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"lease_id": "your-lease-uuid"}'

# Renew (extend TTL)
curl -X POST https://api.octostore.io/locks/leader/renew \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"lease_id": "your-lease-uuid", "ttl_seconds": 60}'
```

## Constraints

- **100 locks** per account
- **1 hour** max TTL (auto-expires — no zombie locks)
- Lock names: alphanumeric + hyphens + dots, max 128 chars
- **Fencing tokens**: monotonically increasing, safe for distributed coordination

## Fencing Tokens

Every acquire returns a fencing token — a monotonically increasing integer. Use it to prevent stale lock holders from making writes:

```python
# Safe write pattern
def write_with_fence(data, fencing_token):
    db.execute(
        "UPDATE state SET data=?, fence=? WHERE fence < ?",
        (data, fencing_token, fencing_token)
    )
```

This is the thing that makes OctoStore's locking actually *safe*, unlike Redis/Redlock.

## SDKs

Client libraries for all major languages:

| Language | Package |
|----------|---------|
| Python | [`sdks/python`](sdks/python) |
| Go | [`sdks/go`](sdks/go) |
| Rust | [`sdks/rust`](sdks/rust) |
| TypeScript | [`sdks/typescript`](sdks/typescript) |
| Java | [`sdks/java`](sdks/java) |
| C# | [`sdks/csharp`](sdks/csharp) |
| Ruby | [`sdks/ruby`](sdks/ruby) |
| PHP | [`sdks/php`](sdks/php) |

## Architecture

Under the hood:

- **Rust** (Axum + Tokio) — because lock services shouldn't have GC pauses
- **DashMap** — concurrent in-memory lock storage
- **SQLite** — user accounts + fencing token persistence
- **GitHub OAuth** — zero-friction signup

That's it. No Redis. No Raft. No consensus protocol. One process, one binary, one database file. If the server dies, all locks expire and clients re-acquire. Simple.

## Benchmark

```bash
cargo build --release
./target/release/octostore-bench \
  --token $TOKEN \
  --admin-key $KEY \
  --concurrency 100 \
  --duration 30
```

## Contributing

PRs welcome. This is a community project with no business model.

## License

MIT
