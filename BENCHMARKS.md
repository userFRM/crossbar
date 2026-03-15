# Benchmarks

All numbers come from [Criterion](https://github.com/bheisler/criterion.rs)
benchmarks in [`benches/transport.rs`](benches/transport.rs).

The refactor removed SHM RPC from the crate, so this document now focuses on
the two remaining benchmark families:

- **In-process dispatch** through `InProcessClient`
- **Pub/sub** through `ShmPublisher` / `ShmSubscriber`

## Environment

| | |
|---|---|
| **CPU** | Intel Core i7-10700KF @ 3.80 GHz |
| **OS** | Ubuntu, kernel 6.8 |
| **Profile** | `release` |

> [!IMPORTANT]
> Treat these as directional numbers from one machine. Re-run on your hardware
> before making decisions.

## What is measured

- `dispatch/*`: direct URI routing through `InProcessClient`
- `inproc/*`: endpoint-style request/response in one process
- `pubsub_transport_only/*`: descriptor publication with minimal payload
- `pubsub_o1/*`: publish + receive across a shared-memory ring
- `throughput_pubsub/*`: sustained payload movement through the pool

Publisher and subscriber share the same mmap region during the benchmark. That
isolates transport cost from scheduler noise, but real cross-process results
will be somewhat higher.

---

## Current headline numbers

| Path | Benchmark | Latency |
|---|---|---|
| In-process | `/health` | ~143 ns |
| Pub/sub | `publish()` + `try_recv()` | ~67 ns |
| Pub/sub | `publish_silent()` + `try_recv()` | ~65 ns |

These are not apples-to-apples semantic comparisons:

- In-process dispatch builds `Request` / `Response` values and runs URI routing.
- Pub/sub moves raw bytes by publishing an 8-byte descriptor.

Pub/sub is faster because it does less work on the hot path.

---

## Payload scaling

The pool pub/sub transfer stays O(1) in descriptor cost, but the payload still
has to be written into the shared-memory block. That means end-to-end latency
rises with payload size even though the publication step itself remains constant.

| Payload | What grows |
|---|---|
| 8 B | almost pure transport cost |
| 64 KB | write cost dominates more |
| 1 MB | payload write cost dominates heavily |

The same distinction applies when reading benchmark graphs: the descriptor path
is constant-sized, the memory write is not.

---

## Throughput

The `throughput_pubsub` group measures sustained movement of large payloads
through the shared-memory pool.

- 64 KB and 1 MB are benchmarked today.
- The read side stays zero-copy through `SampleGuard`.
- The write side is still bounded by memory bandwidth because data must be
  copied into the loaned SHM block.

---

## Reproducing

```sh
# All current benchmarks
cargo bench --features shm

# In-process only
cargo bench --features shm -- "dispatch|inproc"

# Pub/sub only
cargo bench --features shm -- "pubsub|throughput_pubsub"
```

Criterion reports are written to `target/criterion/`.
