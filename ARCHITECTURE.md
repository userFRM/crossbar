# Architecture

Crossbar is a single Rust crate that moves data between processes at O(1) cost. It provides two communication patterns:

1. **Pool + Ring** (`Publisher` / `Subscriber`) -- zero-copy pub/sub over shared memory. An 8-byte descriptor (block index + data length) is transferred through a seqlock ring; the subscriber reads the payload in-place from the mmap region. Supports untyped and typed (`Pod`) payloads, multi-publisher, pinned publish, and bidirectional channels.

2. **PodBus** (`PodBus<T>` / `BusSubscriber<T>`) -- SPMC broadcast ring for `Pod` types, also backed by shared memory. Each publish writes a stamped copy of `T` into a cache-line-aligned ring slot. Supports unbounded (lossy) and bounded (lossless with backpressure) modes. Up to 64 concurrent subscribers across processes.

---

## SHM region layout (pool + ring)

One mmap region, divided into four logical areas:

```
+---------------------------+  offset 0
| Global Header (128 bytes) |
|   0x00 magic     [u8; 8]  |  "XBAR_ZC\0"
|   0x08 version   u32      |  3
|   0x0C max_topics u32     |
|   0x10 block_count u32    |
|   0x14 block_size  u32    |
|   0x18 ring_depth  u32    |  must be power of 2
|   0x20 pool_head  AtomicU64|  Treiber stack: pack(generation, index)
|   0x28 heartbeat  AtomicU64|  microseconds since UNIX epoch
|   0x30 pid        u64     |
|   0x38 stale_timeout_us u64|
+---------------------------+  offset 128
| Topic Entry 0 (128 bytes) |
|  1st cache line (hot):    |
|   0x00 ACTIVE    AtomicU32|  0=FREE, 2=INIT, 1=ACTIVE
|   0x04 (reserved) u32    |
|   0x08 WRITE_SEQ AtomicU64|  monotonic publish counter + futex wake addr
|   0x10 WAITERS   AtomicU32|  blocked subscriber count
|  2nd cache line (cold):   |
|   0x14 URI_LEN   u32      |
|   0x18 URI_HASH  u64      |  FNV-1a
|   0x20 URI       [u8; 64] |
|   0x60 TYPE_SIZE u32      |  size_of::<T>() for typed topics; 0 = untyped
|   0x64 PINNED_BLOCK u32   |  block index for pinned publish; NO_BLOCK = none
|   0x68 PINNED_READERS      |  AtomicU32 -- active pinned-read guard count
|   0x70 PINNED_SEQ AtomicU64|  packed (seq:32 | data_len:32) seqlock
|   0x78 SUBSCRIBER_COUNT    |  AtomicU32 -- live Stream count
+---------------------------+
| Topic Entry 1 ...         |
+---------------------------+  after all topic entries
| Ring 0 (ring_depth x 16B) |
|  per entry:               |
|   0x00 seq       AtomicU64|  seqlock: SEQ_WRITING (u64::MAX) = being written
|   0x08 block_idx u32      |  Relaxed (bracketed by seqlock)
|   0x0C data_len  u32      |  Relaxed (bracketed by seqlock)
+---------------------------+
| Ring 1 ...                |
+---------------------------+  after all rings
| Block Pool                |
|  per block:               |
|   0x00 next_free AtomicU32|  Treiber free-list link
|   0x04 refcount  AtomicU32|
|   0x08 data ...           |  user payload (block_size - 8 bytes)
+---------------------------+
```

Hot topic fields (`ACTIVE`, `WRITE_SEQ`, `WAITERS`) are packed into the first cache line of each topic entry to minimize cache misses on the subscriber polling path.

---

## PodBus SHM layout

```
+----------------------------------------------------------+
| Header (64 bytes)                                        |
|   [0..8)   magic: "XPOD_ZC\0"                           |
|   [8..12)  version: u32 (1 = unbounded, 3 = bounded)    |
|   [12..16) ring_size: u32                                |
|   [16..20) value_size: u32 (size_of::<T>())              |
|   [20..24) value_align: u32 (align_of::<T>())            |
|   [24..32) heartbeat_us: AtomicU64                       |
|   [32..40) write_seq: AtomicU64                          |
|   [40..48) publisher_pid: u64                            |
|   [48..52) watermark: u32 (0 for v1 unbounded)           |
|   [52..64) reserved                                      |
+----------------------------------------------------------+
| Subscriber slots (v3 only): 64 x 16 bytes each           |
|     [+0..+4)  pid: AtomicU32 (subscriber PID, 0 = free)  |
|     [+4..+8)  _padding: u32                              |
|     [+8..+16) state_cursor: AtomicU64                    |
|               high 2 bits = state:                       |
|                 0b00 = FREE                               |
|                 0b01 = ACTIVE (low 62 bits = cursor pos)  |
|                 0b10 = CLAIMING (low 62 = claim timestamp)|
|               low 62 bits = value                        |
+----------------------------------------------------------+
| Ring: ring_size x Slot<T>                                |
|   #[repr(C, align(64))] -- one slot per cache line       |
|   stamp: AtomicU64                                       |
|   data: UnsafeCell<T>                                    |
+----------------------------------------------------------+
```

In v1 (unbounded), the subscriber slots section is absent; the ring immediately follows the header.

---

## Lock-free protocols

### Treiber stack (block pool)

The block pool is a lock-free stack with a 64-bit `(generation, index)` head pointer. The generation counter prevents ABA: even if the same block is freed and reallocated between a load and a CAS, the generation mismatch causes the CAS to fail.

- **Alloc**: `compare_exchange_weak` on head to `(gen+1, next_free_of_head_block)`; reuse `Err(current)` on retry.
- **Free**: `compare_exchange_weak` on head to `(gen+1, freed_block)`; write old head index into freed block's link field.
- **Per-publisher cache**: Each publisher caches up to 8 blocks locally, refilling from the global stack when empty. This amortizes the CAS over multiple allocations (~87% cache hit rate).
- **Block recycling**: `commit_to_ring` stashes the freed block in `Region.last_freed`. The next `alloc_cached` grabs this block first -- it's still warm in L1/L2, eliminating RFO cache misses for large payloads.

### Seqlock (ring slots)

Ring data fields (`block_idx`, `data_len`) are read and written with `Relaxed` ordering. This is safe because the seqlock bracket provides the ordering: the `Release` store at seqlock close synchronizes-with the `Acquire` load at seqlock check.

- **Sentinel**: `SEQ_WRITING` (`u64::MAX`) marks a slot as "being written". Subscribers skip slots where `entry_seq == SEQ_WRITING`.
- **Write**: Publisher CAS's the slot from its current seq to `SEQ_WRITING`, writes `block_idx` and `data_len`, then stores the new seq with `Release`.
- **Read**: Subscriber loads seq (`Acquire`), reads data fields (`Relaxed`), then re-checks seq. If seq changed, the read is discarded and retried.
- **Single-publisher fast path**: When only one publisher exists, the seqlock acquisition uses a plain `Release` store instead of CAS, saving ~10-15 ns.

### PodBus seqlock stamps

PodBus uses a different stamp encoding (photon-ring protocol):

- `0` -- slot never written.
- `seq * 2 + 1` (odd) -- write in progress for sequence `seq`.
- `seq * 2 + 2` (even) -- write complete for sequence `seq`.

The publisher writes data between the odd and even stamp transitions. Subscribers snapshot the stamp, read via `read_volatile`, then verify the stamp is unchanged. If the stamp changed mid-read, the data was torn and the read is discarded.

### Pinned publish CAS sentinel

`loan_pinned()` atomically sets `PINNED_WRITER_ACTIVE` (`u32::MAX`) in `TE_PINNED_READERS` via `compare_exchange`. This prevents concurrent subscriber reads during the pinned write. Subscribers use a CAS loop that rejects the sentinel, then re-check the seqlock after registering as a reader. `PinnedLoan::Drop` clears the sentinel, preventing permanent subscriber lockout on panic.

### Packed AtomicU64 subscriber slots (PodBus v3)

Each subscriber slot packs a 2-bit state and 62-bit value into a single `AtomicU64`:

| State (2 bits) | Meaning |
|---|---|
| `0b00` (FREE) | Slot available for claiming |
| `0b10` (CLAIMING) | Slot being claimed; value = claim timestamp in microseconds |
| `0b01` (ACTIVE) | Slot owned by a subscriber; value = cursor position |

Transitions are CAS-guarded. The publisher scans ACTIVE slots to find the slowest subscriber cursor for backpressure enforcement.

---

## Service discovery

### Registry layout

The registry is a fixed-size shared memory file at `/dev/shm/crossbar-registry` (Linux), `$TMPDIR/crossbar-registry` (macOS/Windows). The path is configurable via the `CROSSBAR_REGISTRY` environment variable.

```
Header (64 bytes):
  magic: [u8; 8] = "XREG_ZC\0"
  version: u32
  entry_count: AtomicU32
  max_entries: u32 = 256

Entry (128 bytes each, up to 256):
  active: AtomicU32    // 0 = free, 1 = active
  pid: u32             // publisher PID
  timestamp: u64       // heartbeat (microseconds since epoch)
  region_name: [u8; 48] // null-terminated
  topic_uri: [u8; 64]   // null-terminated
```

### CAS registration

Slot acquisition uses `compare_exchange(FREE, ACTIVE)` -- only one thread wins the CAS and gets to write the entry. This prevents concurrent register races.

### Wildcard matching

`discover(pattern)` and `discover_since(pattern, since_us)` support trailing `*` wildcards: `"/tick/*"` matches `"/tick/AAPL"`. Exact strings require exact match.

`discover_since` enables reactive, polling-based discovery: callers store the latest `timestamp_us` from the previous batch and pass it as `since_us` on the next call.

### Stale pruning

`prune_stale(timeout)` removes entries whose timestamp is older than `timeout` from now. Both `prune_stale` and `unregister` use `compare_exchange(ACTIVE, FREE)` to atomically clear slots, preventing a double-decrement race on `entry_count`. A wrap-around guard clamps the counter to zero if it underflows.

---

## Wait strategies

| Strategy | Latency | CPU usage | Best for |
|---|---|---|---|
| `BusySpin` | Lowest (~0 ns wakeup) | 100% core | Dedicated, pinned cores |
| `YieldSpin` | Low (~30 ns on x86) | High | Shared cores, SMT |
| `BackoffSpin` | Medium (exponential) | Decreasing | Background consumers |
| `Adaptive` | Auto-scaling | Varies | General purpose (default) |

### Adaptive (3-phase)

1. **Bare spin** for `spin_iters` iterations (default 100) -- fastest wakeup, no instructions emitted.
2. **Yield spin** for `yield_iters` iterations (default 10) -- emits `PAUSE` (x86) or `SEVL+WFE` (aarch64).
3. **OS sleep** -- `futex(FUTEX_WAIT)` on Linux, `WaitOnAddress` on Windows, `WFE` on macOS/aarch64, 1 ms sleep on macOS/x86.

The OS phase only fires when a subscriber is genuinely blocked. Subscribers register in `TE_WAITERS` before sleeping; the publisher reads `WAITERS` and skips `futex_wake` if zero. This means publish has zero notification overhead when all subscribers poll with `try_recv()`.

### Platform yield hints

| Platform | Spin hint | OS wake mechanism |
|---|---|---|
| x86/x86_64 | `PAUSE` (~140 cycles) | `futex` (Linux), `WaitOnAddress` (Windows) |
| aarch64 | `SEVL` + `WFE` (cache-line wake) | `WFE` (macOS), `futex` (Linux) |
| macOS x86 | `PAUSE` | 1 ms sleep (no futex equivalent) |

---

## Publish path

1. Pop block from per-publisher cache, or Treiber stack if cache empty (CAS on `pool_head`).
2. Write payload into block at `BLOCK_DATA_OFFSET` (offset 8).
3. Claim sequence number: `fetch_add(1)` on `WRITE_SEQ` (AcqRel).
4. Compute slot: `seq & (ring_depth - 1)` (bitmask, ring_depth must be power of 2).
5. Acquire ring slot -- **single-publisher**: plain store of `SEQ_WRITING` (Release); **multi-publisher**: `compare_exchange_weak(current, SEQ_WRITING)` CAS loop.
6. Set block refcount to 1 (Release).
7. Write `block_idx` and `data_len` (Relaxed) -- safe, bracketed by seqlock.
8. Seqlock close: store new `seq` (Release) -- slot is now readable.
9. Decrement old block's refcount (AcqRel); free if it reaches zero.
10. Smart wake: call `futex_wake` on `WRITE_SEQ` low 32 bits only if `WAITERS > 0`.

Step 10 means `publish()` costs zero extra when all subscribers use `try_recv()` (no blocked waiters). The futex syscall (~170 ns) fires only when a subscriber is blocked in `recv()`.

## Receive path

1. Load `WRITE_SEQ` (Acquire) -- return `None` if unchanged.
2. Compute scan window: `[last_seq+1 .. write_seq]`, clamped to `ring_depth`.
3. For each seq in the window (first committed slot wins):
   a. Compute slot: `seq & (ring_depth - 1)`.
   b. Seqlock check 1: load slot `seq` (Acquire) -- skip if `SEQ_WRITING` or mismatch.
   c. Read `block_idx` and `data_len` (Relaxed) -- safe inside seqlock bracket.
   d. CAS-increment block refcount (AcqRel) -- acquire a reference.
   e. Seqlock check 2: verify slot `seq` again -- undo refcount if overwritten.
4. Advance `last_seq`, return `Sample`.
5. On guard drop: decrement refcount (Release); if last reference, acquire fence then free block.

In single-publisher mode, the scan window is always 1 slot. Under multi-publisher, at most `ring_depth` slots are scanned.

---

## Pod trait and typed pub/sub

`Pod` is an unsafe marker trait:

```rust
pub unsafe trait Pod: Copy + Send + 'static {}
```

The implementor guarantees:
1. `Copy + 'static` -- no heap, no lifetimes.
2. Every bit pattern is a valid value -- no padding-dependent invariants, no niche optimizations.
3. `#[repr(C)]` -- deterministic field layout.

At runtime, `register_typed::<T>()` writes `size_of::<T>()` into the topic entry (`TYPE_SIZE`). `try_recv_typed::<T>()` returns `None` if the stored size doesn't match -- catching publisher/subscriber type mismatches across process boundaries.

---

## Heartbeat and liveness

Publishers store a microsecond-resolution timestamp in the global header every `heartbeat_interval` (default 100 ms), amortized over 1024 loan calls to avoid `Instant::now()` overhead on the hot path. With multiple publishers, `fetch_max` ensures the heartbeat only advances -- any live publisher keeps the region alive.

Subscribers check the heartbeat when blocking in `recv()`. If the timestamp is older than `stale_timeout` (default 5 s), `recv()` returns `Err(Error::PublisherDead)`.

---

## Multi-publisher

Multiple publishers share the same SHM region via `Publisher::open()`. The protocol supports this through:

- **Atomic seq claiming**: `fetch_add(1)` on `WRITE_SEQ` gives each publisher a unique, monotonically increasing sequence number at commit time.
- **CAS-based ring slot locking**: `compare_exchange(current, SEQ_WRITING)` serializes writes to the same ring slot when two publishers' seqs differ by exactly `ring_depth`.
- **CAS-based topic registration**: Three-state `ACTIVE` field (`FREE=0 -> INIT=2 -> ACTIVE=1`) prevents concurrent register races.
- **Shared flock**: The creating publisher downgrades its exclusive lock to shared after initialization. Secondary publishers acquire shared locks. All shared locks together prevent a new `create()` from truncating the region.
- **`fetch_max` heartbeat**: Any live publisher keeps the heartbeat fresh.

The block pool (Treiber stack) is already lock-free and handles concurrent alloc/free from multiple publishers without modification.

---

## Bidirectional channel

`Channel` composes two pub/sub regions into a bidirectional pair. Each side publishes on its own region and subscribes to the other's:

```
Server                                  Client
  Publisher("rpc-srv")  ──────────>  Stream("rpc-srv")
  Stream("rpc-cli")  <──────────  Publisher("rpc-cli")
```

- `listen()` creates `{name}-srv`, polls for `{name}-cli`.
- `connect()` creates `{name}-cli`, connects to `{name}-srv`.
- Both sides get a `Channel` with `send()` / `recv()` / `loan()`.
- Subscriptions start from seq 0 (not latest) so early messages are not missed.

---

## C / C++ FFI

The `ffi` feature (`src/ffi.rs`) exposes `extern "C"` functions with opaque pointer types. Headers: `include/crossbar.h` and `include/crossbar.hpp`.

Key design decisions:
- **No exposed loans in FFI** -- `crossbar_publish()` copies data and publishes in one call. The "born-in-SHM" pattern requires Rust lifetimes that don't translate to C.
- **Self-owned samples** -- `CrossbarSample` holds an `Arc<Region>` to keep the mmap alive, replacing the borrowed `Sample<'a>`. Refcount semantics are identical.
- **Value-type topic handle** -- `crossbar_topic_t` is a small `#[repr(C)]` struct, safe to copy.
- **Null safety** -- all 22 `extern "C"` functions check every pointer parameter for null before dereferencing.

Build: `cargo rustc --release --features ffi --crate-type cdylib`

---

## Subscriber count

Each topic entry contains an `AtomicU32` counter at offset `0x78` (`TE_SUBSCRIBER_COUNT`) that tracks live `Stream` objects.

- **Increment**: `Subscriber::subscribe()` increments via `fetch_add(1, Relaxed)`.
- **Decrement**: `Stream::Drop` decrements via `fetch_sub(1, Relaxed)`.
- **Query**: `Publisher::subscriber_count(&self, handle) -> Result<u32, Error>` reads the counter. FFI: `crossbar_topic_subscriber_count()`.

The counter uses `Relaxed` ordering because it is advisory -- publishers use it for monitoring, not for correctness-critical synchronization.

---

## File layout

```
src/
  lib.rs              Crate root, public re-exports
  pod.rs              Pod trait
  error.rs            Error enum
  wait.rs             WaitStrategy

  protocol/           Pure computation -- no OS calls
    mod.rs
    layout.rs         SHM offset constants and layout helpers
    config.rs         Config struct
    region.rs         Region -- Treiber stack, seqlock ring, refcount, commit_to_ring

  platform/           OS-specific glue (mmap, futex, file creation)
    mod.rs
    mmap.rs           RawMmap
    notify.rs         futex / WaitOnAddress / WFE
    shm.rs            Publisher, Subscriber
    subscription.rs   Stream, Sample, PinnedGuard, TypedSample
    loan.rs           Loan, TypedLoan, PinnedLoan, Topic
    channel.rs        Channel -- bidirectional channel
    pod_bus.rs        PodBus, BusSubscriber -- SPMC broadcast ring
    registry.rs       Registry -- global topic registry for service discovery

include/
  crossbar.h          C header
  crossbar.hpp        C++ header

bindings/
  python/             Python bindings

tests/               Integration tests
benches/             Criterion benchmarks (pubsub, pod_bus, spmc_contention)
examples/            publisher, subscriber, pinned_throughput
```
