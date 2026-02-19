# OctoStore Roadmap

Inspired by [Chubby: The Lock Service for Loosely-Coupled Distributed Systems](https://research.google/pubs/the-chubby-lock-service-for-loosely-coupled-distributed-systems/) (Burrows, 2006).

OctoStore is already a usable lock + config + rate-limit service. This roadmap closes the gap between "it works" and "you'd trust it for leader election in production."

---

## Phase 1 — Reliable Primitives
*Make the lock model correct under failure, not just under happy-path.*

| # | What | Why (from Chubby) |
|---|------|-------------------|
| 1 | **Lock delay** — grace period before a dropped lock can be re-acquired | Protects unmodified servers from delayed/reordered messages after holder failure |
| 2 | **Sessions + KeepAlive** — client heartbeats; all locks tied to a session | Locks survive short network hiccups; expire cleanly when client dies |
| 3 | **Ephemeral locks** — auto-released when session expires | Removes the need for explicit cleanup; signals liveness |
| 4 | **Per-lock metadata** — attach a small payload on acquire | Primary advertises its address in the lock itself (GFS/Bigtable pattern) |

---

## Phase 2 — Observability
*Let clients react to changes instead of polling.*

| # | What | Why (from Chubby) |
|---|------|-------------------|
| 5 | **SSE watch endpoint** — `GET /locks/{name}/events` stream | Eliminates polling; clients get notified the moment a lock changes hands |
| 6 | **Webhooks** — POST callback on acquire/release/expire | Same value for services that can't hold a persistent connection |
| 7 | **Lock namespace hierarchy** — `/service/component/lock` paths | List and watch whole subtrees; mirrors how GFS and Bigtable use Chubby |

---

## Phase 3 — Client Libraries
*Chubby's client library is half the system. OctoStore needs the same.*

| # | What | Notes |
|---|------|-------|
| 8 | **Go SDK** — sessions, acquire/release, KeepAlive, watch | First-class; used internally for dogfooding |
| 9 | **TypeScript/Node SDK** | Covers the web backend crowd |
| 10 | **Python SDK** | Covers ML/data pipelines |

Each SDK should handle KeepAlive automatically, retry transient errors, and expose an `OnLockLost` callback.

---

## Phase 4 — Scale & HA
*Single-server is fine until it isn't.*

| # | What | Why (from Chubby) |
|---|------|-------------------|
| 11 | **Embedded Raft replication** — 3- or 5-node cluster mode | Chubby runs 5 replicas; single-master is the only real SPoF |
| 12 | **Read-replica / proxy mode** — aggregate KeepAlives, serve cached reads | Chubby's biggest scaling win; reduces master load by N× |

---

## Phase 5 — Access Control
*Multi-tenant correctness.*

| # | What | Notes |
|---|------|-------|
| 13 | **Per-lock ACLs** — who can acquire, who can observe | Chubby uses ACL files; we can use token scopes or explicit allow-lists |
| 14 | **Org/team namespacing** — partition the lock space by owner | Prevents noisy-neighbour problems on shared deployments |

---

## Non-goals (for now)
- **Fine-grained locking** — OctoStore is intentionally coarse-grained (Chubby §2.1). If you need sub-second locks, run your own in-process lock server.
- **Large file storage** — config values are capped at 256 KB. Use S3.
- **Mandatory locking** — advisory only, same reasoning as Chubby §2.4.

---

## Current state (v0.2.x)
- ✅ Lock acquire / release / renew / status
- ✅ Fencing tokens (sequencers)
- ✅ TTL-based expiry
- ✅ Rate limits, feature flags, config with history
- ✅ GitHub OAuth + static token auth
- ✅ OpenAPI spec + Swagger UI at `/docs`
- ✅ Public status page + admin dashboard
- ✅ Automated release → deploy pipeline
