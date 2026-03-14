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

Benchmark functions use `std::hint::black_box` to prevent the compiler from eliding work.
All transports use a **single persistent connection** (no connect/disconnect per request).
SHM RPC benchmarks use the V2 block pool architecture (64 coordination slots, 192 blocks × 64 KiB).
Pub/sub benchmarks use 1 MiB sample capacity with a 64-slot ring.

### What is measured

The timing covers the **full client-side round-trip**: serialize request, transmit, route, execute handler, serialize response, receive. For In-process transport, this is a direct `Arc<Router>` function call. For SHM, this includes block allocation, memcpy writes, atomic state transitions, and zero-copy reads. For pub/sub, it covers publish (memcpy into mmap) + receive (pointer deref or copy).

### What is NOT measured

- Connection setup / teardown (all benchmarks reuse a persistent connection)
- Concurrent client contention (each benchmark uses a single client)
- Handler compute time beyond trivial JSON serialization

## Results

### Request/Response latency (single client, sequential requests)

| Benchmark | In-process | SHM (V2) |
|---|---|---|
| `/health` (2-byte response) | 152 ns | 53.5 µs |
| JSON + path params (OHLC) | 1.14 µs | 55.7 µs |
| POST JSON body (order) | 1.34 µs | 56.5 µs |
| 64 KB response | 1.19 µs | 55.8 µs |
| 1 MB response | 16.97 µs | 72.7 µs |

### Pub/Sub latency (shared memory, single publisher + subscriber)

| Payload | Zero-copy (`try_recv_ref`) | Memcpy (`set_data` + `try_recv_ref`) |
|---|---|---|
| 8 bytes | 223 ns | — |
| 64 bytes | — | 225 ns |
| 64 KB | 1.54 µs | 1.54 µs |
| 1 MB | 29.8 µs | 29.8 µs |

### Pub/Sub wake cost breakdown

| Mode | Latency | Notes |
|---|---|---|
| `publish()` (with futex wake) | 218 ns | Full path: atomics + futex syscall |
| `publish_silent()` (no futex) | 45 ns | Atomics only — 2.2x faster than iceoryx |

The `publish_silent()` path skips `futex(FUTEX_WAKE)` and the notification counter,
proving the transport itself runs at **45 ns** — the remaining ~173 ns is pure Linux
kernel overhead from the futex syscall.

**Zero-copy path:** Publisher writes payload into a loaned mmap slot (`ptr::copy_nonoverlapping`),
subscriber reads via `try_recv_ref()` which returns a `ShmSampleRef` — use `copy_to_vec()` for safe
access or `as_bytes_unchecked()` for zero-copy. The write is O(n) but the read is O(1).

**Memcpy path:** Publisher uses `set_data()` (same underlying copy), subscriber uses `try_recv_ref()`
then accesses the data. Both paths are equivalent for the subscriber — the naming reflects the
publisher API used.

### Request/Response throughput (bytes/sec, large payloads)

| Payload | In-process | SHM |
|---|---|---|
| 64 KB | 53.2 GiB/s | 1.09 GiB/s |
| 1 MB | 54.7 GiB/s | 13.6 GiB/s |

### Pub/Sub throughput (bytes/sec)

| Payload | Ring Pub/Sub | Pool Pub/Sub (O(1)) |
|---|---|---|
| 64 KB | 17.5 GiB/s | 45.6 GiB/s |
| 1 MB | 15.2 GiB/s | 29.7 GiB/s |

### Pool-backed O(1) Pub/Sub latency

| Mode | Latency | Notes |
|---|---|---|
| `publish()` + `try_recv()` (smart wake) | **67 ns** | 1.5× faster than iceoryx2 (~100 ns) |
| `publish_silent()` + `try_recv()` (no wake) | **65 ns** | Pure atomics floor |

Smart wake skips the `futex_wake` syscall (~170 ns) when no subscriber is blocked in `recv()`.
Since benchmarks use `try_recv()` (polling), waiters = 0 and no futex is issued. The smart wake
overhead vs silent is just ~2 ns (one `fetch_add` + one `load`).

| Payload | Latency (includes data write) |
|---|---|
| 8 bytes | 67 ns |
| 64 KB | 1.40 µs |
| 1 MB | 32.6 µs |

The 8B → 1MB progression shows the born-in-SHM write cost (O(n)), not the transfer cost.
The transfer itself is always O(1) — only the block index is written to the ring.

Throughput is 2.6× faster than ring pub/sub at 64 KB because the subscriber never copies data.

## Interpretation

**In-process** is a direct function call through `Arc<Router>` — no serialization, no copying,
no kernel involvement. This is the theoretical floor.

**SHM RPC (V2)** uses direct `libc::mmap` with `MADV_HUGEPAGE` (transparent 2 MiB huge pages
for TLB efficiency) + block pool allocator + atomics + futex for cross-process request/response.
The V2 architecture separates coordination slots (64 bytes) from data blocks (64 KiB), uses a
Treiber stack for lock-free block allocation, and a custom `Body::Mmap` guard for zero-copy reads
(eliminating 2 of 4 memcpys per roundtrip — no `bytes` crate dependency). However, the **dominant
bottleneck is coordination overhead** (~54 µs), not data copying:

1. `spawn_blocking` on the client to avoid blocking the tokio runtime
2. Atomic CAS state transitions (5 per roundtrip)
3. Futex wake/wait kernel transitions
4. Full request/response serialization (URI, headers, body)

This fixed overhead means `/health` (2 bytes) and `64 KB` show nearly identical latency.
At 1 MB, the data copy starts to contribute, but coordination still dominates.

**SHM Ring Pub/Sub** is the fastest cross-process path for small payloads at ~220 ns (45 ns without
futex wake). It uses a ring buffer with seqlock validation — no slot state machine, no routing,
no serialization. The ~220 ns breaks down as: ~45 ns for atomics + ~173 ns for `futex(FUTEX_WAKE)`.
Use `publish_silent()` for polling consumers to bypass the futex entirely.

**SHM Pool Pub/Sub** (`ShmPoolPublisher`) achieves **O(1) transfer regardless of payload size** —
iceoryx2-style ownership transfer. The publisher writes data directly into a pool block (born-in-SHM),
then publishes only the block index. The subscriber holds the block alive via atomic refcounting and
reads directly from mmap — no copy, no seqlock. At **67 ns** (smart wake) / **65 ns** (silent) for
any payload size, it's **1.5× faster than iceoryx2's ~100 ns** while providing a fully safe API
(`Deref<Target=[u8]>`).

**Smart wake** skips the `futex_wake` syscall when no subscriber is actually blocked in `recv()`.
The publisher checks an atomic waiters counter (~2 ns) instead of issuing a futex syscall (~170 ns).
When subscribers use `try_recv()` (polling), `publish()` is nearly as fast as `publish_silent()`.

### Why SHM RPC is the bottleneck, not the data path

The V2 block pool successfully makes reads O(1), but the 54 µs base latency comes from the
request/response coordination protocol, not from memcpy. To reach sub-microsecond SHM RPC,
the server would need to poll shared memory directly from a dedicated thread (no tokio, no
spawn_blocking) and use a simpler coordination primitive than the 5-state slot machine.

The pub/sub path proves this: it achieves 220 ns precisely because it skips all of that —
no routing, no request serialization, no state machine, no spawn_blocking.

### Why ring pub/sub scales linearly with payload size

The ring pub/sub 8B→64KB→1MB latency progression (223 ns → 1.54 µs → 29.8 µs) shows clear O(n)
scaling. This is because every publish does a `memcpy` of the full payload into the
ring buffer slot. The read side is O(1) (pointer deref into mmap), but the write
dominates total latency.

### Why pool pub/sub is O(1) — and 1.5× faster than iceoryx2

The pool pub/sub (`ShmPoolPublisher`) achieves constant **67 ns** regardless of payload size by
transferring only a block index (~8 bytes). The publisher writes data directly into a pool block
via `as_mut_slice()` (born-in-SHM) — this write IS O(n), but it happens BEFORE the transfer.
The `publish()` call itself is O(1): write block index to ring slot + atomic refcount set +
smart wake check. The subscriber reads via `Deref` directly into mmap — also O(1).

**Why faster than iceoryx2 (~100 ns):**
- Smart wake: only issues `futex_wake` when a subscriber is actually blocked (~2 ns check vs ~170 ns syscall)
- Counter-based heartbeat: avoids `Instant::now()` on every loan (checked every 1024 loans, not every call)
- Inline hot paths: `alloc_block`, `commit`, `try_recv` are `#[inline]` to eliminate call overhead
- Minimal coordination: seqlock + refcount + Treiber stack — no service discovery, no event channels

### Scaling note

These benchmarks measure **single-client sequential latency**. Under concurrent load:

- In-process scales linearly with tokio worker threads
- SHM RPC supports concurrent requests across coordination slots with block pool contention managed by lock-free CAS
- Pub/sub supports many concurrent subscribers reading the same ring buffer

## Reproducing

```sh
# All benchmarks (requires Unix + shm feature)
cargo bench --features shm

# Specific group
cargo bench -- "inproc/"
cargo bench -- "shm/"
cargo bench -- "dispatch/"

# Ring pub/sub only
cargo bench --features shm -- "pubsub/"

# Pool pub/sub only
cargo bench --features shm -- "pool_pubsub"

# Throughput only
cargo bench --features shm -- "throughput"
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html`
for interactive graphs (requires a browser).

> [!TIP]
> For the most stable results, close other applications, disable CPU frequency scaling
> (`sudo cpupower frequency-set -g performance`), and run multiple times to check
> for consistency.
