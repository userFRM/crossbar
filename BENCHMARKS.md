# Benchmarks

All numbers from [Criterion](https://github.com/bheisler/criterion.rs) microbenchmarks
in `benches/transport.rs`.

## Environment

| | |
|---|---|
| **CPU** | Intel Core i7-10700KF @ 3.80 GHz (8C / 16T) |
| **RAM** | 128 GB DDR4 |
| **OS** | Ubuntu, kernel 6.8.0-101-generic |
| **Rust** | 1.93.1 (stable) |
| **Profile** | `release` (Criterion default) |

> [!IMPORTANT]
> These numbers are from **one machine on one day**. The relative ordering
> matters — not the absolute values. **Run the benchmarks yourself.**

## Methodology

Each benchmark creates the relevant transport, then runs the Criterion harness: automatic
warm-up, 100 statistical samples. Functions use `std::hint::black_box` to prevent elision.

**What is measured:** full round-trip (publish + receive for pub/sub, or
serialize + transmit + route + respond + receive for RPC).

**How it runs:** publisher/subscriber and client/server run in the **same process**,
sharing the same mmap region. This isolates transport overhead from kernel scheduling
and cross-core cache effects. Real cross-process latency will be higher.

**What is NOT measured:** connection setup, concurrent contention, handler compute time,
cross-process kernel scheduling.

---

## Pub/Sub

Pool pub/sub uses 64 blocks x (1 MiB + 8B header).

### Latency

| Mode | Latency | Notes |
|---|---|---|
| `publish()` + `try_recv()` (smart wake) | **67 ns** | 1.5x faster than iceoryx2 (~100 ns) |
| `publish_silent()` + `try_recv()` | **65 ns** | Pure atomics floor |

Smart wake: publisher checks an atomic waiters counter (~2 ns) instead of issuing a
`futex_wake` syscall (~170 ns). When subscribers poll with `try_recv()`, no futex is issued.

### Latency by payload size

| Payload | Latency |
|---|---|
| 8 B | 67 ns |
| 64 KB | 1.40 µs |
| 1 MB | 32.6 µs |

The 8B-to-1MB progression is the cost of writing data into the SHM block (O(n)).
The transfer itself is always O(1) — only an 8-byte descriptor is written to the ring.

### Throughput

| Payload | Throughput |
|---|---|
| 64 KB | **45.6 GiB/s** |
| 1 MB | **29.7 GiB/s** |

---

## RPC

SHM RPC uses V2 block pool architecture (64 coordination slots, 192 blocks x 64 KiB)
with inline spin response detection, hint-based slot scanning, and zero-alloc route
matching.

### Latency

| Benchmark | In-process | SHM | SHM overhead |
|---|---|---|---|
| `/health` (2B) | 143 ns | **757 ns** | 614 ns |
| JSON + path params (OHLC) | 937 ns | 1.63 µs | 693 ns |
| POST JSON body (order) | 1.26 µs | 1.86 µs | 600 ns |
| 64 KB response | 1.28 µs | 1.96 µs | 680 ns |
| 1 MB response | 18.3 µs | 18.9 µs | 600 ns |

### Throughput

| Payload | In-process | SHM |
|---|---|---|
| 64 KB | 49.7 GiB/s | 35.6 GiB/s |
| 1 MB | 50.9 GiB/s | 54.2 GiB/s |

SHM throughput at 1 MB exceeds in-process because `Body::Mmap` zero-copy avoids the
`Vec<u8>` clone that in-process dispatch performs.

---

## Analysis

### Why pub/sub is 67 ns

The transfer is O(1): only an 8-byte descriptor (block index + data length) is written to
the ring, regardless of payload size. The publisher writes data directly into a pool block
via `as_mut_slice()` (born-in-SHM). The subscriber reads via `Deref` into mmap.

**Why faster than iceoryx2 (~100 ns):**
- Smart wake: checks atomic waiters counter (~2 ns) instead of futex syscall (~170 ns)
- Counter-based heartbeat: checks clock every 1024 loans, not every call
- Inline hot paths: `alloc_block`, `commit`, `try_recv` are `#[inline]`
- Minimal coordination: seqlock + refcount + Treiber stack — no service discovery

### Why RPC is ~757 ns

The data transfer is O(1) (block offsets + zero-copy reads via `Body::Mmap`). The
~757 ns is purely coordination overhead:

| Step | Cost |
|---|---|
| Block pool alloc (Treiber stack CAS) | ~50 ns |
| Slot acquisition (CAS + hint scan) | ~80 ns |
| Request serialization into block | ~100 ns |
| Smart wake (atomic store, no syscall under load) | ~5 ns |
| Server: hint scan + CAS + dispatch_fast | ~300 ns |
| Response serialization into block | ~80 ns |
| Client: spin detection + zero-copy read | ~100 ns |

### What was eliminated (71x speedup from naive)

| Optimization | Savings |
|---|---|
| **Inline spin** — client spins on slot state, catches fast handlers without channels | ~4 µs |
| **Dedicated OS threads** — server off tokio, no `spawn_blocking` | ~15-20 µs |
| **Smart wake** — skip `futex_wake` when server is busy | ~170 ns |
| **Try-poll dispatch** — noop waker poll, skip `block_on` for sync handlers | ~200 ns |
| **Hint-based scanning** — client + server skip to last-known slot | ~300 ns |
| **Zero-alloc route matching** — literal patterns bypass HashMap/Vec | ~50 ns |
| **Counter heartbeat** — check liveness every 1024 requests | ~20 ns |
| **`CLOCK_MONOTONIC_COARSE`** — ~6 ns timestamps vs ~25 ns | ~19 ns |

### Why larger payloads cost more

The O(1) claim applies to the **transfer** (writing a block offset to a coordination slot)
and the **read** (`Body::Mmap` points directly into mmap — zero copy). However, data must
still **enter** SHM somehow: the handler creates the response body (e.g. `vec![42u8; 65536]`)
and the transport `memcpy`s it into a pool block. Both operations are O(n).

For true O(1) end-to-end, handlers would need to write directly into SHM blocks
("born-in-SHM") — the same pattern pub/sub uses with `loan.as_mut_slice()`. This is a
planned future API extension.

### Scaling

These benchmarks measure **single-client sequential latency**. Under concurrent load:

- In-process scales linearly with tokio worker threads
- SHM RPC supports concurrent requests across coordination slots (lock-free CAS)
- Pub/sub supports many concurrent subscribers reading the same pool

---

## Reproducing

```sh
# All benchmarks (requires Unix + shm feature)
cargo bench --features shm

# Pub/sub only
cargo bench --features shm -- "pool_pubsub"

# SHM RPC only
cargo bench --features shm -- "shm/"

# Throughput only
cargo bench --features shm -- "throughput"
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html`
for interactive graphs.

> [!TIP]
> For stable results, close other applications, disable CPU frequency scaling
> (`sudo cpupower frequency-set -g performance`), and run multiple times.
