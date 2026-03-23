# 🐙 OctoStore

**Hosted coordination for agent swarms**

OctoStore gives distributed workers and agent swarms one owner per job, visible ownership metadata, and automatic recovery when a worker disappears.

If multiple agents can run the same skill against the same task, only one of them should do the work.

That is what OctoStore is for.

> **Alpha software.** API may change. AI-assisted development. No SLA.

## Why it exists

Once agents share reusable skills, runtime contention becomes inevitable.

Without a coordination layer, multi-agent systems tend to produce:
- duplicate work
- invisible ownership
- blind retries
- stuck jobs after worker crashes

OctoStore solves that with simple execution ownership.

## The model

The stack is straightforward:

- **Skills** provide capabilities
- **The marketplace** distributes those capabilities
- **OctoStore** coordinates execution

In practice, that means:
- claim a job
- heartbeat while working
- inspect the current owner
- release on completion
- recover automatically on failure

## Core properties

### One owner per job
Only one worker successfully claims a task.

### Visible ownership
Attach metadata so anyone can see who owns a job and what they are doing.

### Heartbeat while running
Workers keep claims alive while they are healthy.

### Automatic recovery
If a worker dies, its claim expires and the job becomes available again.

## Example

Five agents can all run the same GitHub issue skill on the same issue.
Only one should own the job.

With OctoStore:
1. each agent tries to claim the same stable job name
2. one agent wins
3. the others can inspect the owner metadata
4. if the winning worker crashes, the claim expires
5. another agent can take over

## Quick start

### Hosted version

```bash
# Sign in with GitHub (opens browser)
open https://api.octostore.io/auth/github

# Acquire a job claim / lock (60s TTL)
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
| `POST` | `/locks/{name}/acquire` | Acquire a lock / job claim |
| `POST` | `/locks/{name}/release` | Release a lock |
| `POST` | `/locks/{name}/renew` | Extend lock TTL |
| `GET` | `/locks/{name}` | Check current owner / lock status |
| `GET` | `/locks` | List your locks |
| `GET` | `/docs` | Interactive API docs |
| `GET` | `/health` | Health check + storage details |

## Fencing tokens

Every acquire returns a fencing token — a monotonically increasing integer.
Use it to guard writes against stale lock holders:

```sql
UPDATE state SET data = ?, fence = ? WHERE fence < ?
```

This is what makes the locking actually safe, unlike Redlock.

## Good fit

OctoStore works well for:
- agent swarms
- coding agents claiming issues or PR tasks
- CI/CD deploy serialization
- background workers on shared queues
- human-plus-agent workflows with inspectable ownership

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

See `BENCHMARKS.md` for detailed performance results and system specifications.

## License

MIT
