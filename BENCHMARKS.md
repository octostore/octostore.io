# OctoStore Benchmarks

## System Specifications

- **OS**: Linux 6.8.0-94-generic (Ubuntu)
- **CPU**: 16 cores (x86_64)
- **RAM**: 30GB total, ~25GB available
- **Storage**: SSD (demo-host: expanso-demo-machine)
- **Date**: 2026-02-13

## Benchmark Results

### Current Status

⚠️ **Note**: Benchmarks are currently experiencing compilation issues due to test code requiring fixes. The benchmark infrastructure is in place with comprehensive criterion-based tests.

### Available Benchmarks

The project includes comprehensive benchmarks in `benches/lock_benchmarks.rs`:

#### Micro-benchmarks (Direct LockStore API)
- `acquire_lock` - Measures lock acquisition latency
- `release_lock` - Measures lock release latency  
- `acquire_release_cycle` - Full acquire→release cycle time
- `fencing_token_generation` - Fencing token generation performance
- `sqlite_persistence` - SQLite write-through latency

#### Concurrency Benchmarks
- `contention_2_threads` - Lock contention with 2 competing threads
- `contention_10_threads` - Lock contention with 10 competing threads
- `many_different_locks` - Performance with 1000+ unique locks

#### HTTP-level Benchmarks
- `http_acquire_release` - Full HTTP stack performance

### Benchmark Categories

1. **Latency Tests**: Single-operation timing
2. **Throughput Tests**: Operations per second under load
3. **Concurrency Tests**: Multi-threaded lock contention
4. **Persistence Tests**: SQLite I/O performance
5. **HTTP Stack Tests**: End-to-end API performance

## Reproduction Instructions

```bash
# Install dependencies
cargo install criterion

# Run all benchmarks
cargo bench

# Run specific benchmark group
cargo bench -- acquire

# Generate detailed HTML reports
cargo bench -- --save-baseline main
```

## Expected Performance Characteristics

Based on the architecture (DashMap + SQLite + Axum):

- **Single acquire/release**: Sub-millisecond latency
- **Lock contention**: Graceful degradation with multiple threads
- **Fencing tokens**: Monotonic, persistent counter performance
- **HTTP overhead**: Minimal added latency over direct API calls

## Future Improvements

Once compilation issues are resolved:

1. Run full benchmark suite
2. Establish performance baselines
3. Monitor regression over time
4. Add memory usage benchmarks
5. Test under various load patterns

---

*Generated on 2026-02-13 via OpenClaw automation*