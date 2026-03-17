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
///
/// On aarch64: SEVL + WFE puts the core into a low-power state until a
/// cache-line invalidation event wakes it.
/// On x86: PAUSE yields the pipeline to the SMT sibling (~140 cycles).
#[inline(always)]
pub(crate) fn yield_hint() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("sevl", options(nomem, nostack));
        core::arch::asm!("wfe", options(nomem, nostack));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::hint::spin_loop();
}

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
                } else if iter < spin_iters + yield_iters {
                    // Phase 2: yield-spin -- yields pipeline.
                    yield_hint();
                }
                // Phase 3 (iter >= spin_iters + yield_iters) is handled
                // by the caller (recv_with) which falls through to the
                // OS sleep path (futex/WaitOnAddress/WFE).
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
        for i in 0..1000 {
            ws.wait(i);
        }
    }

    #[test]
    fn yield_spin_returns() {
        let ws = WaitStrategy::YieldSpin;
        for i in 0..100 {
            ws.wait(i);
        }
    }

    #[test]
    fn backoff_spin_returns() {
        let ws = WaitStrategy::BackoffSpin;
        for i in 0..20 {
            ws.wait(i);
        }
    }

    #[test]
    fn adaptive_phases() {
        let ws = WaitStrategy::Adaptive {
            spin_iters: 4,
            yield_iters: 4,
        };
        for i in 0..20 {
            ws.wait(i);
        }
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
        assert!(s.contains("BusySpin"));
    }
}
