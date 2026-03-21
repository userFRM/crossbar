# Changelog

## 0.5.0 — 2026-03-20

### Added
- `Publisher::subscriber_count()` — query active subscriber count per topic.
- `crossbar_topic_subscriber_count()` FFI function.
- `WaitStrategy::MonitorWait` — Intel UMONITOR/UMWAIT on Alder Lake+ (x86_64 only).
- `PodBus<T: Pod>` — SPMC broadcast ring over shared memory.
- `BusSubscriber<T: Pod>` — per-consumer cursor for PodBus.
- Prefetch next ring slot on publish (PREFETCHT0/PRFM).
- `Stream` implements `Drop` (decrements subscriber count).
- `TE_SUBSCRIBER_COUNT` field at offset 0x78 in topic entry.

### Changed
- `Channel` field drop order fixed (subscription drops before subscriber).

## 0.4.0 — 2026-03-19

### Breaking Changes
- `Publisher::loan()` and `loan_typed()` now return `Result<_, Error>` instead of panicking on pool exhaustion.
- `Publisher::loan_pinned()` returns `Result<_, Error>` instead of panicking on active readers.
- `Loan::set_data()` and `set_len()` return `Result<_, Error>` instead of panicking on oversized data.
- `PinnedLoan::set_data()` and `set_len()` return `Result<_, Error>`.
- `Channel::send()` and `loan()` return `Result<_, Error>`.
- `Publisher::heartbeat()` and `Channel::heartbeat()` return `Result<_, Error>`.
- `Stream::try_recv_typed()` returns `None` on type size mismatch instead of panicking.
- `Publisher::register_typed()` returns `Err` on alignment > 8 instead of panicking.
- `Region::from_raw` is now `pub(crate)` (was `pub`).

### Added
- **CAS-based pinned writer sentinel**: `loan_pinned()` atomically sets `PINNED_WRITER_ACTIVE` via `compare_exchange`, preventing the publisher/subscriber data race that existed in 0.3.1. Subscribers use a CAS loop that rejects the sentinel + seqlock re-check after reader registration.
- **`PinnedLoan` has a `Drop` impl**: clears the writer sentinel on panic or early drop, preventing permanent subscriber lockout.
- **New `Error` variants**: `PoolExhausted`, `DataTooLarge`, `PinnedReadersActive`, `ClockError`.
- **Path traversal defense**: `is_valid_segment_name()` rejects `/`, `\`, `..`, and null bytes in segment names.
- **File permissions**: SHM and lock files created with `mode(0o600)` on Unix.
- **Config validation in `connect()`/`open()`**: validates magic, version, `ring_depth` power-of-2, all config bounds, checked arithmetic for region size.
- **`max_topics` capped at 4096**: prevents OOM from malicious SHM headers.
- **FFI null safety**: all 22 `extern "C"` functions check every pointer parameter for null before dereferencing.
- **FFI `from_raw_parts` fix**: `crossbar_publish` and `crossbar_channel_send` use `&[]` for zero-length payloads instead of potentially-null `from_raw_parts`.
- **Free-list corruption defense**: `alloc_block()` validates both `idx` and `next` pointer bounds.
- **Fully checked arithmetic**: `region_size_checked()` and `block_pool_offset_checked()` use `checked_mul`/`checked_add`.
- **Compile-time endianness check**: `compile_error!` on non-little-endian targets.
- **`Topic`** now derives `Clone, Copy, Debug, PartialEq, Eq`.
- **`Stream`** and **`Channel`** implement `Debug`.

### Changed
- **Pinned block_idx** read atomically (`AtomicU32::load(Acquire)`) instead of plain pointer read.
- **`release_block`** and **`free_block`** use runtime bounds checks (not `debug_assert`).
- **`Topic` validation** promoted from `debug_assert_eq` to runtime check.
- **`len as u32` truncation** guarded by runtime `assert!` (not `debug_assert!`).
- **`generate_publisher_id`** uses `AtomicU64` counter + PID + thread hash (was `Instant::now().elapsed()` which returned ~0).
- **Heartbeat errors propagated**: `update_heartbeat()` returns `Result<_, ClockError>`. `loan_preamble` only advances timer on success.
- **`blocking_recv`** deduplicated into a single generic method (was 3 copies).
- **`open_and_validate_region`** shared between `Publisher::open()` and `Subscriber::connect()`.
- **Magic number `1`** replaced with `TE_STATE_ACTIVE` constant in `subscribe()`.
- Layout constants gated with `#[cfg(feature = "std")]` — zero `#[allow(dead_code)]`.

### Fixed
- **CRITICAL**: Pinned publish TOCTOU data race (#13) — publisher used plain `load` instead of CAS, allowing concurrent reader/writer access.
- **HIGH**: Library panics on pool exhaustion, oversized data, type mismatch (#14).
- **HIGH**: Silent `unwrap_or_default()` on `SystemTime` in heartbeat (#16) — clock skew silently killed pub/sub fabric.
- **HIGH**: FFI null pointer UB on all entry points (#15).
- **HIGH**: Path traversal in segment names (#15) — `../../etc/cron.d/evil` was accepted.
- **HIGH**: Subscriber config not validated from SHM header (#15) — malicious `ring_depth=0` caused division by zero.
- C header `crossbar_publisher_heartbeat` return type updated from `void` to `int` to match Rust FFI.

## 0.3.1 — 2026-03-18

### Changed
- **Per-publisher block cache**: Each publisher caches up to 8 blocks locally, amortizing Treiber stack CAS (~87% cache hit rate, saves ~15–25 ns per publish under contention).
- **Single-publisher fast path**: `commit_to_ring` uses a plain store instead of CAS for seqlock acquisition when the publisher is the sole owner (saves ~10–15 ns per publish).
- **Notification merged into `WRITE_SEQ`**: Subscribers futex-wait on the low 32 bits of `WRITE_SEQ` instead of a separate `NOTIFY` counter. Eliminates 1 `fetch_add` per publish and 1 cache line miss on subscriber wakeup.
- **`compare_exchange_weak`**: All CAS loops (`alloc_block`, `free_block`, seqlock, refcount) now use weak CAS with `Err(current)` reuse — saves 4–8 cycles per retry on ARM.
- **Arc::drop pattern**: `Sample` and `TypedSample` use `fetch_sub(Release)` + `fence(Acquire)` on final decrement — saves ~4 cycles per drop on ARM.
- **Prefetch hints**: `PREFETCHW`/`PRFM` in `alloc_block` and `try_read_slot_raw` hide 10–40 ns of cache miss latency.
- **Non-temporal stores**: Payloads >= 2 MiB on x86-64 use streaming stores to avoid cache pollution.
- Removed redundant `refcount.store(0)` in `alloc_block` (already zero from `free_block`).
- `assert_eq!` → `debug_assert_eq!` for handle validation on hot path.
- Combined `SEVL` + `WFE` into single `asm!` block for robustness on aarch64.

### Changed
- **Safe pinned API**: `loan_pinned` and `try_recv_pinned` no longer require `unsafe`. A shared reader count in the topic entry prevents data races at runtime — `loan_pinned` panics if any `PinnedSample` is held. Cost: ~10 ns (26 ns total vs 16 ns unsafe), still 8.7× faster than iceoryx2.
- **Blocking pinned receive**: `recv_pinned()` and `recv_pinned_with(strategy)` — three-phase adaptive wait (spin → yield → futex), same as the safe API.

### Fixed
- **Block recycling**: `commit_to_ring` now stashes freed blocks in a per-region `last_freed` slot instead of returning them to the Treiber stack. The next `alloc_cached()` grabs the recycled block first — it's still warm in L1/L2 from the previous write. Eliminates the ~11 µs RFO (read-for-ownership) penalty at 1 MB payloads. E2E 1 MB: 45 µs → 33 µs.
- **Double-spin bug**: `recv_with` Adaptive mode spun 220 iterations before futex sleep (100 spin + 10 yield in `recv_with`, then another 100 + 10 inside `wait_until_not`). Now skips the internal spin when called from Adaptive phase 3.

### Removed
- `TE_NOTIFY` layout constant (notification merged into `WRITE_SEQ`).

## 0.3.0 — 2026-03-18

### Added
- **Multi-publisher**: `Publisher::open()` joins an existing region as a secondary publisher. Multiple publishers can write to the same or different topics concurrently. Atomic seq claiming via `fetch_add`, CAS-based ring slot locking, shared flock for region safety.
- **Bidirectional channel**: `Channel::listen()` / `connect()` — TCP-like bidirectional messaging over two pub/sub regions.
- **C/C++ FFI**: `include/crossbar.h` + `src/ffi.rs` behind the `ffi` feature. Opaque pointer API for publisher, subscriber, sample, and channel. Zero-allocation `crossbar_try_recv_into()` for hot paths.
- **CAS-based topic registration**: Safe concurrent `register()` from multiple publishers using three-state active field (`FREE → INIT → ACTIVE`).

### Changed
- `Config` is now `Copy`.
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
- TOCTOU in `Publisher::open()`: secondary publishers now hold a shared flock preventing region truncation.
- `Sample` `Debug` impl uses `core::fmt` consistently (was `std::fmt`).

## 0.2.0

Initial release. Single-publisher zero-copy pub/sub over shared memory with typed and untyped APIs, no_std protocol core, three-platform support (Linux, macOS, Windows).
