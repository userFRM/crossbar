# Architecture

Crossbar is a single Rust crate. It moves data between processes at O(1) cost by transferring only an 8-byte descriptor (block index + data length) through a seqlock ring. The block lives in a shared mmap region; the subscriber reads it in-place.

---

## Crate structure

```
src/
  lib.rs          #![no_std] crate root, feature gates, public re-exports
  pod.rs          Pod trait
  error.rs        IpcError
  wait.rs         WaitStrategy
  ffi.rs          C FFI bindings (#[cfg(feature = "ffi")])

  protocol/       no_std — pure atomics, raw pointer math, no OS calls
    layout.rs     All SHM offset constants and layout helper functions
    config.rs     PubSubConfig
    region.rs     Region — Treiber stack, seqlock ring, refcount, commit_to_ring

  platform/       #[cfg(feature = "std")] — every OS call lives here
    mmap.rs       RawMmap
    notify.rs     futex / WaitOnAddress / WFE
    shm.rs        ShmPublisher, ShmSubscriber
    subscription.rs  Subscription, SampleGuard, TypedSampleGuard
    loan.rs       ShmLoan, TypedShmLoan, TopicHandle
    channel.rs    ShmChannel — bidirectional channel
    pod_bus.rs    PodBus, PodSubscriber — SPMC broadcast ring

include/
  crossbar.h      C/C++ header
```

The split is intentional: `protocol/` can be used on bare-metal (no OS) if you bring your own mmap. `platform/` is the opinionated std implementation that most users want.

---

## Memory layout

One mmap region — three logical areas:

```
+---------------------------+  offset 0
| Global Header (128 bytes) |
|   magic (8B)              |  XBAR_ZC\0
|   version (u32)           |  3
|   max_topics (u32)        |
|   block_count (u32)       |
|   block_size (u32)        |
|   ring_depth (u32)        |
|   pool_head (AtomicU64)   |  Treiber stack head: pack(generation, index)
|   heartbeat (AtomicU64)   |  µs since epoch
|   pid (u64)               |
|   stale_timeout_us (u64)  |
+---------------------------+  offset 128
| Topic Entry 0 (128 bytes) |
|   1st cache line (hot):   |
|   0x00 ACTIVE   (u32)     |  AtomicU32
|   0x04 (reserved, u32)    |  (was NOTIFY — now merged into WRITE_SEQ)
|   0x08 WRITE_SEQ(u64)     |  AtomicU64 — monotonic publish counter + futex wake address
|   0x10 WAITERS  (u32)     |  AtomicU32 — blocked subscriber count
|   2nd cache line (cold):  |
|   0x14 URI_LEN  (u32)     |
|   0x18 URI_HASH (u64)     |  FNV-1a
|   0x20 URI      (64B)     |
|   0x60 TYPE_SIZE(u32)     |  size_of::<T>() for typed topics; 0 = untyped
|   0x64 PINNED_BLOCK (u32) |  block index for pinned publish; NO_BLOCK = none
|   0x68 PINNED_READERS     |  AtomicU32 — active pinned-read guard count
|   0x70 PINNED_SEQ  (u64)  |  AtomicU64 — packed (seq:32 | data_len:32) seqlock
|   0x78 SUBSCRIBER_COUNT   |  AtomicU32 — live Subscription count for this topic
+---------------------------+
| Topic Entry 1 …           |
+---------------------------+
| Ring 0 (ring_depth × 16B) |
|   per entry:              |
|   0x00 seq      (AtomicU64)|  seqlock — SEQ_WRITING (u64::MAX) = being written
|   0x08 block_idx(AtomicU32)|  Relaxed
|   0x0C data_len (AtomicU32)|  Relaxed
+---------------------------+
| Ring 1 …                  |
+---------------------------+
| Block Pool                |
|   per block:              |
|   0x00 next_free(AtomicU32)|  Treiber free-list link
|   0x04 refcount (AtomicU32)|
|   0x08 data …             |  user payload starts here
+---------------------------+
```

Hot topic fields (`ACTIVE`, `WRITE_SEQ`, `WAITERS`) are packed into the first cache line of each topic entry to avoid cache misses on the subscriber polling path.

---

## Publish path

1. Pop block from per-publisher cache, or Treiber stack if cache empty (CAS on `pool_head`)
2. Write payload into block at `BLOCK_DATA_OFFSET` (offset 8)
3. Claim sequence number: `fetch_add(1)` on `WRITE_SEQ` (AcqRel)
4. Compute slot: `seq & (ring_depth - 1)` (bitmask, ring_depth must be power of 2)
5. Acquire ring slot — **single-publisher**: plain store of `SEQ_WRITING` (Release); **multi-publisher**: `compare_exchange_weak(current, SEQ_WRITING)` CAS loop with `yield_hint`
6. Set block refcount to 1 (Release)
7. Write `block_idx` and `data_len` (Relaxed) — safe, bracketed by seqlock
8. Seqlock close: store new `seq` (Release) — slot is now readable
9. Decrement old block's refcount (AcqRel); free if it reaches zero
10. Smart wake: call `futex_wake` on `WRITE_SEQ` low-32 bits only if `WAITERS > 0`

Step 10 means `publish()` costs zero extra when all subscribers use `try_recv()` (no blocked waiters). The futex syscall (~170 ns) fires only when a subscriber is blocked in `recv()`.

The per-publisher block cache (step 1) eliminates the Treiber stack CAS for ~87% of allocations. Each publisher caches up to 8 blocks locally.

---

## Receive path

1. Load `WRITE_SEQ` (Acquire) — return `None` if unchanged
2. Compute scan window: `[last_seq+1 .. write_seq]`, clamped to `ring_depth`
3. For each seq in the window (first committed slot wins):
   a. Compute slot: `seq & (ring_depth - 1)`
   b. Seqlock check 1: load slot `seq` (Acquire) — skip if `SEQ_WRITING` or mismatch
   c. Read `block_idx` and `data_len` (Relaxed) — safe inside seqlock bracket
   d. CAS-increment block refcount (AcqRel) — acquire a reference
   e. Seqlock check 2: verify slot `seq` again — undo refcount if overwritten
4. Advance `last_seq`, return `SampleGuard`
5. On guard drop: decrement refcount (Release); if last reference, acquire fence then free block

In single-publisher mode, the scan window is always 1 slot — the loop executes once. Under multi-publisher, at most `ring_depth` slots are scanned (default: 8, each check is ~9 ns).

---

## Seqlock correctness

Ring data fields (`block_idx`, `data_len`) are read and written with `Relaxed` ordering. This is not a data race: `AtomicU32` operations are atomic by definition. The memory ordering (visibility guarantees) comes from the seqlock bracket — the Release store at seqlock close synchronizes-with the Acquire load at seqlock check 1. The Relaxed data reads between the two checks are safe because they happen within the same coherent atomic region.

The seqlock uses `SEQ_WRITING` (`u64::MAX`) as the "slot locked" sentinel instead of 0. Publishers CAS the slot from its current seq to `SEQ_WRITING` before writing, then store the new seq on close. This allows multiple publishers to safely contend on the same ring slot — the CAS serializes writes, and subscribers skip slots where `entry_seq == SEQ_WRITING`.

---

## Treiber stack

The block pool is a lock-free stack with a 64-bit `(generation, index)` head pointer. The generation counter prevents ABA: even if the same block is freed and reallocated between a load and a CAS, the generation mismatch causes the CAS to fail.

- **Alloc**: CAS-weak head to `(gen+1, next_free_of_head_block)`; reuse `Err(current)` on retry
- **Free**: CAS-weak head to `(gen+1, freed_block)`; write old head index into freed block's link field
- **Per-publisher cache**: Each publisher caches up to 8 blocks locally, refilling from the global stack when empty. This amortizes the CAS over multiple allocations.
- **Block recycling**: When `commit_to_ring` frees the old block, it stashes the index in `Region.last_freed` instead of pushing to the stack. The next `alloc_cached` grabs this block first — it's still warm in L1/L2 from the previous write, eliminating RFO cache misses for large payloads.

---

## Pod trait and typed pub/sub

`Pod` is an unsafe marker trait:

```rust
pub unsafe trait Pod: Copy + Send + 'static {}
```

The implementor guarantees:
1. `Copy + 'static` — no heap, no lifetimes
2. Every bit pattern is a valid value — no padding-dependent invariants, no niche optimizations
3. `#[repr(C)]` — deterministic field layout

At runtime, `register_typed::<T>()` writes `size_of::<T>()` into the topic entry (`TYPE_SIZE`). `try_recv_typed::<T>()` panics if the stored size doesn't match — catching publisher/subscriber type mismatches across process boundaries.

---

## Smart wake

Notification is merged into `WRITE_SEQ`: the publisher's `fetch_add` on `WRITE_SEQ` changes the value, and subscribers in `recv()` futex-wait on the low 32 bits of `WRITE_SEQ`. A subscriber registers itself in `TE_WAITERS` before sleeping; it unregisters on wakeup. The publisher reads `WAITERS` before calling `futex_wake` — if zero, it skips the syscall entirely.

Result: when all subscribers poll with `try_recv()`, publish has zero notification overhead — no separate counter to increment. The ~170 ns futex overhead only appears when a subscriber is genuinely blocked.

---

## Platform wake

| Platform | Mechanism |
|---|---|
| Linux | `futex(FUTEX_WAIT)` / `futex(FUTEX_WAKE)` via `libc::syscall` |
| Windows | `WaitOnAddress` / `WakeByAddressAll` from Win32 |
| macOS x86 | 1 ms sleep (no `futex` equivalent) |
| macOS aarch64 | `WFE` (Wait For Event) — wakes on cache-line invalidation, ~30 ns |

The spin phase uses `PAUSE` on x86 and `SEVL + WFE` on aarch64. WFE puts the core into a low-power state until the publisher's store invalidates the cache line — effectively hardware-assisted polling.

---

## Heartbeat and liveness

Publishers store a microsecond-resolution timestamp in the global header every `heartbeat_interval` (default 100 ms), amortized over 1024 loan calls to avoid `Instant::now()` overhead on the hot path. With multiple publishers, `fetch_max` ensures the heartbeat only advances — any live publisher keeps the region alive.

Subscribers check the heartbeat when blocking in `recv()`. If the timestamp is older than `stale_timeout` (default 5 s), `recv()` returns `Err(IpcError::PublisherDead)`.

---

## Multi-publisher

Multiple publishers can share the same SHM region via `ShmPublisher::open()`. The protocol supports this through:

- **Atomic seq claiming**: `fetch_add(1)` on `WRITE_SEQ` gives each publisher a unique, monotonically increasing sequence number at commit time
- **CAS-based ring slot locking**: `compare_exchange(current, SEQ_WRITING)` serializes writes to the same ring slot when two publishers' seqs differ by exactly `ring_depth`
- **CAS-based topic registration**: Three-state `ACTIVE` field (`FREE=0 → INIT=2 → ACTIVE=1`) prevents concurrent register races
- **Shared flock**: The creating publisher downgrades its exclusive lock to shared after initialization. Secondary publishers acquire shared locks. All shared locks together prevent a new `create()` from truncating the region.
- **`fetch_max` heartbeat**: Any live publisher keeps the heartbeat fresh

The block pool (Treiber stack) is already lock-free and handles concurrent alloc/free from multiple publishers without modification.

---

## Bidirectional channel

`ShmChannel` composes two pub/sub regions into a bidirectional pair. Each side publishes on its own region and subscribes to the other's:

```
Server                                  Client
  ShmPublisher("rpc-srv")  ──────────>  Subscription("rpc-srv")
  Subscription("rpc-cli")  <──────────  ShmPublisher("rpc-cli")
```

- `listen()` creates `{name}-srv`, polls for `{name}-cli`
- `connect()` creates `{name}-cli`, connects to `{name}-srv`
- Both sides get an `ShmChannel` with `send()` / `recv()` / `loan()`
- Subscriptions start from seq 0 (not latest) so early messages are not missed

This is built entirely on the existing pub/sub transport — no new protocol machinery. The channel adds ~0 ns overhead vs raw pub/sub.

---

## C / C++ FFI

The `ffi` feature (`src/ffi.rs`) exposes `extern "C"` functions with opaque pointer types. The C header is `include/crossbar.h`.

Key design decisions:
- **No exposed loans in FFI** — `crossbar_publish()` copies data and publishes in one call. The "born-in-SHM" pattern requires Rust lifetimes that don't translate to C.
- **Self-owned samples** — `CrossbarSample` holds an `Arc<Region>` to keep the mmap alive, replacing the borrowed `SampleGuard<'a>`. Refcount semantics are identical.
- **Value-type topic handle** — `crossbar_topic_t` is a small `#[repr(C)]` struct, safe to copy.

Build: `cargo rustc --release --features ffi --crate-type cdylib`

---

## PodBus — SPMC broadcast ring

`PodBus<T: Pod>` is a single-producer, multi-consumer broadcast ring for `Pod` types. It lives entirely in process memory (no SHM) and is designed for high fan-out scenarios where one producer feeds many consumers with the same data stream.

**Architecture:**

```
PodBus<T>
  ring: Box<[Slot<T>]>     power-of-two array of seqlock-stamped slots
  mask: u64                 ring_size - 1, for bitmask indexing
  write_seq: AtomicU64      monotonic publish counter

Slot<T>
  stamp: AtomicU64          seqlock: 0 = never written, odd = writing, even = committed
  data: UnsafeCell<T>       payload (volatile read/write)

PodSubscriber<T>
  cursor: u64               independent read position per consumer
```

**Seqlock stamp protocol:**

- `0` — slot never written
- `seq * 2 + 1` (odd) — write in progress, readers must retry
- `(seq + 1) * 2` (even) — committed and readable

The publisher writes the data between the odd and even stamp transitions. Subscribers snapshot the stamp, read via `read_volatile`, then verify the stamp is unchanged. If the stamp changed mid-read, the data was torn and the read is discarded.

**Key properties:**

- Publish is O(1) regardless of subscriber count — no per-subscriber bookkeeping
- Each `PodSubscriber` tracks its own cursor independently
- Subscribers that fall behind auto-skip to the oldest available data (lossy)
- `T: Pod` ensures all bit patterns are valid, so torn reads are harmless (just discarded)
- No heap allocation on publish or receive

---

## MonitorWait — UMONITOR/UMWAIT

`WaitStrategy::MonitorWait` is an x86_64-only wait strategy that uses Intel's WAITPKG instructions (available on Tremont, Alder Lake, and newer microarchitectures) for cache-line-aware low-power waiting.

**CPUID detection:**

WAITPKG support is detected via `CPUID leaf 7, sub-leaf 0, ECX bit 5`. The result is cached in a static `AtomicU8` with racy initialization (benign — worst case is redundant `CPUID` calls on first access).

**Instruction encoding:**

`UMONITOR` and `UMWAIT` are not available as stable Rust intrinsics, so they are encoded as raw `.byte` sequences in inline assembly:

- `UMONITOR rax` — `F3 0F AE F0` — sets up monitoring on the cache line containing the address in `rax`
- `UMWAIT ecx` — `F2 0F AE F1` — waits until a store hits the monitored cache line, or a TSC deadline expires

**Usage in blocking_recv:**

In `blocking_recv`, when `MonitorWait` is the active strategy, the subscriber monitors the `WRITE_SEQ` futex address via `UMONITOR`, then enters C0.1 low-power state via `UMWAIT` with a ~100 us TSC deadline (~300,000 cycles at 3 GHz). A publisher's store to `WRITE_SEQ` invalidates the monitored cache line and wakes the subscriber with ~30 ns latency.

**Fallback:**

On CPUs without WAITPKG, `monitor_wait_on_address` falls back to `PAUSE`. The generic `WaitStrategy::wait()` method (which has no address to monitor) also falls back to `PAUSE`.

---

## Subscriber count

Each topic entry contains an `AtomicU32` counter at offset `0x78` (`TE_SUBSCRIBER_COUNT`) that tracks the number of live `Subscription` objects for that topic.

- **Increment**: When `ShmSubscriber::subscribe()` creates a new `Subscription`, the counter is incremented via `fetch_add(1, Relaxed)`.
- **Decrement**: `Subscription` implements `Drop`, which decrements the counter via `fetch_sub(1, Relaxed)`.
- **Query**: `ShmPublisher::subscriber_count(&self, handle) -> Result<u32, IpcError>` reads the counter. The FFI equivalent is `crossbar_topic_subscriber_count()`.

The counter uses `Relaxed` ordering because it is advisory — publishers use it for monitoring and graceful shutdown, not for correctness-critical synchronization. The `ShmChannel` field order ensures `rx: Subscription` is declared before `_rx_sub: ShmSubscriber`, so the subscription drops (and decrements the counter) before the subscriber unmaps the region.
