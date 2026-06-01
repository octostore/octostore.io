# OctoStore

Distributed locks over HTTP.

OctoStore is a small coordination service for shared work. Use it when several processes, jobs, or hosted workers can all see the same task but only one should own it at a time.

The core loop is simple:

1. acquire a named lock with a TTL
2. do the protected work if you acquired it
3. renew the lease while work is still running
4. release the lock on completion
5. rely on expiry if the worker disappears

> Alpha software. APIs and deployment details may change.

## What it is good for

- deploy serialization
- singleton cron jobs
- queue or backlog workers that need a visible owner
- hosted agents claiming issues, tickets, or evaluation tasks
- cache rebuilds and migrations that should not overlap
- simple cross-language coordination from scripts and services

OctoStore is not a scheduler, workflow engine, queue, or DAG system. It is a focused lease service.

## API model

Lock names should be single path segments such as `deploy-main`, `issue-1842`, or `customer-sync-72`.

Acquire a lock:

```http
POST /locks/:name/acquire
```

Request body:

```json
{
  "ttl_seconds": 60,
  "metadata": "runner=host-7 job=https://ci.example/runs/1842"
}
```

`metadata` is optional and is intended for operator-visible context. Keep it small.

Acquire responses use two primary statuses:

- `acquired` — you own the lease and may proceed
- `held` — another holder owns the lock; back off, retry later, or inspect status

Renew a lock:

```http
POST /locks/:name/renew
```

Request body:

```json
{
  "lease_id": "...",
  "ttl_seconds": 60
}
```

Renew returns:

```json
{
  "lease_id": "...",
  "expires_at": "..."
}
```

Release a lock:

```http
POST /locks/:name/release
```

Request body:

```json
{
  "lease_id": "..."
}
```

Release returns:

```json
null
```

## Getting started

### 1. Run locally with a static token

```bash
cargo run
```

For local development, configure a static bearer token:

```bash
export STATIC_TOKENS='alice:devtoken'
cargo run
```

Then use:

```bash
export OCTOSTORE_TOKEN='devtoken'
```

### 2. Acquire a lock

```bash
curl -X POST http://localhost:3000/locks/deploy-main/acquire \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds":60,"metadata":"local smoke test"}'
```

If the response contains `"status":"acquired"`, this process owns the lease until `expires_at`.

If the response contains `"status":"held"`, another holder owns the lock. Do not run the protected work.

### 3. Renew while work continues

```bash
curl -X POST http://localhost:3000/locks/deploy-main/renew \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"lease_id":"YOUR_LEASE_ID","ttl_seconds":60}'
```

The response contains the same `lease_id` and the new `expires_at` timestamp.

### 4. Release when done

```bash
curl -X POST http://localhost:3000/locks/deploy-main/release \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"lease_id":"YOUR_LEASE_ID"}'
```

A successful release returns `null`.

### 5. Inspect lock status

```bash
curl http://localhost:3000/locks/deploy-main \
  -H "Authorization: Bearer ***"
```

Use status and metadata to understand who owns the work and when the lease expires.

## Hosted API

The hosted API is available at:

```text
https://api.octostore.io
```

Interactive docs are published at:

```text
https://octostore.io/docs/
```

The website and docs source live at:

```text
https://github.com/octostore/octostore.io
```

## Development

```bash
cargo test
cargo build --release
```

Useful files:

- `src/models.rs` — request and response types
- `src/store.rs` — lock storage and lease logic
- `openapi.yaml` — API reference source
- `site/` — static website

## License

MIT
