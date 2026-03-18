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

| Backing buffer | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 64 B | **49 ns** | 233 ns | 4.8× |
| 4 KB | **49 ns** | 232 ns | 4.7× |
| 64 KB | **50 ns** | 235 ns | 4.7× |
| 256 KB | **50 ns** | 234 ns | 4.7× |
| 1 MB | **50 ns** | 234 ns | 4.7× |

### End-to-end with full payload (loan → memcpy → publish → recv → deref)

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **50 ns** | 240 ns | **4.8×** |
| 1 KB | **65 ns** | 250 ns | **3.8×** |
| 64 KB | 1.35 µs | 1.35 µs | ~1× |
| 256 KB | 7.20 µs | 6.89 µs | ~1× |
| 1 MB | 33 µs | 30 µs | ~1× |

### Throughput (born-in-SHM write)

| Payload | throughput |
|---|---|
| 64 KB | **42.5 GiB/s** |
| 1 MB | **15.7 GiB/s** |

### Silent publish (no wake path)

| | latency |
|---|---|
| 8 B, `publish_silent()` | **46 ns** |

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
