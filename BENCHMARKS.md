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
SHM RPC benchmarks use the default configuration (64 slots, 64 KiB slot capacity).
Pub/sub benchmarks use 1 MiB sample capacity with a 64-slot ring.

### What is measured

The timing covers the **full client-side round-trip**: serialize request, transmit, route, execute handler, serialize response, receive. For Memory transport, this is a direct `Arc<Router>` function call. For network transports (UDS, TCP), this includes kernel syscalls. For pub/sub, it covers publish (memcpy into mmap) + receive (pointer deref or copy).

### What is NOT measured

- Connection setup / teardown (all benchmarks reuse a persistent connection)
- Concurrent client contention (each benchmark uses a single client)
- Handler compute time beyond trivial JSON serialization

## Results

### Request/Response latency (single client, sequential requests)

| Benchmark | Memory | Channel | SHM | UDS | TCP |
|---|---|---|---|---|---|
| `/health` (2-byte response) | 145 ns | 5.97 µs | 53.1 µs | 13.6 µs | 27.7 µs |
| JSON + path params (OHLC) | 1.13 µs | 8.42 µs | 55.6 µs | 18.0 µs | 31.8 µs |
| POST JSON body (order) | 1.35 µs | 8.43 µs | 55.6 µs | 16.5 µs | 32.9 µs |
| 64 KB response | 1.38 µs | 8.13 µs | 55.6 µs | 26.1 µs | 52.1 µs |
| 1 MB response | 19.3 µs | 23.7 µs | 72.5 µs | 211 µs | 214 µs |

### Pub/Sub latency (shared memory, single publisher + subscriber)

| Payload | Zero-copy (`try_recv_ref`) | Memcpy (`set_data` + `try_recv_ref`) |
|---|---|---|
| 8 bytes | 223 ns | — |
| 64 bytes | — | 227 ns |
| 64 KB | 1.48 µs | 1.52 µs |
| 1 MB | 29.3 µs | 29.1 µs |

**Zero-copy path:** Publisher writes payload into a loaned mmap slot (`copy_from_slice`),
subscriber reads via `try_recv_ref()` which returns a pointer into mmap — no allocation, no copy.
The write is O(n) but the read is O(1).

**Memcpy path:** Publisher uses `set_data()` (same underlying copy), subscriber uses `try_recv_ref()`
then accesses the data (pointer deref, same as zero-copy). Both paths are equivalent for the
subscriber — the naming reflects the publisher API used.

### Request/Response throughput (bytes/sec, large payloads)

| Payload | Memory | SHM | UDS | TCP |
|---|---|---|---|---|
| 64 KB | 53.0 GiB/s | 1.1 GiB/s | 2.3 GiB/s | 1.4 GiB/s |
| 1 MB | 51.9 GiB/s | 13.5 GiB/s | 4.5 GiB/s | 4.3 GiB/s |

### Pub/Sub throughput (bytes/sec)

| Payload | Pub/Sub SHM |
|---|---|
| 64 KB | 18.4 GiB/s |
| 1 MB | 15.5 GiB/s |

## Interpretation

**Memory** is a direct function call through `Arc<Router>` — no serialization, no copying,
no kernel involvement. This is the theoretical floor.

**Channel** adds tokio `mpsc` + `oneshot` overhead (~6 µs base). Good for cross-task
communication within one process.

**SHM RPC** uses `mmap` + atomics + futex for cross-process request/response. The current V1
implementation has significant coordination overhead (~53 µs base) from its spin-poll slot
state machine, making it **slower than UDS and TCP for small payloads**. At 1 MB, the data
copy dominates and SHM recovers because it avoids kernel buffer copies. This is a known
limitation — the V2 redesign (block pool + `Bytes::from_owner`) targets eliminating read-side
copies entirely.

**SHM Pub/Sub** is the fastest cross-process path at 223 ns for small payloads. It uses a
ring buffer with seqlock validation — no slot state machine, no routing, no serialization.
However, the write is still O(n) because data must be copied into the mmap region. True O(1)
transfer (like iceoryx2's ~100 ns for any size) would require data to be "born" in shared
memory, which crossbar does not yet support.

**UDS** and **TCP** go through the kernel. UDS avoids the TCP/IP stack but still requires
`write`/`read` syscalls. TCP adds protocol overhead and `TCP_NODELAY` latency.

### Why SHM RPC is slow for small payloads

The SHM RPC transport uses a spin-poll server loop that checks slot states in a tight loop.
The ~53 µs base latency comes from:

1. Atomic CAS state transitions (5 per roundtrip)
2. `spawn_blocking` overhead on the client side
3. Futex wake/wait kernel transitions
4. Full request/response serialization (URI, headers, body) into the slot

This overhead is fixed regardless of payload size, which is why `/health` (2 bytes) and
`64 KB` both show ~55 µs. The pub/sub path avoids all of this.

### Why pub/sub scales linearly with payload size

The 8B→64KB→1MB latency progression (223 ns → 1.48 µs → 29.3 µs) shows clear O(n)
scaling. This is because every publish does a `memcpy` of the full payload into the
ring buffer slot. The read side is O(1) (pointer deref into mmap), but the write
dominates total latency.

For comparison, iceoryx2 achieves ~100 ns regardless of payload size because the
publisher writes data directly into the shared memory buffer (data is "born in SHM")
and only an 8-byte offset is transferred to the subscriber. Crossbar's current
architecture requires the publisher to own the data externally and copy it in.

### Scaling note

These benchmarks measure **single-client sequential latency**. Under concurrent load:

- Memory and Channel scale linearly with tokio worker threads
- SHM RPC supports up to 64 concurrent requests (one per slot) with no contention
- Pub/sub supports many concurrent subscribers reading the same ring buffer
- UDS and TCP are limited by kernel socket buffer sizes and context-switch overhead

## Reproducing

```sh
# All transports (requires Unix + shm feature)
cargo bench --features shm

# Specific transport group
cargo bench -- "memory/"
cargo bench -- "channel/"
cargo bench -- "tcp/"
cargo bench -- "uds/"
cargo bench -- "shm/"

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
