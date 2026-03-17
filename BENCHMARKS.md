# Benchmarks

All numbers from [Criterion](https://github.com/bheisler/criterion.rs) benchmarks in `benches/pubsub.rs`.
Same-process publisher + subscriber. `try_recv` only (no futex syscall — `WAITERS = 0`).

> Numbers are directional. Re-run on your hardware before making decisions.
> `cargo bench` — all benchmarks
> `cargo bench -- head_to_head` — iceoryx2 comparison only (Unix, requires iceoryx2 dev-dep)

---

## Apple M1 Pro · macOS · rustc 1.92.0

### Transport overhead (8 B write, varying backing buffer)

Proves O(1): latency is flat regardless of how large the backing block is.

| Backing buffer | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 64 B | **52 ns** | 189 ns | 3.6× |
| 4 KB | **52 ns** | 189 ns | 3.6× |
| 64 KB | **52 ns** | 189 ns | 3.7× |
| 256 KB | **52 ns** | 188 ns | 3.6× |
| 1 MB | **55 ns** | 188 ns | 3.4× |

### End-to-end with full payload (loan → memcpy → publish → recv → deref)

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **55 ns** | 189 ns | 3.5× |
| 1 KB | **77 ns** | 210 ns | 2.7× |
| 64 KB | 1.27 µs | 1.35 µs | 1.1× |
| 256 KB | 5.10 µs | 5.20 µs | ~1× |
| 1 MB | 23.9 µs | 23.5 µs | ~1× |

### Throughput (born-in-SHM write)

| Payload | throughput |
|---|---|
| 64 KB | **48.8 GiB/s** |
| 1 MB | **43.3 GiB/s** |

---

## Intel i7-10700KF @ 3.80 GHz · Linux 6.8 · rustc stable

### Transport overhead

| Backing buffer | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 64 B | **64 ns** | 237 ns | 3.7× |
| 64 KB | **64 ns** | 238 ns | 3.7× |
| 1 MB | **65 ns** | 245 ns | 3.8× |

### End-to-end with full payload

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **63 ns** | 237 ns | 3.8× |
| 1 KB | 74 ns | 247 ns | 3.3× |
| 64 KB | 1.44 µs | 1.35 µs | ~1× |
| 256 KB | 6.75 µs | 6.92 µs | ~1× |
| 1 MB | 30.1 µs | 30.8 µs | ~1× |

### Throughput

| Payload | throughput |
|---|---|
| 64 KB | **46.7 GiB/s** |
| 1 MB | **31.4 GiB/s** |

---

## Reading the numbers

**The crossbar advantage lives below 64 KB.** The speed difference comes from a lighter path — no service discovery, no POSIX configuration layer, straight to atomics. Above 64 KB, `memcpy` dominates and both frameworks are equal.

**Born-in-SHM avoids the memcpy entirely.** If the publisher writes directly into the loaned block (no intermediate copy), the latency at any payload size is just the transport overhead: ~52–65 ns depending on hardware. The throughput numbers above measure the memcpy cost separately.

**Throughput is memory-bandwidth-bound.** 48 GiB/s is close to M1 Pro's measured memory bandwidth (~77 GB/s with THP) — the bulk of time is writing the payload, not the framework.
