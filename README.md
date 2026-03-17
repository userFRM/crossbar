# crossbar

[![CI](https://github.com/userFRM/crossbar/actions/workflows/ci.yml/badge.svg)](https://github.com/userFRM/crossbar/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/crossbar.svg)](https://crates.io/crates/crossbar)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE-MIT)

**Zero-copy pub/sub over shared memory. URI-addressed. ~55 ns end-to-end.**

Allocates blocks from a lock-free pool, writes data into the mmap'd region, and transfers ownership via an 8-byte descriptor. Subscribers read directly from shared memory — no copy, no deserialization.

---

## When to use crossbar

- High-frequency small messages: market data ticks, sensor readings, telemetry, game state
- Rust-native multi-process pipelines where latency compounds (10 000+ msg/s)
- Topics that need to be discovered at runtime by URI, not wired at compile time
- You want one crate with no heavy dependencies

## When not to use crossbar

- You need C or C++ consumers — use [iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2)
- You need request/response — use a channel or iceoryx2
- Payload > 64 KB and you're copying into the block — both frameworks are memcpy-bound at that point and latency is equal

---

## Installation

```toml
[dependencies]
crossbar = "0.2"
```

---

## Quick start

### Byte-oriented

**Publisher** — write any bytes into shared memory:

```rust
use crossbar::*;

let mut pub_ = ShmPublisher::create("market", PubSubConfig::default())?;
let topic = pub_.register("/prices/AAPL")?;

let mut loan = pub_.loan(&topic);
loan.set_data(b"42.50");
loan.publish(); // O(1) — writes 8 bytes to ring
```

**Subscriber** — read in-place, zero copies, no `unsafe`:

```rust
use crossbar::*;

let sub = ShmSubscriber::connect("market")?;
let stream = sub.subscribe("/prices/AAPL")?;

if let Some(guard) = stream.try_recv() {
    println!("{}", std::str::from_utf8(&guard).unwrap());
} // guard drops → block freed back to pool
```

### Typed

Any `Copy + 'static` struct where every bit pattern is valid can implement `Pod` for direct zero-copy reads:

```rust
use crossbar::*;

#[derive(Clone, Copy)]
#[repr(C)]
struct Tick { price: f64, volume: u64 }

unsafe impl Pod for Tick {}

// Publisher
let mut pub_ = ShmPublisher::create("market", PubSubConfig::default())?;
let topic = pub_.register_typed::<Tick>("/prices/AAPL")?;
let mut loan = pub_.loan_typed::<Tick>(&topic);
*loan.as_mut() = Tick { price: 42.50, volume: 1000 };
loan.publish();

// Subscriber
let sub = ShmSubscriber::connect("market")?;
let stream = sub.subscribe("/prices/AAPL")?;

if let Some(guard) = stream.try_recv_typed::<Tick>() {
    println!("${:.2} × {}", guard.price, guard.volume);
}
```

### Blocking receive

```rust
// Default: three-phase spin → yield → futex/WFE
let guard = stream.recv()?;

// Or pick a strategy
let guard = stream.recv_with(WaitStrategy::BusySpin)?;
```

### Born-in-SHM (zero-copy publish)

Write directly into the pool block — no intermediate buffer, no copy at any payload size:

```rust
let mut loan = pub_.loan(&topic);
let buf = loan.as_mut_slice();
// write directly into shared memory
encode_frame(&mut buf[..frame_len]);
loan.set_len(frame_len);
loan.publish();
```

---

## Performance

All measurements: Criterion, same-process publisher + subscriber, `try_recv` (no futex).

### Apple M1 Pro · macOS · rustc 1.92

| | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B (transport overhead) | **52 ns** | 189 ns | **3.6×** |
| 1 KB | 77 ns | 210 ns | 2.7× |
| 64 KB | 1.27 µs | 1.35 µs | 1.1× |
| 256 KB | 5.10 µs | 5.20 µs | ~1× |
| 1 MB | 23.9 µs | 23.5 µs | ~1× |

### Intel i7-10700KF · Linux 6.8 · rustc stable

| | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B (transport overhead) | **57 ns** | 231 ns | **4.1×** |
| 1 KB | 65 ns | 231 ns | 3.6× |
| 64 KB | 1.40 µs | 1.35 µs | ~1× |
| 1 MB | 30.8 µs | 32.0 µs | ~1× |

**The win is in the overhead.** At small payloads crossbar's lighter path (no service discovery, no POSIX config layer) is 3.5–4× faster. At 64 KB+ both frameworks are memcpy-bound and converge. The 8-byte descriptor is always O(1) — payload latency scales with how long you take to write into the block.

Reproduce: `cargo bench -- head_to_head` (requires `iceoryx2` dev-dep, Unix only).

---

## Configuration

```rust
PubSubConfig {
    max_topics:          16,    // concurrent topics
    block_count:        256,    // pool blocks
    block_size:       65536,    // bytes per block (usable: block_size - 8)
    ring_depth:           8,    // samples before overwrite
    heartbeat_interval: 100ms,  // liveness signal period
    stale_timeout:        5s,   // publisher dead after this
}
```

The pool is a Treiber stack — lock-free allocation at any payload size. Blocks are refcounted; a subscriber holding a `SampleGuard` keeps the block alive.

---

## Project layout

```
src/
  lib.rs                 Crate root (#![no_std], feature gates)
  pod.rs                 Pod trait — marker for safe zero-copy SHM reads
  error.rs               IpcError
  wait.rs                WaitStrategy (BusySpin / YieldSpin / BackoffSpin / Adaptive)

  protocol/              no_std core — pure atomics, no OS calls
    layout.rs            SHM layout constants and offset helpers
    config.rs            PubSubConfig
    region.rs            Region — Treiber stack, seqlock, refcount

  platform/              std only — mmap, futex, file I/O
    mmap.rs              RawMmap (MADV_HUGEPAGE on Linux)
    notify.rs            futex (Linux) / WaitOnAddress (Windows) / WFE (aarch64)
    shm.rs               ShmPublisher, ShmSubscriber
    subscription.rs      Subscription, SampleGuard, TypedSampleGuard
    loan.rs              ShmLoan, TypedShmLoan, TopicHandle

tests/
  pubsub.rs              Integration tests
  typed_pubsub.rs        Typed pub/sub integration tests
benches/pubsub.rs        Criterion benchmarks (+ iceoryx2 head-to-head, Unix)
examples/
  publisher.rs           Cross-process latency benchmark — publisher side
  subscriber.rs          Cross-process latency benchmark — subscriber side
scripts/
  gen_benchmark_chart.py Regenerate assets/benchmark_comparison.svg
```

---

## no_std

The protocol core (`src/protocol/`, `src/pod.rs`, `src/wait.rs`, `src/error.rs`) is `no_std` + `alloc`. The platform layer (mmap, futex, file I/O) requires `std` and is gated behind `features = ["std"]` (the default).

Requirement: `target_has_atomic = "64"` — the ABA-safe Treiber stack uses 64-bit CAS.

```toml
# no_std + alloc only (protocol core, no ShmPublisher/ShmSubscriber)
crossbar = { version = "0.2", default-features = false }

# std (default — includes everything)
crossbar = "0.2"
```

---

## License

MIT OR Apache-2.0 — your choice.
