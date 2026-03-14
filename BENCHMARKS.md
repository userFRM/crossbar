# Benchmarks

All numbers from [Criterion](https://github.com/bheisler/criterion.rs) microbenchmarks
in `benches/transport.rs`. Reproduce on your hardware:

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
> These numbers are from **one machine on one day**. The relative ordering between
> transports is what matters — not the absolute values. **Run the benchmarks yourself.**

## Methodology

Each benchmark creates the relevant transport, then runs the Criterion harness: automatic
warm-up, 100 statistical samples. Functions use `std::hint::black_box` to prevent elision.
All transports use a single persistent connection. SHM RPC uses V2 block pool architecture
(64 coordination slots, 192 blocks × 64 KiB). Pool pub/sub uses 64 blocks × 1 MiB.

**What is measured:** full client-side round-trip (publish + receive for pub/sub, or
serialize + transmit + route + respond + receive for RPC).

**What is NOT measured:** connection setup, concurrent contention, handler compute time.

## Results

### Pool Pub/Sub — O(1) transfer (headline numbers)

| Mode | Latency | Notes |
|---|---|---|
| `publish()` + `try_recv()` (smart wake) | **67 ns** | 1.5× faster than iceoryx2 (~100 ns) |
| `publish_silent()` + `try_recv()` | **65 ns** | Pure atomics floor |

Smart wake: publisher checks an atomic waiters counter (~2 ns) instead of issuing a
`futex_wake` syscall (~170 ns). When subscribers poll with `try_recv()`, no futex is issued.

| Payload (includes born-in-SHM write) | Latency |
|---|---|
| 8 bytes | 67 ns |
| 64 KB | 1.40 µs |
| 1 MB | 32.6 µs |

The 8B→1MB progression is the write cost (O(n)), not the transfer. The transfer itself is
always O(1) — only the block index is written to the ring.

### Pool Pub/Sub throughput

| Payload | Pool Pub/Sub (O(1)) | Ring Pub/Sub | Pool speedup |
|---|---|---|---|
| 64 KB | **45.6 GiB/s** | 17.5 GiB/s | 2.6× |
| 1 MB | **29.7 GiB/s** | 15.2 GiB/s | 2.0× |

### Ring Pub/Sub — specialized small-payload path

| Mode | Latency | Notes |
|---|---|---|
| `publish()` + futex wake | 218 ns | ~45 ns atomics + ~173 ns futex syscall |
| `publish_silent()` (no futex) | 45 ns | Lowest achievable latency, unsafe reads |

| Payload | Zero-copy (`try_recv_ref`) |
|---|---|
| 8 bytes | 223 ns |
| 64 KB | 1.54 µs |
| 1 MB | 29.8 µs |

Ring pub/sub scales O(n) with payload size (memcpy into ring slot). Subscriber reads
via `ShmSampleRef` require `unsafe` (`as_bytes_unchecked()`) or a copy (`copy_to_vec()`).

### Request/Response latency (SHM RPC)

| Benchmark | In-process | SHM (V2) |
|---|---|---|
| `/health` (2B) | 152 ns | 53.5 µs |
| JSON + path params (OHLC) | 1.14 µs | 55.7 µs |
| POST JSON body (order) | 1.34 µs | 56.5 µs |
| 64 KB response | 1.19 µs | 55.8 µs |
| 1 MB response | 16.97 µs | 72.7 µs |

### Request/Response throughput

| Payload | In-process | SHM |
|---|---|---|
| 64 KB | 53.2 GiB/s | 1.09 GiB/s |
| 1 MB | 54.7 GiB/s | 13.6 GiB/s |

## Interpretation

### Why pool pub/sub is O(1) — and 1.5× faster than iceoryx2

The pool pub/sub transfers only a block index (~8 bytes) regardless of payload size.
The publisher writes data directly into a pool block via `as_mut_slice()` (born-in-SHM).
The `publish()` call writes the block index to a ring slot + sets refcount + checks waiters.
The subscriber reads via `Deref` directly into mmap — also O(1).

**Why faster than iceoryx2 (~100 ns):**
- Smart wake: checks atomic waiters counter (~2 ns) instead of futex syscall (~170 ns)
- Counter-based heartbeat: checks clock every 1024 loans, not every call
- Inline hot paths: `alloc_block`, `commit`, `try_recv` are `#[inline]`
- Minimal coordination: seqlock + refcount + Treiber stack — no service discovery

### Why SHM RPC is ~54 µs

The V2 block pool makes reads O(1), but the base latency comes from coordination:

1. `spawn_blocking` to avoid blocking the tokio runtime (~15-20 µs)
2. Atomic CAS state transitions (5 per roundtrip)
3. Futex wake/wait kernel transitions
4. Full request/response serialization (URI, headers, body)

The pub/sub path proves this: it achieves 67 ns precisely because it skips all of that.

### Scaling note

These benchmarks measure **single-client sequential latency**. Under concurrent load:

- In-process scales linearly with tokio worker threads
- SHM RPC supports concurrent requests across coordination slots (lock-free CAS)
- Pub/sub supports many concurrent subscribers reading the same ring/pool

## Reproducing

```sh
# All benchmarks (requires Unix + shm feature)
cargo bench --features shm

# Pool pub/sub only
cargo bench --features shm -- "pool_pubsub"

# Ring pub/sub only
cargo bench --features shm -- "pubsub/"

# SHM RPC only
cargo bench --features shm -- "shm/"

# Throughput only
cargo bench --features shm -- "throughput"
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html`
for interactive graphs (requires a browser).

> [!TIP]
> For the most stable results, close other applications, disable CPU frequency scaling
> (`sudo cpupower frequency-set -g performance`), and run multiple times to check
> for consistency.
