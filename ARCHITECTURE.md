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
|   0x04 NOTIFY   (u32)     |  AtomicU32 — smart wake counter
|   0x08 WRITE_SEQ(u64)     |  AtomicU64 — monotonic publish counter
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
|   0x00 seq      (AtomicU64)|  seqlock — 0 means slot is being written
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

1. Pop block from Treiber stack (CAS on `pool_head`)
2. Write payload into block at `BLOCK_DATA_OFFSET` (offset 8)
3. Set block refcount to 1 (Release) — ring now holds one reference
4. Seqlock open: store `seq = 0` to ring slot (Release) — invalidates slot
5. Write `block_idx` and `data_len` (Relaxed) — safe, bracketed by seqlock
6. Seqlock close: store new `seq` (Release) — slot is now readable
7. Store new `seq` to topic `WRITE_SEQ` (Release)
8. Decrement old block's refcount (AcqRel); free if it reaches zero
9. Smart wake: increment `NOTIFY`; call `futex_wake` only if `WAITERS > 0`

Step 9 means `publish()` costs ~8 ns when all subscribers use `try_recv()` (no blocked waiters). The futex syscall (~170 ns) fires only when a subscriber is blocked in `recv()`.

---

## Receive path

1. Load `WRITE_SEQ` (Acquire) — return `None` if unchanged
2. Compute slot: `seq % ring_depth`
3. Seqlock check 1: load slot `seq` (Acquire) — skip if 0 or mismatch
4. Read `block_idx` and `data_len` (Relaxed) — safe inside seqlock bracket
5. CAS-increment block refcount (AcqRel) — acquire a reference
6. Seqlock check 2: verify slot `seq` again — undo refcount and retry if overwritten
7. Advance `last_seq`, return `SampleGuard`
8. On guard drop: decrement refcount (AcqRel); free block if it reaches zero

---

## Seqlock correctness

Ring data fields (`block_idx`, `data_len`) are read and written with `Relaxed` ordering. This is not a data race: `AtomicU32` operations are atomic by definition. The memory ordering (visibility guarantees) comes from the seqlock bracket — the Release store at seqlock close synchronizes-with the Acquire load at seqlock check 1. The Relaxed data reads between the two checks are safe because they happen within the same coherent atomic region. This pattern eliminates formal UB while compiling to identical instructions on all platforms.

---

## Treiber stack

The block pool is a lock-free stack with a 64-bit `(generation, index)` head pointer. The generation counter prevents ABA: even if the same block is freed and reallocated between a load and a CAS, the generation mismatch causes the CAS to fail.

- **Alloc**: CAS head to `(gen+1, next_free_of_head_block)`; clear refcount
- **Free**: CAS head to `(gen+1, freed_block)`; write old head index into freed block's link field

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

The `TE_NOTIFY` field is a monotonic counter. The publisher increments it on every `publish()`. A subscriber in `recv()` registers itself in `TE_WAITERS` before sleeping on the futex; it unregisters on wakeup. The publisher reads `WAITERS` before calling `futex_wake` — if zero, it skips the syscall entirely.

Result: when all subscribers poll with `try_recv()`, publish costs ~8 ns (atomic increment only). The ~170 ns futex overhead only appears when a subscriber is genuinely blocked.

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

The publisher stores a microsecond-resolution timestamp in the global header every `heartbeat_interval` (default 100 ms), amortized over 1024 loan calls to avoid `Instant::now()` overhead on the hot path.

Subscribers check the heartbeat when blocking in `recv()`. If the timestamp is older than `stale_timeout` (default 5 s), `recv()` returns `Err(IpcError::PublisherDead)`.
