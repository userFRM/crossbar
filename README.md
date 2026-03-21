# crossbar

[![Crates.io](https://img.shields.io/crates/v/crossbar.svg)](https://crates.io/crates/crossbar)
[![docs.rs](https://docs.rs/crossbar/badge.svg)](https://docs.rs/crossbar)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![no_std](https://img.shields.io/badge/no__std-compatible-brightgreen.svg)](https://docs.rs/crossbar)
[![CI](https://github.com/userFRM/crossbar/actions/workflows/ci.yml/badge.svg)](https://github.com/userFRM/crossbar/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.87-blue.svg)](https://www.rust-lang.org)

**Zero-copy pub/sub over shared memory. URI-addressed. O(1) transfer at any payload size.**

Transfers an 8-byte descriptor through a lock-free ring — O(1) regardless of payload. Subscribers read directly from shared memory. No copy, no serialization, no service discovery layer.

---

## When to use crossbar

- High-frequency small messages: market data ticks, sensor readings, telemetry, game state
- Rust-native multi-process pipelines where latency compounds (10 000+ msg/s)
- Topics that need to be discovered at runtime by URI, not wired at compile time
- You want one crate with no heavy dependencies

## When not to use crossbar

- Payload > 64 KB and you're copying into the block — both frameworks are memcpy-bound at that point and latency is equal

---

## Installation

```toml
[dependencies]
crossbar = "0.4"
```

---

## Quick start

### Byte-oriented

**Publisher** — write any bytes into shared memory:

```rust
use crossbar::*;

let mut pub_ = ShmPublisher::create("market", PubSubConfig::default())?;
let topic = pub_.register("/prices/AAPL")?;

let mut loan = pub_.loan(&topic).unwrap();
loan.set_data(b"42.50").unwrap();
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
let mut loan = pub_.loan_typed::<Tick>(&topic).unwrap();
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

### Multi-publisher

Multiple publishers can share the same region — one creates, others join:

```rust
use crossbar::*;

// Process A — creates the region
let mut pub_a = ShmPublisher::create("market", PubSubConfig::default())?;
let topic_a = pub_a.register("/prices/AAPL")?;

// Process B — joins the existing region
let mut pub_b = ShmPublisher::open("market")?;
let topic_b = pub_b.register("/prices/GOOG")?;
// Or publish to the same topic as pub_a:
let topic_b2 = pub_b.register("/prices/AAPL")?;
```

Sequence numbers are claimed atomically via `fetch_add`. CAS-based ring slot locking prevents corruption when two publishers write to the same slot. Subscribers scan the ring window to handle out-of-order commits.

### Bidirectional channel

`ShmChannel` wraps two pub/sub regions into a TCP-like pair — one side listens, the other connects:

```rust
use crossbar::*;
use std::time::Duration;

// Process A (server)
let mut srv = ShmChannel::listen("rpc", PubSubConfig::default(),
    Duration::from_secs(30))?;

// Process B (client)
let mut cli = ShmChannel::connect("rpc", PubSubConfig::default(),
    Duration::from_secs(5))?;

cli.send(b"request")?;
let msg = srv.recv()?;
// ... process and respond
srv.send(b"response")?;
let reply = cli.recv()?;
```

### Born-in-SHM (zero-copy publish)

Write directly into the pool block — no intermediate buffer, no copy at any payload size:

```rust
let mut loan = pub_.loan(&topic).unwrap();
let buf = loan.as_mut_slice();
// write directly into shared memory
encode_frame(&mut buf[..frame_len]);
loan.set_len(frame_len).unwrap();
loan.publish();
```

---

## Performance (v0.6.0)

All measurements: Criterion, same-process publisher + subscriber, `try_recv` (no futex).
Same-process benchmarks; cross-process latency is typically 2-5x higher.

### Intel i7-10700KF · Linux 6.8 · rustc 1.87

| | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B (transport overhead) | **55 ns** | 230 ns | **4.2×** |
| 1 KB | **67 ns** | 239 ns | **3.6×** |
| 64 KB | 1.47 µs | 1.32 µs | 0.9× |
| 1 MB | 30.7 µs | 29.8 µs | ~1× |

### Apple M1 Pro · macOS · rustc 1.92 (v0.3.0, pre-optimization)

| | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B (transport overhead) | **52 ns** | 189 ns | **3.6×** |
| 1 KB | 77 ns | 210 ns | 2.7× |
| 64 KB | 1.27 µs | 1.35 µs | 1.1× |
| 1 MB | 23.9 µs | 23.5 µs | ~1× |

**The win is in the overhead.** At small payloads crossbar's lighter path (no service discovery, no POSIX config layer) is 4.2x faster. At 64 KB+ both frameworks are memcpy-bound and converge — iceoryx2 is slightly faster at large payloads. The 8-byte descriptor is always O(1) — payload latency scales with how long you take to write into the block.

### Pinned mode (latest-value, same buffer every iteration)

| | crossbar | iceoryx2 | speedup |
|---|---|---|---|
| 8 B | **35 ns** | 229 ns | **6.5×** |
| 1 KB | **45 ns** | 237 ns | **5.3×** |
| 64 KB | **1.10 µs** | 1.32 µs | **1.2×** |
| 1 MB | 18.4 µs | 18.3 µs | ~1× |

Pinned mode (`loan_pinned` / `try_recv_pinned`) reuses the same block every iteration — no allocation, no refcount, no ring. Safe API with CAS-based reader/writer exclusion. Best for market data, sensors, telemetry, game state.

Reproduce: `cargo bench -- head_to_head` (requires `iceoryx2` dev-dep, Unix only).

See [BENCHMARKS.md](BENCHMARKS.md) for full numbers, PodBus results, and methodology caveats.

---

## C / C++ FFI

Build the shared library and link against it from C or C++:

```sh
cargo build --release --features ffi --crate-type cdylib
# produces target/release/libcrossbar.so (Linux), .dylib (macOS), .dll (Windows)
```

Include `include/crossbar.h`:

```c
#include "crossbar.h"

// Publisher
crossbar_publisher_t* pub = crossbar_publisher_create("market", NULL);
crossbar_topic_t topic = crossbar_publisher_register(pub, "/prices/AAPL");
crossbar_publish(pub, topic, data, data_len);

// Subscriber
crossbar_subscriber_t* sub = crossbar_subscriber_connect("market");
crossbar_subscription_t* stream = crossbar_subscriber_subscribe(sub, "/prices/AAPL");
crossbar_sample_t* sample = crossbar_try_recv(stream);  // allocates
if (sample) {
    const uint8_t* data = crossbar_sample_data(sample);
    size_t len = crossbar_sample_len(sample);
    // zero-copy read — data points directly into SHM
    crossbar_sample_free(sample);
}

// Or zero-allocation hot path:
crossbar_sample_t sample;
if (crossbar_try_recv_into(stream, &sample)) {
    const uint8_t* data = crossbar_sample_data(&sample);
    crossbar_sample_free(&sample);
}

// Bidirectional channel
crossbar_channel_t* ch = crossbar_channel_connect("rpc", NULL, 5000);
crossbar_channel_send(ch, msg, msg_len);
crossbar_sample_t* reply = crossbar_channel_recv(ch);
```

---

## Configuration

```rust
PubSubConfig {
    max_topics:          16,    // concurrent topics
    block_count:        256,    // pool blocks
    block_size:       65536,    // bytes per block (usable: block_size - 8)
    ring_depth:           8,    // samples before overwrite (must be power of 2)
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
  ffi.rs                 C FFI bindings (behind "ffi" feature)

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
    channel.rs           ShmChannel — bidirectional channel

include/
  crossbar.h             C/C++ header for FFI consumers
tests/
  pubsub.rs              Integration tests
  typed_pubsub.rs        Typed pub/sub integration tests
  channel.rs             Bidirectional channel tests
  multi_publisher.rs     Multi-publisher tests
benches/pubsub.rs        Criterion benchmarks (+ iceoryx2 head-to-head, Unix)
examples/
  publisher.rs           Cross-process latency benchmark — publisher side
  subscriber.rs          Cross-process latency benchmark — subscriber side
```

---

## no_std

The protocol core (`src/protocol/`, `src/pod.rs`, `src/wait.rs`, `src/error.rs`) is `no_std` + `alloc`. The platform layer (mmap, futex, file I/O) requires `std` and is gated behind `features = ["std"]` (the default).

Requirement: `target_has_atomic = "64"` — the ABA-safe Treiber stack uses 64-bit CAS.

```toml
# no_std + alloc only (protocol core, no ShmPublisher/ShmSubscriber)
crossbar = { version = "0.4", default-features = false }

# std (default — includes everything)
crossbar = "0.4"
```

---

## License

Apache-2.0.
