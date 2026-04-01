# 🐙 OctoStore

**Self-hostable coordination over HTTP**

OctoStore is a self-hostable distributed lock and lease service over HTTP.
Use it locally or across many machines to coordinate work with visible ownership and automatic recovery.

> **Alpha software.** API may change. AI-assisted development. No SLA.

## What it is

OctoStore is a narrow primitive:
- claim a stable job name
- renew while the owner is alive
- release when done
- let ownership expire automatically on failure

That makes it useful for:
- local scripts and cron jobs
- background workers
- CI/CD deploy serialization
- multi-machine services
- agent runtimes

## Why it exists

Without a shared lock or lease layer, distributed systems tend to drift into the same failures:
- duplicate work
- invisible ownership
- blind retries
- stuck work after crashes
- ad hoc coordination logic spread across many services

OctoStore gives you one clean coordination primitive instead.

## Local and multi-machine coordination

### Local coordination
Run OctoStore on one machine and let local processes coordinate through it.

Good for:
- several workers on one box
- cron dedup
- local agents
- dev and demo environments

### Multi-machine coordination
Run one shared OctoStore deployment and let workers across many servers coordinate through the same API.

Good for:
- shared job processing
- deploy locks
- cross-machine cron dedup
- fleet-wide visibility into what is running where

## Coordination pattern

The practical pattern is simple:
1. choose one stable name for one logical unit of work
2. have all workers try to claim the same name
3. attach metadata about the owner
4. renew while active
5. release on completion
6. let TTL expiry recover from crashes

Recommended metadata includes:
- `owner`
- `host`
- `service`
- `task_id`
- `job`
- `claimed_at`
- `run_id`
- `run_url`

## Example

Five workers can all see `github/issues/123`.
Only one should do the work.

With OctoStore:
1. each worker tries to claim the same stable job name
2. one worker wins
3. the others inspect the owner metadata and back off
4. the winner renews while working
5. if the winner crashes, the claim expires
6. another worker can take over

## Quick start

### Hosted version

```bash
# Sign in with GitHub (opens browser)
open https://api.octostore.io/auth/github

# Acquire a lock / claim (60s TTL)
curl -X POST https://api.octostore.io/locks/my-service/acquire \
  -H "Authorization: Bearer YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds": 60}'
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
- local coordination on one machine
- multi-machine workers sharing jobs
- deploy serialization
- background workers on shared queues
- agent systems that need one owner per task

## What it is not

OctoStore is not trying to be:
- a workflow engine
- a queue
- a scheduler
- a giant orchestration control plane

It is the coordination primitive underneath those higher-level patterns.

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
cargo test
cargo bench
cargo tarpaulin --skip-clean
cargo +nightly fuzz run fuzz_lock_name
```

See `BENCHMARKS.md` for detailed performance results and system specifications.

## Release notes

- `v0.10.0` — coordination-first positioning, broader docs set, docs-first navigation
- `v0.9.x` — namespace safety, WAL-backed recovery, remaining namespace test cleanup

## License

MIT
