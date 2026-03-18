# Changelog

## 0.3.1 — 2026-03-18

### Changed
- **Per-publisher block cache**: Each publisher caches up to 8 blocks locally, amortizing Treiber stack CAS (~87% cache hit rate, saves ~15–25 ns per publish under contention).
- **Single-publisher fast path**: `commit_to_ring` uses a plain store instead of CAS for seqlock acquisition when the publisher is the sole owner (saves ~10–15 ns per publish).
- **Notification merged into `WRITE_SEQ`**: Subscribers futex-wait on the low 32 bits of `WRITE_SEQ` instead of a separate `NOTIFY` counter. Eliminates 1 `fetch_add` per publish and 1 cache line miss on subscriber wakeup.
- **`compare_exchange_weak`**: All CAS loops (`alloc_block`, `free_block`, seqlock, refcount) now use weak CAS with `Err(current)` reuse — saves 4–8 cycles per retry on ARM.
- **Arc::drop pattern**: `SampleGuard` and `TypedSampleGuard` use `fetch_sub(Release)` + `fence(Acquire)` on final decrement — saves ~4 cycles per drop on ARM.
- **Prefetch hints**: `PREFETCHW`/`PRFM` in `alloc_block` and `try_read_slot_raw` hide 10–40 ns of cache miss latency.
- **Non-temporal stores**: Payloads >= 2 MiB on x86-64 use streaming stores to avoid cache pollution.
- Removed redundant `refcount.store(0)` in `alloc_block` (already zero from `free_block`).
- `assert_eq!` → `debug_assert_eq!` for handle validation on hot path.
- Combined `SEVL` + `WFE` into single `asm!` block for robustness on aarch64.

### Changed
- **Safe pinned API**: `loan_pinned` and `try_recv_pinned` no longer require `unsafe`. A shared reader count in the topic entry prevents data races at runtime — `loan_pinned` panics if any `PinnedGuard` is held. Cost: ~10 ns (26 ns total vs 16 ns unsafe), still 8.7× faster than iceoryx2.
- **Blocking pinned receive**: `recv_pinned()` and `recv_pinned_with(strategy)` — three-phase adaptive wait (spin → yield → futex), same as the safe API.

### Fixed
- **Block recycling**: `commit_to_ring` now stashes freed blocks in a per-region `last_freed` slot instead of returning them to the Treiber stack. The next `alloc_cached()` grabs the recycled block first — it's still warm in L1/L2 from the previous write. Eliminates the ~11 µs RFO (read-for-ownership) penalty at 1 MB payloads. E2E 1 MB: 45 µs → 33 µs.
- **Double-spin bug**: `recv_with` Adaptive mode spun 220 iterations before futex sleep (100 spin + 10 yield in `recv_with`, then another 100 + 10 inside `wait_until_not`). Now skips the internal spin when called from Adaptive phase 3.

### Removed
- `TE_NOTIFY` layout constant (notification merged into `WRITE_SEQ`).

## 0.3.0 — 2026-03-18

### Added
- **Multi-publisher**: `ShmPublisher::open()` joins an existing region as a secondary publisher. Multiple publishers can write to the same or different topics concurrently. Atomic seq claiming via `fetch_add`, CAS-based ring slot locking, shared flock for region safety.
- **Bidirectional channel**: `ShmChannel::listen()` / `connect()` — TCP-like bidirectional messaging over two pub/sub regions.
- **C/C++ FFI**: `include/crossbar.h` + `src/ffi.rs` behind the `ffi` feature. Opaque pointer API for publisher, subscriber, sample, and channel. Zero-allocation `crossbar_try_recv_into()` for hot paths.
- **CAS-based topic registration**: Safe concurrent `register()` from multiple publishers using three-state active field (`FREE → INIT → ACTIVE`).

### Changed
- `PubSubConfig` is now `Copy`.
- `ring_depth` must be a power of 2 (enables bitmask instead of division — saves ~70 ns/recv).
- Seqlock uses `SEQ_WRITING` (u64::MAX) sentinel and CAS acquisition instead of simple store-based open/close.
- Heartbeat uses `fetch_max` so any live publisher keeps the region alive.
- Subscriber `try_recv` uses ring-window scan to handle out-of-order multi-publisher commits.
- Sequence numbers are claimed atomically at commit time via `fetch_add` (moved from loan time).
- Spin hints use `SEVL+WFE` on aarch64 (via `yield_hint()`) instead of `core::hint::spin_loop()` which emits the weaker `YIELD` instruction.
- CI now tests `--features ffi`.

### Removed
- `assets/` and `scripts/` directories (stale benchmark chart artifacts).

### Fixed
- TOCTOU in `ShmPublisher::open()`: secondary publishers now hold a shared flock preventing region truncation.
- `SampleGuard` `Debug` impl uses `core::fmt` consistently (was `std::fmt`).

## 0.2.0

Initial release. Single-publisher zero-copy pub/sub over shared memory with typed and untyped APIs, no_std protocol core, three-platform support (Linux, macOS, Windows).
