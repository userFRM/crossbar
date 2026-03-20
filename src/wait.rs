// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Wait strategies for blocking receive operations.
//!
//! [`WaitStrategy`] controls how a subscriber thread waits when no new sample
//! is available. The spin/yield phases use platform-specific hints; the OS
//! phase uses `futex` (Linux), `WaitOnAddress` (Windows), or `WFE`
//! (macOS/aarch64).
//!
//! | Strategy | Latency | CPU usage | Best for |
//! |---|---|---|---|
//! | `BusySpin` | Lowest (~0 ns wakeup) | 100% core | Dedicated, pinned cores |
//! | `YieldSpin` | Low (~30 ns on x86) | High | Shared cores, SMT |
//! | `BackoffSpin` | Medium (exponential) | Decreasing | Background consumers |
//! | `Adaptive` | Auto-scaling | Varies | General purpose |
//! | `MonitorWait` | Near-zero (~30 ns on Intel) | Near-zero | Intel Alder Lake+ |
//!
//! # Platform-specific optimizations
//!
//! On **aarch64**, `YieldSpin` and `BackoffSpin` use the `WFE` (Wait For
//! Event) instruction instead of `core::hint::spin_loop()` (which maps to
//! `YIELD`). `WFE` puts the core into a low-power state until an event --
//! such as a cache line invalidation from the publisher's store -- wakes it.
//! The `SEVL` + `WFE` pattern is used: `SEVL` sets the local event register
//! so the first `WFE` doesn't block unconditionally.
//!
//! On **x86/x86_64**, `core::hint::spin_loop()` emits `PAUSE`, which is the
//! standard spin-wait hint (~140 cycles on Skylake+).
//!
//! On **x86_64** with WAITPKG (Intel Tremont+, Alder Lake+), `MonitorWait`
//! uses `UMONITOR`/`UMWAIT` to monitor a cache line for writes, entering
//! C0.1 low-power state with ~30 ns wakeup latency. Falls back to `PAUSE`
//! on CPUs without WAITPKG support.

/// Wait strategy for blocking `recv()` on shared-memory subscriptions.
///
/// Controls how the subscriber waits when no new sample is available.
/// The spin/yield phases use platform-specific hints; the OS phase uses
/// `futex` (Linux), `WaitOnAddress` (Windows), or `WFE` (macOS/aarch64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStrategy {
    /// Pure busy-spin. No hint instruction. Minimum wakeup latency
    /// but consumes 100% of one CPU core. Use on dedicated, pinned cores.
    BusySpin,

    /// PAUSE (x86) or SEVL+WFE (aarch64) per iteration. Yields the CPU
    /// pipeline to the SMT sibling and reduces power vs `BusySpin`.
    YieldSpin,

    /// Exponential backoff with platform yield hint. Starts with bare
    /// spins, then escalates to PAUSE/WFE-based spins with increasing
    /// delays. Good for consumers that may be idle for extended periods.
    BackoffSpin,

    /// Three-phase: bare spin -> yield -> OS sleep. Default.
    ///
    /// Phase 1: bare spin for `spin_iters` iterations (fastest wakeup).
    /// Phase 2: PAUSE/WFE spin for `yield_iters` iterations.
    /// Phase 3: OS-assisted sleep (`futex`/`WaitOnAddress`/`WFE`).
    Adaptive {
        /// Bare-spin iterations before yield phase.
        spin_iters: u32,
        /// Yield iterations before OS sleep phase.
        yield_iters: u32,
    },

    /// Intel UMONITOR/UMWAIT on Alder Lake+. Monitors a cache line for
    /// writes, entering C0.1 low-power state. Falls back to PAUSE if the
    /// CPU doesn't support WAITPKG.
    ///
    /// In `blocking_recv`, the futex address is monitored via UMONITOR so
    /// that a publisher's store wakes the subscriber with ~30 ns latency
    /// and near-zero power consumption.
    ///
    /// When called through the generic `wait()` method (which has no
    /// address to monitor), falls back to PAUSE.
    #[cfg(target_arch = "x86_64")]
    MonitorWait,
}

impl Default for WaitStrategy {
    fn default() -> Self {
        WaitStrategy::Adaptive {
            spin_iters: 100,
            yield_iters: 10,
        }
    }
}

/// Platform-specific yield hint.
///
/// On aarch64: SEVL + WFE puts the core into a low-power state until a
/// cache-line invalidation event wakes it.
/// On x86: PAUSE yields the pipeline to the SMT sibling (~140 cycles).
#[cfg(feature = "std")]
#[inline(always)]
pub(crate) fn yield_hint() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("sevl", "wfe", options(nomem, nostack));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::hint::spin_loop();
}

// ---------------------------------------------------------------------------
// WAITPKG (UMONITOR / UMWAIT / TPAUSE) — Intel Tremont+, Alder Lake+
// ---------------------------------------------------------------------------
//
// CPUID leaf 7, sub-leaf 0, ECX bit 5 indicates WAITPKG support.
// The result is cached in a static AtomicU8 (racy init -- benign data race,
// worst case is redundant CPUID calls on first access).

/// Cached WAITPKG support flag: 0 = unknown, 1 = unsupported, 2 = supported.
#[cfg(all(target_arch = "x86_64", feature = "std"))]
static WAITPKG_SUPPORT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Returns `true` if the CPU supports WAITPKG (UMONITOR/UMWAIT/TPAUSE).
#[cfg(all(target_arch = "x86_64", feature = "std"))]
#[inline]
fn waitpkg_supported() -> bool {
    let cached = WAITPKG_SUPPORT.load(core::sync::atomic::Ordering::Relaxed);
    if cached != 0 {
        return cached == 2;
    }
    // CPUID leaf 7, sub-leaf 0, ECX bit 5
    let result = core::arch::x86_64::__cpuid_count(7, 0);
    let supported = result.ecx & (1 << 5) != 0;
    WAITPKG_SUPPORT.store(
        if supported { 2 } else { 1 },
        core::sync::atomic::Ordering::Relaxed,
    );
    supported
}

// SAFETY wrappers for UMONITOR/UMWAIT instructions.
// These are encoded via raw bytes because stable Rust doesn't expose them
// as intrinsics yet.
//
// UMONITOR: sets up address monitoring (F3 0F AE /6)
// UMWAIT:   wait until store to monitored line or timeout (F2 0F AE /6)
//
// EDX:EAX = absolute TSC deadline. The instruction exits when either:
//   (a) a store hits the monitored cache line (UMWAIT only), or
//   (b) TSC >= deadline, or
//   (c) an OS-configured timeout (IA32_UMWAIT_CONTROL MSR) fires.
//
// We set the deadline ~100us in the future -- long enough to actually
// enter a low-power state, short enough to bound worst-case latency
// if the wakeup event is missed (e.g., the store happened between
// UMONITOR and UMWAIT).
#[cfg(all(target_arch = "x86_64", feature = "std"))]
mod umwait {
    /// Read the TSC and return a deadline ~100us in the future.
    /// On a 3 GHz CPU, 100us ~ 300,000 cycles.
    #[inline(always)]
    fn deadline_100us() -> (u32, u32) {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let deadline = tsc.wrapping_add(300_000); // ~100us at 3 GHz
        (deadline as u32, (deadline >> 32) as u32) // (eax, edx)
    }

    /// Set up monitoring on the cache line containing `addr`.
    /// The CPU will track writes to this line until UMWAIT is called.
    #[inline(always)]
    pub(super) unsafe fn umonitor(addr: *const u8) {
        // UMONITOR rax: F3 0F AE /6 (with rax)
        core::arch::asm!(
            ".byte 0xf3, 0x0f, 0xae, 0xf0", // UMONITOR rax
            in("rax") addr,
            options(nostack, preserves_flags),
        );
    }

    /// Wait for a write to the monitored address or timeout.
    /// `ctrl` = 0 for C0.2 (deeper sleep), 1 for C0.1 (lighter sleep).
    /// Returns quickly (~30 ns) when the monitored cache line is written.
    #[inline(always)]
    pub(super) unsafe fn umwait(ctrl: u32) {
        let (lo, hi) = deadline_100us();
        // UMWAIT ecx: F2 0F AE /6 (with ecx for control)
        // edx:eax = absolute TSC deadline
        core::arch::asm!(
            ".byte 0xf2, 0x0f, 0xae, 0xf1", // UMWAIT ecx
            in("ecx") ctrl,
            in("edx") hi,
            in("eax") lo,
            options(nostack, preserves_flags),
        );
    }
}

/// Execute one UMONITOR + UMWAIT cycle on `addr`. Called from
/// `blocking_recv` where the futex address is available.
///
/// If WAITPKG is not supported, falls back to PAUSE.
#[cfg(all(target_arch = "x86_64", feature = "std"))]
#[inline]
pub(crate) fn monitor_wait_on_address(addr: *const u8) {
    if waitpkg_supported() {
        unsafe {
            umwait::umonitor(addr);
            umwait::umwait(1); // C0.1 -- lighter sleep, faster wakeup
        }
    } else {
        core::hint::spin_loop();
    }
}

#[cfg(feature = "std")]
impl WaitStrategy {
    /// Execute one wait iteration. Called by `recv_with` on each loop when
    /// `try_recv` returns `None`.
    ///
    /// `iter` is the zero-based iteration count since the last successful
    /// receive -- it drives phase transitions in `Adaptive` and `BackoffSpin`.
    #[inline]
    pub(crate) fn wait(&self, iter: u32) {
        match self {
            WaitStrategy::BusySpin => {
                // No hint -- pure busy loop. Fastest wakeup, highest power.
            }
            WaitStrategy::YieldSpin => {
                yield_hint();
            }
            WaitStrategy::BackoffSpin => {
                // Exponential backoff: more iterations as we wait longer.
                // On aarch64: WFE sleeps until a cache-line event, making
                // each iteration near-zero power. On x86: PAUSE yields the
                // pipeline with ~140 cycle delay per iteration.
                let pauses = 1u32.wrapping_shl(iter.min(6)); // 1, 2, 4, 8, 16, 32, 64
                for _ in 0..pauses {
                    yield_hint();
                }
            }
            WaitStrategy::Adaptive {
                spin_iters,
                yield_iters,
            } => {
                if iter < *spin_iters {
                    // Phase 1: bare spin -- fastest wakeup.
                } else if iter < spin_iters.saturating_add(*yield_iters) {
                    // Phase 2: yield-spin -- yields pipeline.
                    yield_hint();
                }
                // Phase 3 (iter >= spin_iters + yield_iters) is handled
                // by the caller (recv_with) which falls through to the
                // OS sleep path (futex/WaitOnAddress/WFE).
            }
            #[cfg(target_arch = "x86_64")]
            WaitStrategy::MonitorWait => {
                // Generic wait() has no address to monitor. Fall back to
                // PAUSE. The real UMONITOR+UMWAIT path is in blocking_recv
                // where the futex address is available.
                core::hint::spin_loop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_adaptive() {
        let ws = WaitStrategy::default();
        assert_eq!(
            ws,
            WaitStrategy::Adaptive {
                spin_iters: 100,
                yield_iters: 10,
            }
        );
    }

    #[test]
    fn busy_spin_returns_immediately() {
        let ws = WaitStrategy::BusySpin;
        let start = std::time::Instant::now();
        for i in 0..1000 {
            ws.wait(i);
        }
        // BusySpin with 1000 iterations should complete in well under 100ms
        assert!(start.elapsed().as_millis() < 100, "BusySpin took too long");
    }

    #[test]
    fn yield_spin_returns() {
        let ws = WaitStrategy::YieldSpin;
        let start = std::time::Instant::now();
        for i in 0..100 {
            ws.wait(i);
        }
        assert!(start.elapsed().as_millis() < 500, "YieldSpin took too long");
    }

    #[test]
    fn backoff_spin_returns() {
        let ws = WaitStrategy::BackoffSpin;
        let start = std::time::Instant::now();
        for i in 0..20 {
            ws.wait(i);
        }
        assert!(
            start.elapsed().as_millis() < 500,
            "BackoffSpin took too long"
        );
    }

    #[test]
    fn adaptive_phases() {
        let ws = WaitStrategy::Adaptive {
            spin_iters: 4,
            yield_iters: 4,
        };
        let start = std::time::Instant::now();
        for i in 0..20 {
            ws.wait(i);
        }
        assert!(start.elapsed().as_millis() < 500, "Adaptive took too long");
    }

    #[test]
    fn adaptive_saturating_add_no_overflow() {
        // M7: u32::MAX + 1 should not wrap
        let ws = WaitStrategy::Adaptive {
            spin_iters: u32::MAX,
            yield_iters: 1,
        };
        // Should not panic from overflow
        ws.wait(u32::MAX);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn monitor_wait_returns() {
        let ws = WaitStrategy::MonitorWait;
        let start = std::time::Instant::now();
        for i in 0..100 {
            ws.wait(i);
        }
        assert!(
            start.elapsed().as_millis() < 500,
            "MonitorWait took too long"
        );
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn monitor_wait_on_address_returns() {
        use core::sync::atomic::AtomicU32;
        let val = AtomicU32::new(42);
        let addr = &val as *const AtomicU32 as *const u8;
        let start = std::time::Instant::now();
        for _ in 0..10 {
            super::monitor_wait_on_address(addr);
        }
        assert!(
            start.elapsed().as_secs() < 5,
            "monitor_wait_on_address took too long"
        );
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn waitpkg_detection_is_deterministic() {
        // Calling waitpkg_supported() multiple times must return the same result
        let a = super::waitpkg_supported();
        let b = super::waitpkg_supported();
        assert_eq!(a, b);
    }

    #[test]
    fn clone_and_copy() {
        let ws = WaitStrategy::BusySpin;
        let ws2 = ws;
        #[allow(clippy::clone_on_copy)]
        let ws3 = ws.clone();
        assert_eq!(ws, ws2);
        assert_eq!(ws, ws3);
    }

    #[test]
    fn debug_format() {
        let ws = WaitStrategy::BusySpin;
        let s = alloc::format!("{ws:?}");
        assert_eq!(s, "BusySpin");
    }
}
