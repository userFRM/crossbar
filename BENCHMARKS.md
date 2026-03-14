# Benchmarks

All numbers on this page come from [Criterion](https://github.com/bheisler/criterion.rs) microbenchmarks
shipped in `benches/transport.rs`. You can reproduce them on your own hardware:

```sh
cargo bench --features shm
```

## Test environment

| | |
|---|---|
| **CPU** | Intel Core i7-10700KF @ 3.80 GHz (8C / 16T) |
| **RAM** | 128 GB DDR4 |
| **OS** | Ubuntu, kernel 6.8.0-101-generic |
| **Rust** | 1.93.1 (stable) |
| **Profile** | `release` (Criterion default) |

> [!IMPORTANT]
> These numbers are from **one machine on one day**. Actual latency depends on your CPU, kernel
> version, system load, and NUMA topology. The relative ordering between transports is what
> matters — not the absolute values. **Run the benchmarks yourself.**

## Methodology

Each benchmark:

1. Creates a shared `Router` with the same five endpoints (health, OHLC JSON, POST order, 64 KB response, 1 MB response).
2. Spins up the relevant transport server (if any).
3. Runs the Criterion harness: automatic warm-up, then 100 statistical samples.

Benchmark functions use `criterion::black_box` to prevent the compiler from eliding work.
All transports use a **single persistent connection** (no connect/disconnect per request).
SHM RPC benchmarks use the V2 block pool architecture (64 coordination slots, 192 blocks × 64 KiB).
Pub/sub benchmarks use 1 MiB sample capacity with a 64-slot ring.

### What is measured

The timing covers the **full client-side round-trip**: serialize request, transmit, route, execute handler, serialize response, receive. For Memory transport, this is a direct `Arc<Router>` function call. For SHM, this includes block allocation, memcpy writes, atomic state transitions, and zero-copy reads. For pub/sub, it covers publish (memcpy into mmap) + receive (pointer deref or copy).

### What is NOT measured

- Connection setup / teardown (all benchmarks reuse a persistent connection)
- Concurrent client contention (each benchmark uses a single client)
- Handler compute time beyond trivial JSON serialization

## Results

### Request/Response latency (single client, sequential requests)

| Benchmark | Memory | SHM (V2) |
|---|---|---|
| `/health` (2-byte response) | 149 ns | 54.1 µs |
| JSON + path params (OHLC) | 1.17 µs | 55.6 µs |
| POST JSON body (order) | 1.33 µs | 56.0 µs |
| 64 KB response | 1.25 µs | 55.6 µs |
| 1 MB response | 16.9 µs | 71.8 µs |

### Pub/Sub latency (shared memory, single publisher + subscriber)

| Payload | Zero-copy (`try_recv_ref`) | Memcpy (`set_data` + `try_recv_ref`) |
|---|---|---|
| 8 bytes | 220 ns | — |
| 64 bytes | — | 225 ns |
| 64 KB | 1.50 µs | 1.50 µs |
| 1 MB | 29.6 µs | 29.5 µs |

**Zero-copy path:** Publisher writes payload into a loaned mmap slot (`copy_from_slice`),
subscriber reads via `try_recv_ref()` which returns a pointer into mmap — no allocation, no copy.
The write is O(n) but the read is O(1).

**Memcpy path:** Publisher uses `set_data()` (same underlying copy), subscriber uses `try_recv_ref()`
then accesses the data (pointer deref, same as zero-copy). Both paths are equivalent for the
subscriber — the naming reflects the publisher API used.

### Request/Response throughput (bytes/sec, large payloads)

| Payload | Memory | SHM |
|---|---|---|
| 64 KB | 50.2 GiB/s | 1.1 GiB/s |
| 1 MB | 53.5 GiB/s | 13.6 GiB/s |

### Pub/Sub throughput (bytes/sec)

| Payload | Pub/Sub SHM |
|---|---|
| 64 KB | 18.8 GiB/s |
| 1 MB | 15.3 GiB/s |

## Interpretation

**Memory** is a direct function call through `Arc<Router>` — no serialization, no copying,
no kernel involvement. This is the theoretical floor.

**SHM RPC (V2)** uses `mmap` + block pool allocator + atomics + futex for cross-process
request/response. The V2 architecture separates coordination slots (64 bytes) from data blocks
(64 KiB), uses a Treiber stack for lock-free block allocation, and `Bytes::from_owner` for
zero-copy reads (eliminating 2 of 4 memcpys per roundtrip). However, the **dominant bottleneck
is coordination overhead** (~54 µs), not data copying:

1. `spawn_blocking` on the client to avoid blocking the tokio runtime
2. Atomic CAS state transitions (5 per roundtrip)
3. Futex wake/wait kernel transitions
4. Full request/response serialization (URI, headers, body)

This fixed overhead means `/health` (2 bytes) and `64 KB` show nearly identical latency.
At 1 MB, the data copy starts to contribute, but coordination still dominates.

**SHM Pub/Sub** is the fastest cross-process path at 220 ns for small payloads. It uses a
ring buffer with seqlock validation — no slot state machine, no routing, no serialization.
However, the write is still O(n) because data must be copied into the mmap region. True O(1)
transfer (like iceoryx2's ~100 ns for any size) would require data to be "born" in shared
memory, which crossbar does not yet support.

### Why SHM RPC is the bottleneck, not the data path

The V2 block pool successfully makes reads O(1), but the 54 µs base latency comes from the
request/response coordination protocol, not from memcpy. To reach sub-microsecond SHM RPC,
the server would need to poll shared memory directly from a dedicated thread (no tokio, no
spawn_blocking) and use a simpler coordination primitive than the 5-state slot machine.

The pub/sub path proves this: it achieves 220 ns precisely because it skips all of that —
no routing, no request serialization, no state machine, no spawn_blocking.

### Why pub/sub scales linearly with payload size

The 8B→64KB→1MB latency progression (220 ns → 1.50 µs → 29.6 µs) shows clear O(n)
scaling. This is because every publish does a `memcpy` of the full payload into the
ring buffer slot. The read side is O(1) (pointer deref into mmap), but the write
dominates total latency.

For comparison, iceoryx2 achieves ~100 ns regardless of payload size because the
publisher writes data directly into the shared memory buffer (data is "born in SHM")
and only an 8-byte offset is transferred to the subscriber. Crossbar's current
architecture requires the publisher to own the data externally and copy it in.

### Scaling note

These benchmarks measure **single-client sequential latency**. Under concurrent load:

- Memory scales linearly with tokio worker threads
- SHM RPC supports concurrent requests across coordination slots with block pool contention managed by lock-free CAS
- Pub/sub supports many concurrent subscribers reading the same ring buffer

## Reproducing

```sh
# All benchmarks (requires Unix + shm feature)
cargo bench --features shm

# Specific group
cargo bench -- "memory/"
cargo bench -- "shm/"
cargo bench -- "dispatch/"

# Pub/sub only
cargo bench --features shm -- "pubsub"

# Throughput only
cargo bench --features shm -- "throughput"
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html`
for interactive graphs (requires a browser).

> [!TIP]
> For the most stable results, close other applications, disable CPU frequency scaling
> (`sudo cpupower frequency-set -g performance`), and run multiple times to check
> for consistency.
