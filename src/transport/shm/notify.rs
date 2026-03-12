#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Spin, then yield, then park until `addr` no longer holds `current`.
/// Returns the new value, or Err on timeout.
pub fn wait_until_not(addr: &AtomicU32, current: u32, timeout: Duration) -> Result<u32, ()> {
    const SPIN_ITERS: u32 = 100;
    const YIELD_ITERS: u32 = 10;

    // Phase 1: spin
    for _ in 0..SPIN_ITERS {
        let val = addr.load(Ordering::Acquire);
        if val != current {
            return Ok(val);
        }
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
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let val = addr.load(Ordering::Acquire);
        if val != current {
            return Ok(val);
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        platform::futex_wait(addr, current, Some(Duration::from_millis(10)));
    }
}

/// Wake one thread waiting on `addr`.
pub fn wake_one(addr: &AtomicU32) {
    platform::futex_wake(addr, 1);
}

/// Wake all threads waiting on `addr`.
#[allow(dead_code)]
pub fn wake_all(addr: &AtomicU32) {
    platform::futex_wake(addr, i32::MAX);
}

#[cfg(target_os = "linux")]
mod platform {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    #[allow(unsafe_code)]
    pub fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) {
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs() as _,
            tv_nsec: d.subsec_nanos() as _,
        });
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                addr as *const AtomicU32,
                libc::FUTEX_WAIT,
                expected,
                ts.as_ref()
                    .map_or(std::ptr::null(), |t| t as *const libc::timespec),
                std::ptr::null::<u32>(),
                0u32,
            );
        }
    }

    #[allow(unsafe_code)]
    pub fn futex_wake(addr: &AtomicU32, count: i32) {
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                addr as *const AtomicU32,
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

    // On macOS and other Unix, there's no futex. Use a polling strategy
    // with short sleeps. The spin+yield phases above handle the fast path.
    pub fn futex_wait(_addr: &AtomicU32, _expected: u32, timeout: Option<Duration>) {
        let sleep_time = timeout
            .map(|d| d.min(Duration::from_millis(1)))
            .unwrap_or(Duration::from_millis(1));
        std::thread::sleep(sleep_time);
    }

    pub fn futex_wake(_addr: &AtomicU32, _count: i32) {
        // No-op: the polling loop in wait_until_not will pick up the change
    }
}
