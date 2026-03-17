# Benchmarks

All numbers come from [Criterion](https://github.com/bheisler/criterion.rs)
benchmarks in the workspace:

- `crossbar-inproc/benches/bus.rs` — in-process `Bus<T>` pub/sub
- `crossbar-ipc/benches/pubsub.rs` — shared-memory pub/sub (+ iceoryx2 head-to-head)

## Environment

| | |
|---|---|
| **CPU** | Intel Core i7-10700KF @ 3.80 GHz |
| **OS** | Ubuntu, kernel 6.8 |
| **Profile** | `release` |

> [!IMPORTANT]
> Treat these as directional numbers from one machine. Re-run on your hardware
> before making decisions.

---

## In-process pub/sub (`crossbar-inproc`)

| Benchmark | Latency |
|---|---|
| 1-subscriber roundtrip (publish + try_recv) | **57 ns** |
| Fan-out to 0 subscribers | 37 ns |
| Fan-out to 1 subscriber | 57 ns |
| Fan-out to 2 subscribers | 75 ns |
| Fan-out to 5 subscribers | 132 ns |
| Fan-out to 10 subscribers | **224 ns** |
| Publish only (1 sub, no recv) | 61 ns |
| TopicHandle (pre-resolved) | 57 ns |
| bus.publish() (HashMap lookup) | 111 ns |
| Arc::new + Arc::clone + drop | 26 ns |
| Subscribe + unsubscribe | 644 ns |
| try_recv (empty ring) | 1 ns |

The lock-free SPSC ring with CAS-based overflow adds ~19 ns per subscriber
during fan-out (vs ~37 ns base overhead). The ring uses cache-line padded
head/tail indices to prevent false sharing across cores.

---

## Shared-memory pub/sub (`crossbar-ipc`)

### Latency

| Mode | Latency |
|---|---|
| `publish()` + `try_recv()` (smart wake) | **58 ns** |
| `publish_silent()` + `try_recv()` | **59 ns** |
| O(1) transport 8B | **60 ns** |

### Throughput

| Payload | Throughput |
|---|---|
| 64 KB | **46.7 GiB/s** |
| 1 MB | **31.4 GiB/s** |

### Head-to-head vs iceoryx2

| Payload | crossbar | iceoryx2 | Ratio |
|---|---|---|---|
| O(1) transport 8B | **57 ns** | 231 ns | **4.1x** |
| E2E 8B | **55 ns** | 231 ns | **4.2x** |
| E2E 1 KB | 65 ns | 231 ns | 3.6x |
| E2E 64 KB | 1.40 us | 1.35 us | ~1x |
| E2E 256 KB | 7.02 us | 6.80 us | ~1x |
| E2E 1 MB | 30.8 us | 32.0 us | ~1x |

At small sizes, crossbar's lower overhead wins. At 64 KB+, both converge
because `memcpy` dominates.

### Payload scaling

The transport stays O(1) in descriptor cost, but the payload still
has to be written into the shared-memory block. That means end-to-end latency
rises with payload size even though the publication step itself remains constant.

---

## Reproducing

```sh
# In-process benchmarks
cargo bench -p crossbar-inproc

# Shared-memory benchmarks (+ iceoryx2 head-to-head, Unix only)
cargo bench -p crossbar-ipc

# Only head-to-head comparison
cargo bench -p crossbar-ipc -- "head_to_head"
```

Criterion reports are written to `target/criterion/`.
