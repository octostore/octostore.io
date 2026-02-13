# 🐙 OctoStore

Distributed locking as a service. One binary, simple HTTP API, SQLite persistence.

> **Alpha software.** API may change. AI-assisted development. No SLA.

## What it does

OctoStore gives you distributed locks over HTTP with monotonically increasing
fencing tokens. No etcd, no ZooKeeper, no Consul — just a single Rust binary.

Sign up with GitHub → get a bearer token → start locking.

## Quick start

### Hosted version

```bash
# Sign in with GitHub (opens browser)
open https://api.octostore.io/auth/github

# Acquire a lock (60s TTL)
curl -X POST https://api.octostore.io/locks/my-service/acquire \
  -H "Authorization: Bearer YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds": 60}'

# Response:
# {"status":"acquired","lease_id":"...","fencing_token":1,"expires_at":"..."}
```

### Self-host

```bash
# Build from source
cargo build --release

# Configure (needs GitHub OAuth app credentials)
export GITHUB_CLIENT_ID=...
export GITHUB_CLIENT_SECRET=...

# Run
./target/release/octostore
```

## API

All lock endpoints require `Authorization: Bearer <token>`.

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/auth/github` | Start GitHub OAuth flow |
| `POST` | `/auth/token/rotate` | Rotate bearer token |
| `POST` | `/locks/{name}/acquire` | Acquire a lock |
| `POST` | `/locks/{name}/release` | Release a lock |
| `POST` | `/locks/{name}/renew` | Extend lock TTL |
| `GET` | `/locks/{name}` | Check lock status |
| `GET` | `/locks` | List your locks |
| `GET` | `/docs` | Interactive API docs |
| `GET` | `/health` | Health check |

### Acquire

```bash
curl -X POST https://api.octostore.io/locks/leader/acquire \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds": 60}'
```

**Lock acquired:**
```json
{"status": "acquired", "lease_id": "uuid", "fencing_token": 42, "expires_at": "..."}
```

**Already held:**
```json
{"status": "held", "holder_id": "other-uuid", "expires_at": "..."}
```

### Release / Renew

```bash
# Release
curl -X POST .../locks/leader/release \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"lease_id": "your-lease-uuid"}'

# Renew
curl -X POST .../locks/leader/renew \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"lease_id": "your-lease-uuid", "ttl_seconds": 60}'
```

## Fencing tokens

Every acquire returns a fencing token — a monotonically increasing integer.
Use it to guard writes against stale lock holders:

```sql
UPDATE state SET data = ?, fence = ? WHERE fence < ?
```

This is what makes the locking actually safe, unlike Redlock.

## Constraints

- **100 locks** per user
- **1 hour** max TTL (auto-expires)
- Lock names: `[a-zA-Z0-9.-]`, max 128 chars
- Metadata: max 1 KB per lock

## Architecture

- **Rust** — Axum + Tokio
- **DashMap** — concurrent in-memory lock storage
- **SQLite** — user accounts, fencing counter, lock persistence
- **GitHub OAuth** — authentication

Single process. Locks live in memory for speed, replayed from SQLite on restart.

## Testing & Quality

```bash
cargo test                           # Run all unit + integration tests  
cargo bench                          # Run criterion benchmarks
cargo tarpaulin --skip-clean         # Generate coverage report
cargo +nightly fuzz run fuzz_lock_name  # Run fuzz testing
```

### Code Coverage
![Coverage](https://img.shields.io/badge/coverage-pending-yellow)

Coverage analysis using `cargo-tarpaulin` to ensure comprehensive test coverage across all modules.

### Fuzz Testing
Three dedicated fuzz targets test input handling robustness:
- `fuzz_lock_name` — Lock name validation with arbitrary strings
- `fuzz_auth_header` — Authorization header parsing edge cases  
- `fuzz_json_body` — JSON request body parsing with malformed data

### Benchmarks
Comprehensive performance testing with Criterion.rs:
- Single-operation latency (acquire/release)
- Lock contention under load (2-10 threads)
- Database persistence overhead
- HTTP stack performance

See `BENCHMARKS.md` for detailed results and system specifications.

## License

MIT
