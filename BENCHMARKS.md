# OctoStore benchmarks

## Evidence status

The Criterion benchmark suite compiles and exercises the in-process `LockStore` API. It does not currently contain an HTTP benchmark, a hosted-service benchmark, or evidence for a latency service-level objective.

No latency or throughput number is a public OctoStore claim until it is recorded from a named commit with the machine, database, concurrency, workload, sample size, and command used to produce it. Architecture alone is not performance evidence.

## Included benchmarks

`benches/lock_benchmarks.rs` currently contains:

- `acquire_lock`
- `release_lock`
- `acquire_release_cycle`
- `contention_2_threads`
- `contention_10_attempts`
- `many_different_locks/1000`
- `fencing_token_generation/acquire_generates_token`
- `sqlite_persistence`

These are direct store and SQLite measurements. They do not include Axum routing, HTTP serialization, network transit, the CLI lease loop, or the hosted deployment.

## Reproduce

The repository already declares Criterion as a development dependency; no global installation is required.

```bash
# Compile the exact benchmark target without running measurements.
cargo bench --bench lock_benchmarks --no-run

# Run the direct-store suite and generate Criterion reports.
cargo bench --bench lock_benchmarks
```

Before publishing performance results for the agent-first release, separately measure local p50 and p95 for acquire, renew, and first watch event under a stated HTTP concurrency and database configuration. Hosted measurements must be labeled separately and must not be presented as an SLA without an independently maintained production measurement lane.
