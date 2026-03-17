// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Atomic-wait notification helpers for cross-process synchronization.
//!
//! Provides a three-phase wait strategy (spin, yield, futex/poll) and
//! wake primitives. On Linux, uses the `futex(2)` syscall directly.
//! On Windows, uses `WaitOnAddress`/`WakeByAddress*` from the Win32 API.
//! On other Unix platforms, falls back to short polling sleeps.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Waits until `addr` no longer holds `current`, using a three-phase
/// strategy: spin (100 iterations), yield (10 iterations), then
/// platform-specific park (`futex_wait` on Linux, polling on macOS).
///
/// Returns the new value on success, or `Err(())` on timeout.
///
/// # Errors
///
/// Returns `Err(())` if `timeout` elapses before the value changes.
pub fn wait_until_not(addr: &AtomicU32, current: u32, timeout: Duration) -> Result<u32, ()> {
    const SPIN_ITERS: u32 = 100;
    const YIELD_ITERS: u32 = 10;

    // Phase 1: spin
    for _ in 0..SPIN_ITERS {
        let val = addr.load(Ordering::Acquire);
        if val != current {
            return Ok(val);
        }
        // Platform-specific yield hint:
        // aarch64: SEVL + WFE puts the core into a low-power state until a
        // cache-line invalidation event (from the publisher's store) wakes it.
        // x86: PAUSE yields the pipeline to the SMT sibling (~140 cycles).
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("sevl", options(nomem, nostack));
            core::arch::asm!("wfe", options(nomem, nostack));
        }
        #[cfg(not(target_arch = "aarch64"))]
        core::hint::spin_loop();
    }

    // Phase 2: yield
    for _ in 0..YIELD_ITERS {
        let val = addr.load(Ordering::Acquire);
        if val != current {
            return Ok(val);
        }
        std::thread::yield_now();
    }

    // Phase 3: platform-specific park
    // Use the caller's timeout as the futex chunk so sub-10ms stale timeouts
    // are honored. Cap at 10ms to keep default latency reasonable.
    let chunk = timeout.min(Duration::from_millis(10));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let val = addr.load(Ordering::Acquire);
        if val != current {
            return Ok(val);
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        platform::futex_wait(addr, current, Some(chunk));
    }
}

/// Wake all threads waiting on `addr`.
pub fn wake_all(addr: &AtomicU32) {
    platform::futex_wake(addr, i32::MAX);
}

#[cfg(target_os = "linux")]
mod platform {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// Blocks the calling thread until `*addr != expected` or `timeout` elapses.
    ///
    /// Wraps the Linux `futex(FUTEX_WAIT)` syscall. Spurious wakeups are
    /// possible and handled by the caller's retry loop.
    ///
    /// # Safety
    ///
    /// Uses `libc::syscall` with a pointer to a valid `AtomicU32`. The pointer
    /// is derived via `std::ptr::from_ref` and remains valid for the duration
    /// of the syscall.
    #[allow(unsafe_code)]
    pub fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) {
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs().cast_signed(),
            tv_nsec: d.subsec_nanos().into(),
        });
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                std::ptr::from_ref::<AtomicU32>(addr),
                libc::FUTEX_WAIT,
                expected,
                ts.as_ref()
                    .map_or(std::ptr::null(), std::ptr::from_ref::<libc::timespec>),
                std::ptr::null::<u32>(),
                0u32,
            );
        }
    }

    /// Wakes up to `count` threads blocked in `futex_wait` on `addr`.
    ///
    /// # Safety
    ///
    /// Uses `libc::syscall` with a pointer to a valid `AtomicU32`.
    #[allow(unsafe_code)]
    pub fn futex_wake(addr: &AtomicU32, count: i32) {
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                std::ptr::from_ref::<AtomicU32>(addr),
                libc::FUTEX_WAKE,
                count,
                std::ptr::null::<libc::timespec>(),
                std::ptr::null::<u32>(),
                0u32,
            );
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod platform {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// Fallback wait for non-Linux Unix (e.g. macOS).
    ///
    /// On aarch64 (Apple Silicon): uses WFE for cache-line-aware low-power
    /// wait. WFE wakes on cache-line invalidation (~30 ns) vs sleep(1 ms).
    /// On x86: falls back to short sleep (no WFE equivalent).
    pub fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) {
        let deadline = timeout.map(|d| std::time::Instant::now() + d);

        #[cfg(target_arch = "aarch64")]
        {
            // WFE-based spin with backoff
            let mut backoff = 1u32;
            loop {
                let val = addr.load(std::sync::atomic::Ordering::Acquire);
                if val != expected {
                    return;
                }
                if let Some(dl) = deadline {
                    if std::time::Instant::now() >= dl {
                        return;
                    }
                }
                for _ in 0..backoff {
                    unsafe {
                        core::arch::asm!("wfe", options(nomem, nostack));
                    }
                }
                backoff = (backoff * 2).min(64);
            }
        }

        #[cfg(not(target_arch = "aarch64"))]
        {
            let _ = (addr, expected, deadline);
            // x86 macOS: no WFE equivalent, use short sleep
            let sleep_time = timeout
                .map(|d| d.min(Duration::from_millis(1)))
                .unwrap_or(Duration::from_millis(1));
            std::thread::sleep(sleep_time);
        }
    }

    /// On non-Linux: WFE wakes on cache-line invalidation automatically.
    /// The publisher's atomic store to the notify counter invalidates
    /// the subscriber's cached copy, generating the event.
    /// No explicit wake needed.
    pub fn futex_wake(_addr: &AtomicU32, _count: i32) {}
}

#[cfg(windows)]
mod platform {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    pub fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) {
        let timeout_ms = timeout.map(|d| d.as_millis() as u32).unwrap_or(u32::MAX);
        if timeout_ms == 0 {
            return;
        }
        unsafe {
            windows_sys::Win32::System::Threading::WaitOnAddress(
                std::ptr::from_ref(addr).cast(),
                std::ptr::from_ref(&expected).cast(),
                std::mem::size_of::<u32>(),
                timeout_ms,
            );
        }
    }

    pub fn futex_wake(addr: &AtomicU32, count: i32) {
        unsafe {
            if count == i32::MAX {
                windows_sys::Win32::System::Threading::WakeByAddressAll(
                    std::ptr::from_ref(addr).cast(),
                );
            } else {
                for _ in 0..count {
                    windows_sys::Win32::System::Threading::WakeByAddressSingle(
                        std::ptr::from_ref(addr).cast(),
                    );
                }
            }
        }
    }
}
