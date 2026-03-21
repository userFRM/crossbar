# Benchmarks — v0.6.0

All numbers from [Criterion](https://github.com/bheisler/criterion.rs) benchmarks in `benches/pubsub.rs`.
Same-process publisher + subscriber. `try_recv` only (no futex syscall — `WAITERS = 0`).

> Numbers are directional. Re-run on your hardware before making decisions.
> `cargo bench` — all benchmarks
> `cargo bench -- head_to_head` — iceoryx2 comparison only (Unix, requires iceoryx2 dev-dep)

---

## Intel i7-10700KF @ 3.80 GHz · Linux 6.8 · rustc 1.87

### Pool-backed pub/sub

| Payload | latency |
|---|---|
| 8 B (`smart_wake`) | **58 ns** |
| 8 B (`publish_silent`) | **57 ns** |
| 64 KB | 1.34 µs |
| 1 MB | 30 µs |

### Throughput (born-in-SHM write)

| Payload | throughput |
|---|---|
| 64 KB | **46 GiB/s** |
| 1 MB | **33 GiB/s** |

### Pinned mode (latest-value, same buffer every iteration)

`loan_pinned` / `try_recv_pinned` — no ring, no alloc, no refcount.

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **35 ns** | 229 ns | **6.5×** |
| 1 KB | **45 ns** | 237 ns | **5.3×** |
| 64 KB | **1.10 µs** | 1.32 µs | **1.2×** |
| 1 MB | 18.4 µs | 18.3 µs | ~1× |

### PodBus (seqlock-based single-value broadcast)

| Payload | latency |
|---|---|
| 8 B | **3.1 ns** |
| 64 B | 32 ns |
| 256 B | 122 ns |
| 1 KB | 497 ns |

Fanout: 10 subscribers — **19 ns/msg**, **53M msg/s**.

### Transport overhead (8 B write, varying backing buffer)

Proves O(1): latency is flat regardless of how large the backing block is.

| Backing buffer | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **53 ns** | 225 ns | 4.2× |

### End-to-end with full payload (loan → memcpy → publish → recv → deref)

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **55 ns** | 230 ns | **4.2×** |
| 1 KB | **67 ns** | 239 ns | **3.6×** |
| 64 KB | 1.47 µs | 1.32 µs | 0.9× |
| 256 KB | 7.10 µs | 6.87 µs | 0.97× |
| 1 MB | 30.7 µs | 29.8 µs | 0.97× |

---

## Apple M1 Pro · macOS · rustc 1.92.0

> These numbers are from a previous session (v0.3.0) and have not been re-run after the v0.3.1 performance overhaul. Expect improvement.

### Transport overhead

| Backing buffer | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 64 B | **52 ns** | 189 ns | 3.6× |
| 4 KB | **52 ns** | 189 ns | 3.6× |
| 64 KB | **52 ns** | 189 ns | 3.7× |
| 256 KB | **52 ns** | 188 ns | 3.6× |
| 1 MB | **55 ns** | 188 ns | 3.4× |

### End-to-end with full payload

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **55 ns** | 189 ns | 3.5× |
| 1 KB | **77 ns** | 210 ns | 2.7× |
| 64 KB | 1.27 µs | 1.35 µs | 1.1× |
| 256 KB | 5.10 µs | 5.20 µs | ~1× |
| 1 MB | 23.9 µs | 23.5 µs | ~1× |

### Throughput

| Payload | throughput |
|---|---|
| 64 KB | **48.8 GiB/s** |
| 1 MB | **43.3 GiB/s** |

---

## Reading the numbers

**The crossbar advantage lives below 64 KB.** The speed difference comes from a lighter path — no service discovery, no POSIX configuration layer, straight to atomics. Above 64 KB, `memcpy` dominates and both frameworks converge. At 64 KB+ iceoryx2 is slightly faster (0.9–0.97×) — the numbers are honest.

**Born-in-SHM avoids the memcpy entirely.** If the publisher writes directly into the loaned block (no intermediate copy), the transport is ~35 ns for small payloads regardless of payload size. Run `cargo bench -- born_in_shm` to see the head-to-head.

**Throughput is memory-bandwidth-bound.** 33–46 GiB/s is close to measured memory bandwidth — the bulk of time is writing the payload, not the framework.

**Multi-publisher overhead.** The protocol uses `fetch_add` for atomic sequence claiming and CAS-based ring slot locking. In single-publisher mode (the common case), the seqlock uses a plain store instead of CAS, saving ~10–15 ns. The `silent_no_wake` path at 57 ns shows the pure atomics floor without notification overhead.

**Per-publisher block cache.** Each publisher caches up to 8 blocks locally, amortizing the Treiber stack CAS over multiple loans. Under contention (multiple publishers), this eliminates most CAS retries on the pool head.

---

## Methodology & Caveats

All benchmarks use Criterion with same-process publisher and subscriber on a single thread.
`try_recv()` is used (no futex syscall). The ring buffer is cache-hot throughout.

**These numbers represent peak theoretical performance, not real-world IPC latency.**

### What the benchmarks hide

- **No cross-process cost**: Publisher and subscriber share the same address space.
  Real IPC crosses process boundaries, incurring TLB misses and cache coherency traffic.
  Expect 2-5x higher latency for small payloads in cross-process scenarios.

- **No cross-core cost**: Both run on the same core. Cross-core RFO stalls add
  30-60 ns per cache line transfer. The 4.2x advantage over iceoryx2 at 8B would
  shrink in multi-core deployments.

- **No blocking/wake cost**: `try_recv()` never blocks. The futex wake path
  (~170 ns per `futex_wake` syscall) is never exercised.

- **Cache-hot ring**: The ring buffer stays in L1/L2 throughout. Real workloads
  with competing processes will see cache evictions.

- **46 GiB/s throughput**: Approaches DDR4-3200 theoretical peak (51.2 GiB/s).
  This measures intra-core memcpy speed, not achievable IPC throughput.

- **PodBus 3.1 ns**: Measures seqlock overhead within a single core's L1 cache
  (~12 clock cycles). This is not an IPC metric — it's a thread-local data
  structure benchmark.

### What the benchmarks prove

- The library's internal overhead is minimal — the bottleneck is memcpy at 64KB+.
- O(1) transfer is real: latency is flat regardless of backing buffer size.
- The lock-free algorithms (Treiber stack, seqlock, CAS ring) are correctly
  optimized for the hot path.

### Recommended: cross-process benchmarks

Use `examples/publisher` + `examples/subscriber` for true cross-process latency
measurements with `taskset` to pin to different physical cores.
