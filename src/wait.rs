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
#[inline(always)]
pub(crate) fn yield_hint() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("sevl", "wfe", options(nomem, nostack));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::hint::spin_loop();
}

impl WaitStrategy {
    /// Execute one wait iteration.
    #[inline]
    pub(crate) fn wait(&self, iter: u32) {
        match self {
            WaitStrategy::BusySpin => {}
            WaitStrategy::YieldSpin => {
                yield_hint();
            }
            WaitStrategy::BackoffSpin => {
                let pauses = 1u32.wrapping_shl(iter.min(6));
                for _ in 0..pauses {
                    yield_hint();
                }
            }
            WaitStrategy::Adaptive {
                spin_iters,
                yield_iters,
            } => {
                if iter < *spin_iters {
                    // Phase 1: bare spin
                } else if iter < spin_iters.saturating_add(*yield_iters) {
                    yield_hint();
                }
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
        let ws = WaitStrategy::Adaptive {
            spin_iters: u32::MAX,
            yield_iters: 1,
        };
        ws.wait(u32::MAX);
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
        let s = format!("{ws:?}");
        assert_eq!(s, "BusySpin");
    }
}
