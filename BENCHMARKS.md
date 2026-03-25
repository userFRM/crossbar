# Benchmarks — crossbar v1.0

All numbers from [Criterion](https://github.com/bheisler/criterion.rs) benchmarks
(`benches/pubsub.rs`, `benches/pod_bus.rs`, `benches/spmc_contention.rs`).
Same-process publisher + subscriber. `try_recv` only (no futex syscall).

> Numbers are directional. Re-run on your hardware before making decisions.
> `cargo bench` — all benchmarks
> `cargo bench -- head_to_head` — iceoryx2 comparison only (Unix, requires iceoryx2 dev-dep)

---

## Intel i7-10700KF @ 3.80 GHz · Linux 6.8 · rustc 1.94

### Pool+ring pub/sub

#### Transport overhead (8 B payload)

| Mode | Latency |
|---|---|
| `publish` (smart wake) | **58 ns** |
| `publish_silent` (no wake) | **58 ns** |

#### O(1) transfer (loan + memcpy + publish + recv + deref)

| Payload | Latency |
|---|---|
| 8 B | **58 ns** |
| 64 KB | 1.37 µs |
| 1 MB | 30.6 µs |

#### Throughput

| Payload | Throughput |
|---|---|
| 64 KB | **44.5 GiB/s** |
| 1 MB | **31.8 GiB/s** |

---

### Pinned mode (born-in-SHM)

`loan_pinned` / `try_recv_pinned` — no ring rotation, no alloc, no refcount.
Same buffer reused every iteration.

| Payload | crossbar | iceoryx2 | Speedup |
|---|---|---|---|
| 8 B | **35 ns** | 228 ns | **6.5x** |
| 1 KB | **44 ns** | 238 ns | **5.4x** |
| 64 KB | **1.09 µs** | 1.32 µs | **1.2x** |
| 1 MB | 18.0 µs | 19.3 µs | ~1x |

---

### PodBus (seqlock-based single-value broadcast)

#### Single-subscriber latency

| Payload | Latency |
|---|---|
| 8 B | **7.2 ns** |
| 64 B | 37.7 ns |
| 256 B | 129 ns |
| 1 KB | 494 ns |

#### 10-subscriber fanout (8 B / u64)

| Metric | Value |
|---|---|
| Latency (publish + 10x try_recv) | **22.6 ns** |
| Throughput | **44.2 M msg/s** |

#### SPMC contention — publisher throughput vs subscriber count

100K publishes per iteration. Single-threaded: publisher writes, then all subscribers read sequentially.

**PodBus (seqlock):**

| Subscribers | Publish latency | Throughput |
|---|---|---|
| 0 | 4.7 ns | 211 M msg/s |
| 1 | 9.3 ns | 108 M msg/s |
| 2 | 14.4 ns | 69 M msg/s |
| 4 | 22.4 ns | 45 M msg/s |
| 8 | 23.8 ns | 42 M msg/s |
| 16 | 43.3 ns | 23 M msg/s |

**Pool+ring (for comparison):**

| Subscribers | Publish latency | Throughput |
|---|---|---|
| 0 | 42.0 ns | 23.8 M msg/s |
| 1 | 97.6 ns | 10.2 M msg/s |
| 4 | 133.6 ns | 7.5 M msg/s |
| 8 | 136.4 ns | 7.3 M msg/s |

PodBus is 5--10x faster than pool+ring for SPMC fanout because the seqlock avoids
per-subscriber ring slot management. The gap widens at low subscriber counts where
pool+ring's per-subscriber overhead dominates.

#### Per-subscriber read latency (PodBus)

Measures one subscriber's `try_recv` latency while N other subscribers also exist.

| Other subscribers | Read latency |
|---|---|
| 0 | 10.6 ns |
| 1 | 23.7 ns |
| 4 | 40.1 ns |
| 8 | 36.2 ns |
| 16 | 56.4 ns |

#### Total fanout throughput (PodBus, 100K messages)

Measures end-to-end: publish all messages, then each subscriber reads all messages.

| Subscribers | Wall time | Aggregate throughput |
|---|---|---|
| 1 | 902 µs | 111 M elem/s |
| 4 | 1.97 ms | 203 M elem/s |
| 8 | 2.35 ms | 341 M elem/s |
| 16 | 2.72 ms | 589 M elem/s |

Aggregate throughput scales nearly linearly: 16 subscribers deliver 5.3x the
aggregate of 1 subscriber, at only 3x the wall time.

---

### vs iceoryx2 — head-to-head

#### O(1) transport proof (8 B write, varying backing buffer size)

Proves O(1): latency is flat regardless of how large the backing block is.

| Backing buffer | crossbar | iceoryx2 | Speedup |
|---|---|---|---|
| 64 B | **53 ns** | 230 ns | 4.3x |
| 4 KB | **53 ns** | 228 ns | 4.3x |
| 64 KB | **53 ns** | 231 ns | 4.3x |
| 256 KB | **53 ns** | 227 ns | 4.3x |
| 1 MB | **54 ns** | 230 ns | 4.3x |

#### End-to-end with full payload (loan + memcpy + publish + recv + deref)

| Payload | crossbar | iceoryx2 | Speedup |
|---|---|---|---|
| 8 B | **52 ns** | 231 ns | **4.4x** |
| 1 KB | **71 ns** | 242 ns | **3.4x** |
| 64 KB | 1.39 µs | 1.34 µs | 0.96x |
| 256 KB | 7.00 µs | 6.82 µs | 0.97x |
| 1 MB | 31.8 µs | 32.1 µs | ~1x |

---

## Reading the numbers

**The crossbar advantage lives below 64 KB.** The speed difference comes from a
lighter path — no service discovery, no POSIX configuration layer, straight to
atomics. Above 64 KB, `memcpy` dominates and both frameworks converge. At 64 KB
iceoryx2 is slightly faster (0.96x) — the numbers are honest.

**Born-in-SHM avoids the memcpy entirely.** If the publisher writes directly into
the loaned block (no intermediate copy), the transport is ~35 ns for small payloads
regardless of payload size. Run `cargo bench -- born_in_shm` to see the head-to-head.

**Throughput is memory-bandwidth-bound.** 32--45 GiB/s is close to measured DDR4
memory bandwidth — the bulk of time is writing the payload, not the framework.

**PodBus vs pool+ring tradeoff.** PodBus (seqlock) is 5--10x faster for SPMC
fanout but overwrites the previous value — suitable for latest-value streaming
(quotes, sensor data). Pool+ring preserves history via per-subscriber ring buffers.

---

## Methodology & Caveats

All benchmarks use Criterion with same-process publisher and subscriber on a single thread.
`try_recv()` is used (no futex syscall). The ring buffer is cache-hot throughout.

**These numbers represent peak theoretical performance, not real-world IPC latency.**

### What the benchmarks hide

- **No cross-process cost**: Publisher and subscriber share the same address space.
  Real IPC crosses process boundaries, incurring TLB misses and cache coherency traffic.
  Expect 2--5x higher latency for small payloads in cross-process scenarios.

- **No cross-core cost**: Both run on the same core. Cross-core RFO stalls add
  30--60 ns per cache line transfer. The 4.3x advantage over iceoryx2 at 8B would
  shrink in multi-core deployments.

- **No blocking/wake cost**: `try_recv()` never blocks. The futex wake path
  (~170 ns per `futex_wake` syscall) is never exercised.

- **Cache-hot ring**: The ring buffer stays in L1/L2 throughout. Real workloads
  with competing processes will see cache evictions.

- **44.5 GiB/s throughput**: Approaches DDR4-3200 theoretical peak (51.2 GiB/s).
  This measures intra-core memcpy speed, not achievable IPC throughput.

- **PodBus 7.2 ns**: Measures seqlock overhead within a single core's L1 cache
  (~27 clock cycles). This is not an IPC metric — it is a thread-local data
  structure benchmark.

### What the benchmarks prove

- The library's internal overhead is minimal — the bottleneck is memcpy at 64 KB+.
- O(1) transfer is real: latency is flat regardless of backing buffer size.
- The lock-free algorithms (Treiber stack, seqlock, CAS ring) are correctly
  optimized for the hot path.
- PodBus SPMC scales: 16 subscribers deliver 589 M elem/s aggregate at 43 ns
  per publish — sub-linear overhead growth.

### Recommended: cross-process benchmarks

Use `examples/publisher` + `examples/subscriber` for true cross-process latency
measurements with `taskset` to pin to different physical cores.
