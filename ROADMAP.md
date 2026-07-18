# OctoStore roadmap

OctoStore is becoming the smallest useful coordination plane for agent fleets: self-hosted when the control boundary matters, instantly hosted when several remote processes just need one leader.

## Shipped

### Coordination core

- lock acquire, renew, release, and status
- TTL expiry and optional lock-delay grace periods
- monotonic fencing terms
- SQLite WAL persistence and restart recovery
- hierarchical lock namespaces
- per-lock metadata

### Agent operations

- sessions and keepalive
- ephemeral locks tied to session liveness
- SSE lock watches
- signed webhooks
- status, admin, metrics, and time-series endpoints

### Access

- static token and token-file authentication
- optional GitHub OAuth
- per-user namespace scopes
- account-free, capability-based remote leader election in v0.12

## Next

### Fleet ergonomics

- first-party shell and TypeScript helpers that remain thin wrappers over HTTP
- jittered campaign and renewal recipes
- structured election metadata and fleet dashboards
- event streams for public election changes

### Multi-tenant hardening

- per-lock ACLs
- explicit organization and team namespaces
- operator-configurable request and room quotas
- hashed-at-rest static bearer tokens

### Durability and operations

- online SQLite backup command
- startup integrity report and migration visibility
- webhook retry policy and dead-letter inspection
- exportable OpenTelemetry traces

## Non-goals

- executing prompts or tools
- owning a work queue or DAG language
- mandatory language-specific SDKs
- pretending independent SQLite replicas form a cluster
- large value storage
- mandatory locking of external resources

High availability requires a real consensus or serializable storage boundary. Until OctoStore implements one, the safe topology is one authority per namespace.
