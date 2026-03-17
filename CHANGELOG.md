# Changelog

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
