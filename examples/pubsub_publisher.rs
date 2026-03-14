//! Cross-process pub/sub latency benchmark — publisher side.
//! Run alongside `pubsub_subscriber` to measure true cross-process latency.

#[cfg(not(unix))]
fn main() {
    eprintln!("This example requires a Unix target (Linux/macOS).");
}

#[cfg(unix)]
use crossbar::prelude::*;
#[cfg(unix)]
use std::time::Duration;

/// Returns CLOCK_MONOTONIC nanoseconds (shared across processes on Linux).
#[cfg(unix)]
fn mono_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(unix)]
fn main() {
    let mut pub_ = ShmPublisher::create(
        "bench-xproc",
        PubSubConfig {
            sample_capacity: 1_048_576,
            ring_depth: 64, // deep ring so subscriber doesn't miss
            max_topics: 1,  // only one topic → ~64 MiB instead of ~1 GiB
            ..PubSubConfig::default()
        },
    )
    .unwrap();
    let handle = pub_.register("/tick").unwrap();

    println!("publisher ready — start subscriber now, publishing in 2s...");
    std::thread::sleep(Duration::from_secs(2));

    // Paced publishing: one sample every 10µs so subscriber can keep up
    let n = 100_000u64;
    println!("publishing {n} samples at 10µs intervals...");

    for i in 0..n {
        let mut loan = pub_.loan_to(&handle);
        let buf = loan.as_mut_slice();
        // Write mono timestamp + sequence directly into SHM
        let ts = mono_nanos();
        buf[0..8].copy_from_slice(&ts.to_le_bytes());
        buf[8..16].copy_from_slice(&i.to_le_bytes());
        loan.set_len(16);
        loan.publish();

        // Pace at ~10µs per sample to avoid ring overwrite
        std::thread::sleep(Duration::from_micros(10));
    }

    println!("done publishing");
    std::thread::sleep(Duration::from_secs(2));
}
