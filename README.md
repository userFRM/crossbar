# crossbar

[![Crates.io](https://img.shields.io/crates/v/crossbar.svg)](https://crates.io/crates/crossbar)
[![docs.rs](https://docs.rs/crossbar/badge.svg)](https://docs.rs/crossbar)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![CI](https://github.com/userFRM/crossbar/actions/workflows/ci.yml/badge.svg)](https://github.com/userFRM/crossbar/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.87-blue.svg)](https://www.rust-lang.org)

**Zero-copy pub/sub over shared memory. URI-addressed. O(1) transfer at any payload size.**

---

## How it works

A crossbar publisher creates a memory-mapped file (on Linux this lives in `/dev/shm`, a RAM-backed filesystem). The file contains four regions laid out contiguously:

1. **Header** -- magic number, version, configuration.
2. **Topic table** -- fixed-size entries, one per registered URI (`/tick/AAPL`, `/sensor/temp`, ...). Each entry owns a small lock-free ring buffer of block indices.
3. **Block pool** -- a Treiber stack of fixed-size blocks. This is where payload data lives.
4. **Ring buffers** -- per-topic rings that carry 8-byte block descriptors from publisher to subscribers.

**Publishing** works like this: the publisher allocates a block from the pool, writes data into it, then puts the block's index into the topic's ring. That ring write is 8 bytes -- O(1) regardless of whether the payload is 8 bytes or 8 megabytes.

**Subscribing** is the inverse: the subscriber reads the block index from the ring, then gets a zero-copy reference directly into the shared memory block. No deserialization, no buffer copies. The block stays alive via atomic refcounting until every subscriber drops its reference.

**Cross-process**: a second process opens the same memory-mapped file, maps it into its own address space, and reads the same bytes. There is no kernel involvement on the data path -- no `read()`, no `write()`, no `sendmsg()`. The only system call is the initial `mmap`.

The result: **55 ns end-to-end latency at 8 bytes, memcpy-bound convergence at 64 KB+, zero copies at any size.**

---

## Quick start

**Process A -- publisher:**

```rust
use crossbar::*;

let mut pub_ = Publisher::create("prices", Config::default())?;
let topic = pub_.register("/tick/AAPL")?;

let mut loan = pub_.loan(&topic)?;
loan.set_data(b"42.50")?;
loan.publish();
```

**Process B -- subscriber:**

```rust
use crossbar::*;

let sub = Subscriber::connect("prices")?;
let stream = sub.subscribe("/tick/AAPL")?;

if let Some(sample) = stream.try_recv() {
    println!("{}", std::str::from_utf8(&sample).unwrap());
}
```

---

## Features

| Feature | Description |
|---|---|
| **Pool + ring pub/sub** | `Publisher` / `Subscriber` -- lock-free block pool with per-topic ring buffers. Multi-publisher support via atomic sequence claiming. |
| **Typed pub/sub** | `Pod` trait for `Copy + 'static` structs. `loan_typed` / `try_recv_typed` give zero-copy struct reads with no `unsafe` at the call site. |
| **Pinned mode** | `loan_pinned` / `try_recv_pinned` -- reuses the same block every iteration. No allocation, no refcount, no ring. Lowest possible latency (35 ns at 8 B). |
| **PodBus SPMC broadcast** | Seqlock-based single-value ring. O(1) publish regardless of subscriber count. 3.1 ns at 8 B, scales to 1.14B msg/s at 16 subscribers. |
| **Bounded backpressure** | `PodBus::create_bounded` -- lossless mode where the publisher blocks when the slowest subscriber falls behind. |
| **Bidirectional Channel** | `Channel::listen` / `Channel::connect` -- wraps two pub/sub regions into a TCP-like request/response pair. |
| **Service discovery** | `Registry` + `discover()` -- global SHM registry with automatic heartbeat. Wildcard pattern matching (`/tick/*`). |
| **Structured payloads** | `write_structured` / `read_header` / `read_array` -- header + array layout with zero-copy reads. |
| **Subscriber counting** | `publisher.subscriber_count(&topic)` -- atomic count of live subscribers per topic. |
| **C and C++ bindings** | `crossbar.h` (C FFI) and `crossbar.hpp` (C++20 RAII wrapper). Build with `--features ffi`. |

---

## Performance

### Cross-process latency (the real IPC number)

Measured with separate publisher and subscriber **processes**, core-pinned, 8B payload,
paced at 1µs/msg, busy-spin subscriber. 5 runs × 500K samples = 2.5M measurements.

| Metric | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| **p50** | **172 ns ± 6** | 240 ns ± 6 | **1.40x** |
| min | 114 ns | 159 ns | 1.40x |
| p99 | 205 ns | 325 ns | 1.59x |

Reproduce: `cargo build --release --example cross_process_iceoryx2 && target/release/examples/cross_process_iceoryx2 run`

### Same-process latency (Criterion, cache-hot)

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **55 ns** | 230 ns | **4.2x** |
| 1 KB | **67 ns** | 239 ns | **3.6x** |
| 64 KB | 1.47 us | 1.32 us | 0.9x |
| 1 MB | 30.7 us | 29.8 us | ~1x |

### Pinned mode (same buffer every iteration)

| Payload | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **35 ns** | 229 ns | **6.5x** |
| 1 KB | **45 ns** | 237 ns | **5.3x** |
| 64 KB | **1.10 us** | 1.32 us | **1.2x** |
| 1 MB | 18.4 us | 18.3 us | ~1x |

### PodBus SPMC broadcast (publish throughput vs subscriber count)

| Subscribers | publish latency | total fanout throughput |
|---|---|---|
| 0 | 2.5 ns | -- |
| 8 | 13 ns | 691M msg/s |
| 16 | 24 ns | **1.14B msg/s** |

Same-process Criterion benchmarks measure peak throughput. Cross-process latency is the real
IPC number. See [BENCHMARKS.md](BENCHMARKS.md) for full methodology and caveats.

---

## When to use crossbar

- **Market data** -- ticks, quotes, greeks, option chains at sub-microsecond latency
- **Sensor pipelines** -- IMU, lidar, camera frames between processes
- **Telemetry** -- metrics, traces, logs from hot paths without syscall overhead
- **Game state** -- entity updates, physics frames, render state between threads/processes
- **Any low-latency local IPC** -- when you need shared-memory speed with a topic-based API

## When NOT to use crossbar

- **Networked messaging** -- crossbar is local-only (same machine). Use ZeroMQ, NATS, or gRPC for network transport.
- **Safety-critical systems** -- no DO-178C / IEC 61508 certification. Use iceoryx2 or a certified middleware.
- **Complex service mesh** -- no routing, load balancing, or request tracing. Use a proper service mesh.
- **Persistent messaging** -- messages live in RAM and vanish on reboot. Use Kafka, Pulsar, or a database.

---

## Installation

```toml
[dependencies]
crossbar = "1"
```

---

## Project layout

```
src/
  lib.rs                 Crate root, public re-exports
  pod.rs                 Pod trait -- marker for safe zero-copy SHM reads
  error.rs               Error types
  wait.rs                WaitStrategy (BusySpin / YieldSpin / BackoffSpin / Adaptive)
  ffi.rs                 C FFI bindings (behind "ffi" feature)

  protocol/              Core protocol -- pure atomics, no OS calls
    layout.rs            SHM layout constants and offset helpers
    config.rs            Config
    region.rs            Region -- Treiber stack, seqlock, refcount

  platform/              Platform layer -- mmap, futex, file I/O
    mmap.rs              RawMmap (MADV_HUGEPAGE on Linux)
    notify.rs            futex (Linux) / WaitOnAddress (Windows) / WFE (aarch64)
    shm.rs               Publisher, Subscriber
    subscription.rs      Stream, Sample, TypedSample, PinnedGuard
    loan.rs              Loan, TypedLoan, PinnedLoan, Topic
    channel.rs           Channel -- bidirectional channel
    pod_bus.rs           PodBus, BusSubscriber -- seqlock SPMC broadcast
    registry.rs          Registry, DiscoveredTopic -- service discovery

include/
  crossbar.h             C header for FFI consumers
  crossbar.hpp           C++20 RAII wrapper over crossbar.h

bindings/
  python/                PyO3-based Python bindings

tests/
  pubsub.rs              Pool+ring integration tests
  typed_pubsub.rs        Typed pub/sub tests
  structured.rs          Structured payload (header + array) tests
  channel.rs             Bidirectional channel tests
  multi_publisher.rs     Multi-publisher tests
  pinned.rs              Pinned mode tests
  pod_bus.rs             PodBus SPMC tests
  discovery.rs           Service discovery tests
  concurrent.rs          Concurrency stress tests
  error_paths.rs         Error path coverage
  io_write.rs            io::Write impl tests
  ffi_smoke.rs           C FFI smoke test

benches/
  pubsub.rs              Criterion benchmarks (+ iceoryx2 head-to-head, Unix)
  pod_bus.rs             PodBus latency and fanout benchmarks
  spmc_contention.rs     SPMC contention: publish throughput vs subscriber count

examples/
  publisher.rs           Cross-process latency benchmark -- publisher side
  subscriber.rs          Cross-process latency benchmark -- subscriber side
  pinned_throughput.rs   Pinned mode throughput example
```

---

## License

Apache-2.0.
