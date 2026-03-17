# Benchmarks

All numbers come from [Criterion](https://github.com/bheisler/criterion.rs)
benchmarks in `benches/pubsub.rs` (shared-memory pub/sub + iceoryx2 head-to-head).

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

## Shared-memory pub/sub (`crossbar`)

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
# All shared-memory benchmarks (+ iceoryx2 head-to-head, Unix only)
cargo bench

# Only head-to-head comparison
cargo bench -- "head_to_head"
```

Criterion reports are written to `target/criterion/`.
