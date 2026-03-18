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

Hot topic fields (`ACTIVE`, `NOTIFY`, `WRITE_SEQ`, `WAITERS`) are packed into the first cache line of each topic entry to avoid cache misses on the subscriber polling path.

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
