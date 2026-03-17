# Architecture: Zero-Copy SHM Pub/Sub

Crossbar is a single crate providing zero-copy pub/sub over shared memory
(`/dev/shm`). It moves data between processes at O(1) cost regardless of
payload size by transferring only an 8-byte descriptor (block index + data
length) through a seqlock ring.

---

## Overview

```
  crossbar (58 ns)                    iceoryx2 (~231 ns on same hw)
  ========================            ================================

  ShmLoan::publish()                  publisher.loan()?.write_payload()
          |                                   |
          v                                   v
  alloc block (Treiber CAS)           alloc from lock-free pool
  write payload into SHM              write payload into SHM
          |                                   |
          v                                   v
  set refcount = 1                    service discovery layer
  seqlock open (seq=0)                POSIX configuration layer
  write 8B descriptor                 publish descriptor
  seqlock close (seq=N)                       |
  bump write_seq                              v
  free old block if rc=0              subscriber.receive()
  smart wake (futex)                  [event-based notification]
          |
          v
  Subscription::try_recv()
  [load write_seq]
  [seqlock check 1]
  [read block_idx + data_len]
  [CAS refcount increment]
  [seqlock check 2]
          |
          v
  SampleGuard — Deref<[u8]>
  [reads directly from mmap]
```

Crossbar is faster because it skips iceoryx2's service discovery and POSIX
configuration layer -- it goes straight from user code to atomics.

---

## Shared-memory path

The SHM path moves data between processes at O(1) cost regardless of payload
size. Transfer writes only 8 bytes (block index + data length) to a ring slot.

### Borrowed `SampleGuard`

`SampleGuard<'a>` borrows `&'a Region` (no `Arc::clone` per receive) and holds
a block index and length. It implements `Deref<Target=[u8]>` -- subscribers read
directly from the mmap'd region. The block is held alive by atomic refcounting;
the guard decrements the refcount on drop, freeing the block when it hits zero.

### Seqlock ring

Each topic has a ring of `(seq, block_idx, data_len)` entries. The publisher
uses a seqlock protocol:

1. Invalidate the slot's sequence to 0 (seqlock open)
2. Write block_idx and data_len via `AtomicU32` stores (`Relaxed` ordering) --
   bracketed by the seqlock sequence stores
3. Store the new sequence (seqlock close, Release ordering)

The subscriber:

1. Loads the slot sequence (Acquire) -- skips if 0 or mismatched
2. Reads block_idx and data_len via `AtomicU32` loads (`Relaxed`)
3. CAS-increments the block's refcount (`AtomicU32`, AcqRel)
4. Re-checks the slot sequence to detect an overwrite race
5. If the race is detected, undoes the refcount and returns `None`

All ring data field reads/writes use `AtomicU32` with `Relaxed` ordering. This
eliminates formal UB under the Rust memory model while compiling to identical
instructions on all platforms (the seqlock bracket provides the actual ordering).

### Memory layout

```
  One mmap region — three logical areas
  ======================================

  +---------------------------+  offset 0
  | Global Header (128 bytes) |
  | magic, version, config,   |
  | pool_head (AtomicU64),    |
  | heartbeat, PID, timeout   |
  +---------------------------+  offset 128
  | Topic Entry 0 (128 bytes) |
  |   1st cache line (hot):   |
  |     0x00 ACTIVE   (u32)   |
  |     0x04 NOTIFY   (u32)   |
  |     0x08 WRITE_SEQ(u64)   |
  |     0x10 WAITERS  (u32)   |  <-- hot fields in 1st cache line
  |   2nd cache line (cold):  |
  |     0x14 URI_LEN  (u32)   |
  |     0x18 URI_HASH (u64)   |
  |     0x20 URI      (64B)   |
  |     0x60 TYPE_SIZE(u32)   |  <-- typed pub/sub: expected sizeof(T)
  +---------------------------+
  | Topic Entry 1 ...         |
  +---------------------------+
  | Ring 0 (ring_depth * 16B) |
  |   per entry:              |
  |     0x00 seq      (u64)   |  seqlock
  |     0x08 block_idx(u32)   |  AtomicU32 Relaxed
  |     0x0C data_len (u32)   |  AtomicU32 Relaxed
  +---------------------------+
  | Ring 1 ...                |
  +---------------------------+
  | Block Pool                |
  |   per block:              |
  |     0x00 next_free(u32)   |  Treiber link (AtomicU32)
  |     0x04 refcount (u32)   |  AtomicU32
  |     0x08 data...          |  user payload
  +---------------------------+
```

Hot fields (`TE_ACTIVE`, `TE_NOTIFY`, `TE_WRITE_SEQ`, `TE_WAITERS`) are packed
into the first cache line (offsets 0x00-0x13) of each topic entry to minimize
cache misses on the subscriber's polling path. `TE_TYPE_SIZE` at offset 0x60
stores the `mem::size_of::<T>()` for typed topics -- zero for untyped topics.

### Treiber stack pool

The block pool is a lock-free Treiber stack using a 64-bit packed
`(generation, index)` head pointer. The generation counter prevents ABA:

- **Alloc**: CAS the head to the next-free pointer stored at offset 0 of the
  head block. Clear refcount on success.
- **Free**: CAS the head to the freed block, storing the old head index at
  offset 0 of the freed block.

### Publish flow

1. Pop a block from the Treiber stack (CAS)
2. Write payload into the block at `BLOCK_DATA_OFFSET`
3. Set the block's refcount to 1 (Release)
4. Seqlock-open the target ring slot (store seq=0, Release)
5. Write block_idx and data_len (AtomicU32 Relaxed)
6. Seqlock-close (store seq=N, Release)
7. Bump `write_seq`
8. Decrement old block's refcount; free if it hits 0
9. Smart wake: bump `TE_NOTIFY`, call `futex_wake` only if `TE_WAITERS > 0`

### Receive flow

1. Load `write_seq` -- if unchanged, return `None`
2. Compute target slot from sequence
3. Seqlock-read: check seq, read block_idx + data_len (AtomicU32 Relaxed)
4. CAS-increment block refcount (Acquire load, AcqRel CAS)
5. Seqlock re-check -- undo refcount if slot was overwritten
6. Return `SampleGuard<'a>` (deref reads directly from mmap)
7. On guard drop: `fetch_sub(1, AcqRel)` on refcount; free block if it hits 0

### Heartbeats and liveness

The publisher updates a heartbeat timestamp (`AtomicU64`, microseconds since
epoch) in the global header. `loan()` checks the clock every 1024 calls to
amortize `Instant::now()` overhead.

Subscribers call `check_heartbeat()` when blocking in `recv()`:

- Fresh heartbeat: continue waiting (spin -> futex)
- Stale: return `Err(IpcError::PublisherDead)`

### Platform-specific wake

- **Linux**: `futex(FUTEX_WAIT)` / `futex(FUTEX_WAKE)` via `libc::syscall`
- **Windows**: `WaitOnAddress` / `WakeByAddressAll` from Win32
- **macOS / other Unix**: polling fallback with 1ms sleep

---

## Pod trait and typed pub/sub

The byte-oriented API (`loan` / `set_data` / `SampleGuard<[u8]>`) is the
foundation. On top of it, crossbar provides **typed pub/sub** for structured
data.

### The `Pod` trait

`Pod` (Plain Old Data) marks types that are safe to interpret directly from
shared memory bytes. A type is `Pod` if:

1. It is `Copy + 'static`
2. Every bit pattern is a valid value (no padding-dependent invariants)
3. It has `#[repr(C)]` layout (deterministic field ordering)

```rust
#[derive(Clone, Copy)]
#[repr(C)]
struct Tick {
    price: f64,
    volume: u64,
}

unsafe impl Pod for Tick {}
```

The `unsafe` impl is the programmer's promise that the type satisfies these
invariants. Crossbar performs a runtime size check (`TE_TYPE_SIZE`) to catch
mismatched publisher/subscriber types.

### Typed API

| Byte API | Typed API | Description |
|---|---|---|
| `register(uri)` | `register_typed::<T>(uri)` | Register a topic, recording `size_of::<T>()` in the topic entry |
| `loan(handle)` | `loan_typed::<T>(handle)` | Loan a block, returning a typed mutable reference |
| `try_recv()` | `try_recv_typed::<T>()` | Receive a `TypedSampleGuard<T>` with `Deref<Target=T>` |

`TypedSampleGuard<T>` wraps `SampleGuard` and provides `Deref<Target=T>` --
the subscriber reads the struct directly from shared memory, zero copies,
zero deserialization.

---

## Comparison to iceoryx2

| Aspect | crossbar | iceoryx2 |
|---|---|---|
| Pool allocation | Treiber stack (ABA-safe via generation) | lock-free pool |
| Publication | seqlock ring of 8B descriptors | ring of descriptors |
| Read API | `SampleGuard<'a>` with `Deref<[u8]>` (borrowed) | typed zero-copy sample API |
| Typed API | `Pod` trait + `TypedSampleGuard<T>` | built-in typed pub/sub |
| Notification | futex-based smart wake (skip if no waiters) | event-based notification |
| Higher-level API | URI/topic names | typed service/topic configuration |

Crossbar's distinction is the combination of URI-addressed subscriptions with
typed zero-copy access. The SHM primitive itself is deliberately minimal --
Treiber stack + seqlock ring + refcounted guards.
