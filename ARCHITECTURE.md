# OctoStore architecture

OctoStore is a single-node coordination plane for agent fleets and distributed workers. One Rust process serves an HTTP API, keeps active leases in memory, and persists authority changes to SQLite in WAL mode.

The design optimizes for an operator who wants one binary and one database file, not a new cluster.

## Product boundary

OctoStore owns coordination state:

- current task owner
- current fleet leader
- lease expiry and renewal
- monotonic fencing term
- agent sessions and ephemeral locks
- operator metadata, watches, webhooks, and metrics

It does not execute prompts, schedule DAGs, hold work queues, or make external side effects transactional. Agents keep their execution model. OctoStore decides who is currently authorized to act.

## Request flow

```text
Agent request
    |
    +-- public election route --> capability-based lease
    |
    +-- lock/session route ----> bearer-token authentication
                                  |
                                  v
                         shared LockStore
                         DashMap + SQLite WAL
                                  |
                                  +--> response with lease + term
                                  +--> SSE event / webhook
```

Public elections and authenticated locks use the same lease store. Election records live in a reserved `__election/` namespace that normal lock routes cannot read or mutate.

## Lease lifecycle

### Acquire

The store serializes contenders for one name through a DashMap entry. A winner receives:

- a random lease ID
- a strictly increasing fencing term
- an expiry timestamp

Before the HTTP handler reports success, OctoStore persists the next fencing counter and the complete lease row to SQLite. A database failure becomes a failed acquisition, never a successful response with memory-only authority.

### Renew

Renewal validates both the holder and lease ID. The new expiry is written to SQLite before the in-memory lease changes and before the response is returned.

### Release

Release validates the same holder and lease ID. The SQLite row is deleted before the in-memory entry is removed. A failed delete cannot produce a successful response that later resurrects the lease after restart.

### Expire

A background task scans active leases every five seconds. Expired leases are removed and an expiration event is broadcast. The request path also treats an expired entry as available, so correctness does not depend on the cleanup interval.

## Fencing terms

TTL handles liveness. Fencing handles stale authority.

A worker can pause during a network partition, lose its lease, then wake up believing it is still leader. Every new acquisition receives a higher term. A downstream resource that remembers the highest accepted term can reject late work from an older leader.

The `fencing_counter` SQLite row stores the next term. Updates use `MAX(counter, new_value)` so slower concurrent writes cannot move it backward. Startup takes the maximum of:

- the persisted next term
- the configured initial value
- one more than every restored live lease

Terms therefore remain monotonic even when every lease was released before restart.

## Account-free elections

`POST /elections` generates a 192-bit URL-safe room ID without creating server-side state. The first campaign creates the lease.

A winning campaign generates two random UUIDs: an internal holder ID and lease ID. Their 32 bytes are encoded as an opaque 256-bit `leader_token`. Possession of that capability is required to renew or resign. Internal IDs and the token are never returned to followers or public status calls.

Election properties:

- no account or API key
- public state to anyone who knows the room ID
- leader-only mutation through an unguessable bearer capability
- 5 to 300 second TTL
- configurable active-room capacity
- configurable per-client admission rate for room creation and campaigns
- operator kill switch through `PUBLIC_ELECTIONS=false`

Room IDs reduce accidental discovery; they are not authentication. Deployments that require a private coordination namespace should self-host behind network policy.

## Authenticated coordination

Private lock, session, and webhook routes use bearer tokens. Operators can seed static tokens from an environment variable or file. GitHub OAuth remains optional for human-facing deployments.

Per-user namespaces restrict scoped tokens to a lock-name prefix. The public election namespace is reserved and filtered from normal lock listing.

## Persistence

SQLite runs in WAL mode through one process-local connection protected by a mutex. All active lease fields are persisted:

- name
- holder and lease IDs
- fencing term
- expiry and acquisition timestamps
- metadata
- session relationship
- ephemeral flag
- lock-delay policy

On startup, unexpired rows are restored into DashMap. Expired rows are deleted. This model preserves in-flight authority across clean and unclean restarts without an external database service.

## Concurrency model

- Tokio handles asynchronous HTTP, timers, SSE, and webhooks.
- DashMap provides per-entry coordination, so unrelated names do not contend on one in-memory lock.
- SQLite remains the single durable writer and the ultimate single-node serialization boundary.
- Fencing allocation uses an atomic counter plus monotonic SQLite persistence.

The service is single-node by design. Running two OctoStore processes against separate SQLite files creates two authorities and is unsafe. High availability would require consensus or a shared serializable store and is outside the current contract.

## Failure behavior

| Failure | Result |
| --- | --- |
| Agent process crashes | Renewal stops; lease expires |
| Agent loses network | Lease expires unless connectivity returns before TTL |
| OctoStore restarts | Unexpired leases and next fencing term restore from SQLite |
| SQLite mutation fails | Authority-changing API call fails |
| Webhook fails | Lease remains valid; delivery failure is logged |
| Leader token leaks | Holder can be renewed or resigned; rotate by resigning or waiting for expiry |

## Scaling path

Start by scaling the single node vertically. Most coordination traffic is tiny JSON and short SQLite writes.

If one node becomes the bottleneck, the safe next steps are namespace sharding or replacing the persistence boundary with a consensus-backed store. Adding stateless replicas in front of independent SQLite files is not a scaling strategy; it is split brain with nicer packaging.

## Operational footprint

- one release binary
- one SQLite database plus WAL files
- standard HTTP health and status endpoints
- JSON metrics and admin endpoints
- optional static token file
- reverse proxy for TLS and network policy

That is the whole operational thesis: shared authority without adopting another distributed system to run the distributed system.
