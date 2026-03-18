# Benchmarks

All numbers from [Criterion](https://github.com/bheisler/criterion.rs) benchmarks in `benches/pubsub.rs`.
Same-process publisher + subscriber. `try_recv` only (no futex syscall — `WAITERS = 0`).

> Numbers are directional. Re-run on your hardware before making decisions.
> `cargo bench` — all benchmarks
> `cargo bench -- head_to_head` — iceoryx2 comparison only (Unix, requires iceoryx2 dev-dep)

---

## Intel i7-10700KF @ 3.80 GHz · Linux 6.8 · rustc 1.87

### Transport overhead (8 B write, varying backing buffer)

Proves O(1): latency is flat regardless of how large the backing block is.

| Backing buffer | crossbar | iceoryx2 | ratio |
|---|---|---|---|
| 64 B | **56 ns** | 233 ns | 4.2× faster |
| 4 KB | **56 ns** | 234 ns | 4.2× faster |
| 64 KB | **59 ns** | 237 ns | 4.0× faster |
| 256 KB | **59 ns** | 248 ns | 4.2× faster |
| 1 MB | **59 ns** | 234 ns | 4.0× faster |

### End-to-end with full payload (loan → memcpy → publish → recv → deref)

| Payload | crossbar | iceoryx2 | ratio |
|---|---|---|---|
| 8 B | **60 ns** | 235 ns | **3.9× faster** |
| 1 KB | **72 ns** | 250 ns | **3.5× faster** |
| 64 KB | 1.53 µs | 1.31 µs | 0.86× (iceoryx2 wins) |
| 256 KB | 6.84 µs | 7.00 µs | ~1× |
| 1 MB | 45 µs | 32 µs | 0.71× (iceoryx2 wins) |

At 64 KB+, iceoryx2's `memcpy` path is faster — likely from better SIMD-optimized copy routines or memory prefetching. Crossbar's `set_data()` uses plain `copy_nonoverlapping`. The born-in-SHM pattern (`as_mut_slice()`) eliminates this gap by avoiding the copy entirely.

### Throughput (born-in-SHM write)

| Payload | throughput |
|---|---|
| 64 KB | **42.5 GiB/s** |
| 1 MB | **15.7 GiB/s** |

### Silent publish (no wake path)

| | latency |
|---|---|
| 8 B, `publish_silent()` | **57 ns** |

---

## Apple M1 Pro · macOS · rustc 1.92.0

> These numbers are from a previous session and have not been re-run after multi-publisher changes.

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

**The crossbar advantage lives below 64 KB.** The speed difference comes from a lighter path — no service discovery, no POSIX configuration layer, straight to atomics. Above 64 KB, `memcpy` dominates and both frameworks are equal.

**Born-in-SHM avoids the memcpy entirely.** If the publisher writes directly into the loaned block (no intermediate copy), the latency at any payload size is just the transport overhead: ~52–60 ns depending on hardware. The throughput numbers above measure the memcpy cost separately.

**Throughput is memory-bandwidth-bound.** 45–49 GiB/s is close to measured memory bandwidth — the bulk of time is writing the payload, not the framework.

**Multi-publisher overhead.** The protocol uses `fetch_add` for atomic sequence claiming and CAS-based ring slot locking. In single-publisher mode (the common case), the seqlock uses a plain store instead of CAS, saving ~10–15 ns. The `silent_no_wake` path at 58 ns shows the pure atomics floor without notification overhead.

**Per-publisher block cache.** Each publisher caches up to 8 blocks locally, amortizing the Treiber stack CAS over multiple loans. Under contention (multiple publishers), this eliminates most CAS retries on the pool head.
