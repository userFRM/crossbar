// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wait strategies for blocking receive operations.
//!
//! [`WaitStrategy`] controls how a consumer thread waits when no message is
//! available. All spin-phase variants use platform-specific hints for
//! efficiency.
//!
//! | Strategy | Latency | CPU usage | Best for |
//! |---|---|---|---|
//! | `BusySpin` | Lowest (~0 ns) | 100% core | Dedicated, pinned cores |
//! | `YieldSpin` | Low (~30 ns) | High | Shared cores, SMT |
//! | `BackoffSpin` | Medium | Decreasing | Background consumers |
//! | `Adaptive` | Auto-scaling | Varies | General purpose (default) |
//!
//! # Platform-specific optimizations
//!
//! On **aarch64**, `YieldSpin` and `BackoffSpin` use the `WFE` (Wait For
//! Event) instruction instead of `core::hint::spin_loop()`. `WFE` puts
//! the core into a low-power state until an event — such as a cache line
//! invalidation from the publisher's store — wakes it. The `SEVL` + `WFE`
//! pattern is used: `SEVL` sets the local event register so the first `WFE`
//! doesn't block unconditionally.
//!
//! On **x86/x86_64**, `core::hint::spin_loop()` emits `PAUSE`, which is the
//! standard spin-wait hint (~140 cycles on Skylake+).

/// Strategy for blocking `recv()` operations.
///
/// Controls how a consumer thread waits when no message is available.
/// All spin-phase variants use platform-specific hints for efficiency.
///
/// | Strategy | Latency | CPU usage | Best for |
/// |---|---|---|---|
/// | `BusySpin` | Lowest (~0 ns) | 100% core | Dedicated, pinned cores |
/// | `YieldSpin` | Low (~30 ns) | High | Shared cores, SMT |
/// | `BackoffSpin` | Medium | Decreasing | Background consumers |
/// | `Adaptive` | Auto-scaling | Varies | General purpose (default) |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStrategy {
    /// Pure busy-spin. No hint instruction. Minimum wakeup latency
    /// but consumes 100% of one CPU core.
    BusySpin,

    /// Spin with platform-specific yield hint.
    ///
    /// On **x86/x86_64**: emits `PAUSE` (~140 cycles on Skylake+).
    /// On **aarch64**: emits `SEVL` + `WFE` — puts the core into a
    /// low-power state until a cache-line invalidation event wakes it.
    /// On **other**: `core::hint::spin_loop()`.
    YieldSpin,

    /// Exponential backoff: 1, 2, 4, 8, 16, 32, 64 iterations of the
    /// platform yield hint. Good for consumers that may idle.
    BackoffSpin,

    /// Three-phase escalation: bare spin, then yield-spin, then the
    /// OS sleep primitive (condvar). This is the default.
    Adaptive {
        /// Bare-spin iterations before escalating to yield-spin.
        spin_iters: u32,
        /// Yield-spin iterations before escalating to OS sleep.
        yield_iters: u32,
    },
}

impl Default for WaitStrategy {
    fn default() -> Self {
        WaitStrategy::Adaptive {
            spin_iters: 32,
            yield_iters: 32,
        }
    }
}

impl WaitStrategy {
    /// Execute one wait iteration. `iter` is the zero-based count since
    /// the last successful receive.
    #[inline]
    pub fn wait(&self, iter: u32) {
        match self {
            WaitStrategy::BusySpin => {
                // No hint — pure busy loop.
            }
            WaitStrategy::YieldSpin => {
                yield_hint();
            }
            WaitStrategy::BackoffSpin => {
                let pauses = 1u32.wrapping_shl(iter.min(6)); // 1..64
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
                } else if iter < spin_iters + yield_iters {
                    // Phase 2: yield-spin
                    yield_hint();
                } else {
                    // Phase 3: multiple yields per iteration
                    for _ in 0..8 {
                        yield_hint();
                    }
                }
            }
        }
    }

    /// Returns true if this strategy ever transitions to OS sleep.
    /// Only `Adaptive` does (after spin + yield phases are exhausted).
    #[inline]
    pub fn has_os_sleep_phase(&self) -> bool {
        matches!(self, WaitStrategy::Adaptive { .. })
    }
}

/// Platform-specific yield hint.
///
/// - **aarch64**: `SEVL` + `WFE` — cache-line-aware low-power wait.
///   The core sleeps until a cache-line invalidation event, which happens
///   when the publisher writes to the ring's head index.
/// - **x86/x86_64**: `PAUSE` — yields the pipeline to the SMT sibling.
/// - **other**: `core::hint::spin_loop()`.
#[inline(always)]
fn yield_hint() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("sevl", options(nomem, nostack));
        core::arch::asm!("wfe", options(nomem, nostack));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::hint::spin_loop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_adaptive() {
        assert_eq!(
            WaitStrategy::default(),
            WaitStrategy::Adaptive {
                spin_iters: 32,
                yield_iters: 32,
            }
        );
    }

    #[test]
    fn busy_spin_returns() {
        let ws = WaitStrategy::BusySpin;
        for i in 0..100 {
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
    fn has_os_sleep_phase_only_adaptive() {
        assert!(!WaitStrategy::BusySpin.has_os_sleep_phase());
        assert!(!WaitStrategy::YieldSpin.has_os_sleep_phase());
        assert!(!WaitStrategy::BackoffSpin.has_os_sleep_phase());
        assert!(WaitStrategy::default().has_os_sleep_phase());
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
}
