# Architecture: Two Fast Paths

Crossbar is a workspace with two crates, each providing a distinct fast path:

- **`crossbar-inproc`** — in-process pub/sub via `Bus<T>` with `Arc<T>` fan-out
- **`crossbar-ipc`** — zero-copy pub/sub over shared memory (`/dev/shm`)

Both crates solve the same problem — **named pub/sub channels at hardware
speed** — at different scopes. `crossbar-inproc` operates within a single
process (threads share `Arc<T>`). `crossbar-ipc` operates across processes
(they share mmap'd memory). Same mental model, same topic naming, same
lossy-ring semantics. The only thing that changes is whether you cross an
address space boundary.

---

## Side by side

```
  crossbar-inproc (57 ns)             crossbar-ipc (58 ns)
  ========================            =======================

  TopicHandle::publish()              ShmLoan::publish()
          |                                   |
          v                                   v
  ArcSwap::load()                     alloc block (Treiber CAS)
  [load subscriber list]              write payload into SHM
          |                                   |
          v                                   v
  +---+---+---+                       set refcount = 1
  |   |   |   |                       seqlock open (seq=0)
  v   v   v   v                       write 8B descriptor
  Ring Ring Ring Ring                  seqlock close (seq=N)
  [Arc::clone per sub]                bump write_seq
  [CAS push into SPSC]                free old block if rc=0
          |                           smart wake (futex)
          v                                   |
  Subscription::try_recv()                    v
  [CAS pop from SPSC]                Subscription::try_recv()
          |                           [load write_seq]
          v                           [seqlock check 1]
  Arc<T> — user's message             [read block_idx + data_len]
                                      [CAS refcount increment]
                                      [seqlock check 2]
                                              |
                                              v
                                      SampleGuard — Deref<[u8]>
                                      [reads directly from mmap]
```

---

## In-process path (`crossbar-inproc`)

The `Bus<T>` manages topics and subscriptions. Each subscriber gets a dedicated
lock-free SPSC ring buffer. There is no Mutex on the hot path.

### Publish flow

1. `TopicHandle::publish()` calls `ArcSwap::load()` to get the current subscriber
   list — this is a lock-free atomic load, no hash lookup.
2. For each subscriber, `Arc::clone()` the message and push into that subscriber's
   dedicated SPSC ring.
3. If a ring is full, the producer CAS-advances the tail to drop the oldest message
   (lossy semantics). The CAS loop handles the concurrent-pop race.
4. Bump the notification counter (`AtomicU32`). If any subscriber is blocked in
   `recv()`, notify via condvar.

### Ring buffer internals

```
  Ring<T> (lock-free SPSC, cache-line padded)
  ============================================

  head: CacheAligned<AtomicU64>       [64-byte aligned, producer-owned]
  tail: CacheAligned<AtomicU64>       [64-byte aligned, shared CAS]
  slots: Box<[UnsafeCell<MaybeUninit<Arc<T>>>]>

  Push (producer):                    Pop (consumer):
  +--------------------------+        +-------------------------+
  | load head (Relaxed)      |        | load tail (Relaxed)     |
  | load tail (Acquire)      |        | load head (Acquire)     |
  | if full:                 |        | if empty: return None   |
  |   CAS tail++ (drop old)  |        | CAS tail++ (AcqRel)    |
  | write slot[head % cap]   |        | read slot[tail % cap]   |
  | store head++ (Release)   |        | return Arc<T>           |
  +--------------------------+        +-------------------------+
```

Head and tail live on separate cache lines (`#[repr(align(64))]`) to prevent
false sharing between producer and consumer cores.

### Receive flow

`Subscription::recv()` uses a three-phase wait:

1. **Spin** (32 iterations) — `spin_loop()` hint, lowest latency for burst traffic
2. **Yield** (32 iterations) — `thread::yield_now()`, gives other threads a chance
3. **Condvar** — parks the thread, woken by the publisher's smart-wake check

`try_recv()` is a single `ring.pop()` — CAS on the tail index.

### Subscribe / unsubscribe (cold path)

Adding or removing subscribers is serialized by a `Mutex`. The subscriber list
is replaced atomically via `ArcSwap::store()`, so the publish hot path never
blocks on subscribe/unsubscribe operations.

---

## Shared-memory path (`crossbar-ipc`)

The SHM path moves data between processes at O(1) cost regardless of payload
size. Transfer writes only 8 bytes (block index + data length) to a ring slot.

### Borrowed `SampleGuard`

`SampleGuard<'a>` borrows `&'a Region` (no `Arc::clone` per receive) and holds
a block index and length. It implements `Deref<Target=[u8]>` — subscribers read
directly from the mmap'd region. The block is held alive by atomic refcounting;
the guard decrements the refcount on drop, freeing the block when it hits zero.

### Seqlock ring

Each topic has a ring of `(seq, block_idx, data_len)` entries. The publisher
uses a seqlock protocol:

1. Invalidate the slot's sequence to 0 (seqlock open)
2. Write block_idx and data_len via `AtomicU32` stores (`Relaxed` ordering) —
   bracketed by the seqlock sequence stores
3. Store the new sequence (seqlock close, Release ordering)

The subscriber:

1. Loads the slot sequence (Acquire) — skips if 0 or mismatched
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
cache misses on the subscriber's polling path.

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

1. Load `write_seq` — if unchanged, return `None`
2. Compute target slot from sequence
3. Seqlock-read: check seq, read block_idx + data_len (AtomicU32 Relaxed)
4. CAS-increment block refcount (Acquire load, AcqRel CAS)
5. Seqlock re-check — undo refcount if slot was overwritten
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

## Comparison to iceoryx2

The `crossbar-ipc` crate is the closest analogue to `iceoryx2`.

| Aspect | crossbar-ipc | iceoryx2 |
|---|---|---|
| Pool allocation | Treiber stack (ABA-safe via generation) | lock-free pool |
| Publication | seqlock ring of 8B descriptors | ring of descriptors |
| Read API | `SampleGuard<'a>` with `Deref<[u8]>` (borrowed) | typed zero-copy sample API |
| Notification | futex-based smart wake (skip if no waiters) | event-based notification |
| Higher-level API | URI/topic names | typed service/topic configuration |

Crossbar's distinction is the API layer: URI-addressed subscriptions and a
separate in-process `Bus<T>` for same-process fan-out. The SHM primitive itself
is deliberately minimal — Treiber stack + seqlock ring + refcounted guards.
