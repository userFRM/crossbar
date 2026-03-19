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
| 64 B | **54 ns** | 232 ns | 4.3× |
| 4 KB | **55 ns** | 234 ns | 4.2× |
| 64 KB | **54 ns** | 232 ns | 4.3× |
| 256 KB | **54 ns** | 234 ns | 4.3× |
| 1 MB | **54 ns** | 230 ns | 4.2× |

### End-to-end with full payload (loan → memcpy → publish → recv → deref)

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **54 ns** | 232 ns | **4.3×** |
| 1 KB | **66 ns** | 243 ns | **3.7×** |
| 64 KB | 1.35 µs | 1.38 µs | ~1× |
| 256 KB | 6.65 µs | 6.82 µs | ~1× |
| 1 MB | 31 µs | 29 µs | ~1× |

### Throughput (born-in-SHM write)

| Payload | throughput |
|---|---|
| 64 KB | **45 GiB/s** |
| 1 MB | **32 GiB/s** |

### Pinned mode (latest-value, same buffer every iteration)

`loan_pinned` / `try_recv_pinned` — no ring, no alloc, no refcount.

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **35 ns** | 229 ns | **6.5×** |
| 1 KB | **45 ns** | 238 ns | **5.3×** |
| 64 KB | **1.07 µs** | 1.30 µs | **1.2×** |
| 1 MB | 18.1 µs | 18.4 µs | ~1× |

> **Note:** The 8 B pinned result increased from ~26 ns to ~35 ns due to the CAS-based writer sentinel
> added for safety. This is the cost of the sentinel's compare-and-swap on every `loan_pinned` — a
> deliberate trade-off for correctness under concurrent access.

### Silent publish (no wake path)

| | latency |
|---|---|
| 8 B, `publish_silent()` | **56 ns** |

> The `smart_wake` path now also lands at **56 ns** — the wake-check overhead is in the noise.

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

**The crossbar advantage lives below 64 KB.** The speed difference comes from a lighter path — no service discovery, no POSIX configuration layer, straight to atomics. Above 64 KB, `memcpy` dominates and both frameworks are equal.

**Born-in-SHM avoids the memcpy entirely.** If the publisher writes directly into the loaned block (no intermediate copy), the transport is ~35 ns for small payloads regardless of payload size. Run `cargo bench -- born_in_shm` to see the head-to-head.

**Throughput is memory-bandwidth-bound.** 32–45 GiB/s is close to measured memory bandwidth — the bulk of time is writing the payload, not the framework.

**Multi-publisher overhead.** The protocol uses `fetch_add` for atomic sequence claiming and CAS-based ring slot locking. In single-publisher mode (the common case), the seqlock uses a plain store instead of CAS, saving ~10–15 ns. The `silent_no_wake` path at 56 ns shows the pure atomics floor without notification overhead.

**Per-publisher block cache.** Each publisher caches up to 8 blocks locally, amortizing the Treiber stack CAS over multiple loans. Under contention (multiple publishers), this eliminates most CAS retries on the pool head.
