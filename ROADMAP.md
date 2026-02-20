# OctoStore Roadmap

Inspired by [Chubby: The Lock Service for Loosely-Coupled Distributed Systems](https://research.google/pubs/the-chubby-lock-service-for-loosely-coupled-distributed-systems/) (Burrows, 2006).

OctoStore is a single-binary distributed lock service: one process, HTTP API, SQLite persistence. The roadmap stays focused on correctness, observability, and durability — not clustering or SDK sprawl.

---

## Phase 1 — Reliable Primitives ✅
*Make the lock model correct under failure, not just under happy-path.*

| # | What | Status |
|---|------|--------|
| 1 | **Lock delay** — grace period before a dropped lock can be re-acquired | ✅ v0.6.0 |
| 2 | **Sessions + KeepAlive** — client heartbeats; all locks tied to a session | ✅ v0.5.0 |
| 3 | **Ephemeral locks** — auto-released when session expires | ✅ v0.6.0 |
| 4 | **Per-lock metadata** — attach a small payload on acquire | ✅ v0.3.0 |

---

## Phase 2 — Observability ✅
*Let clients react to changes instead of polling.*

| # | What | Status |
|---|------|--------|
| 5 | **SSE watch endpoint** — `GET /locks/{name}/watch` stream | ✅ v0.4.0 |
| 6 | **Webhooks** — POST callback on acquire/release/expire | ✅ v0.7.0 |
| 7 | **Lock namespace hierarchy** — slash-delimited paths + prefix listing | 🔄 v0.8.0 |

---

## Phase 3 — Durability
*Survive restarts without losing in-flight lock state.*

| # | What | Notes |
|---|------|-------|
| 23 | **WAL-based crash recovery** — SQLite WAL mode, full in-memory restore on startup | Single-node durability, no external deps |

---

## Phase 4 — Access Control
*Multi-tenant correctness.*

| # | What | Notes |
|---|------|-------|
| 13 | **Per-lock ACLs** — who can acquire, who can observe | Token scopes or explicit allow-lists |
| 14 | **Org/team namespacing** — partition the lock space by owner | Prevents noisy-neighbour problems |

---

## Non-goals
- **Client SDKs** — the HTTP API is the interface; curl works fine
- **Raft replication / HA clustering** — single-node with WAL durability covers the target use case
- **Fine-grained locking** — OctoStore is intentionally coarse-grained (Chubby §2.1)
- **Large file storage** — config values are capped at 256 KB. Use S3.
- **Mandatory locking** — advisory only, same reasoning as Chubby §2.4

---

## Current state (v0.7.0)
- ✅ Lock acquire / release / renew / status
- ✅ Fencing tokens (sequencers)
- ✅ TTL-based expiry with lock delay grace period
- ✅ Sessions + KeepAlive
- ✅ Ephemeral locks (auto-released on session expiry)
- ✅ Per-lock metadata (1KB payload)
- ✅ SSE watch stream per lock
- ✅ Webhooks with HMAC-SHA256 signing
- ✅ Rate limits, feature flags, config with history
- ✅ GitHub OAuth + static token auth
- ✅ OpenAPI spec + Swagger UI at `/docs`
- ✅ Public status page + admin dashboard
- ✅ Automated release → deploy pipeline (CI/CD to demo-host)
