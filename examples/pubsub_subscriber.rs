//! Cross-process pub/sub latency benchmark — subscriber side.
//! Reads timestamps written by `pubsub_publisher` and computes one-way latency.

#[cfg(not(unix))]
fn main() {
    eprintln!("This example requires a Unix target (Linux/macOS).");
}

#[cfg(unix)]
use crossbar::prelude::*;

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
    let sub = ShmSubscriber::connect("bench-xproc").unwrap();
    let mut stream = sub.subscribe("/tick").unwrap();

    println!("subscriber connected, polling...");

    let mut latencies: Vec<u64> = Vec::with_capacity(100_000);
    let mut last_seq = 0u64;
    let mut idle_since = std::time::Instant::now();

    loop {
        if let Some(sample) = stream.try_recv_ref() {
            let recv_ts = mono_nanos();
            let send_ts = u64::from_le_bytes(sample[0..8].try_into().unwrap());
            let seq = u64::from_le_bytes(sample[8..16].try_into().unwrap());

            if seq > last_seq + 1 && last_seq > 0 {
                // Skipped samples (ring overwrite)
            }
            last_seq = seq;

            let latency = recv_ts.saturating_sub(send_ts);
            latencies.push(latency);
            idle_since = std::time::Instant::now();
        } else if idle_since.elapsed().as_secs() > 3 {
            break; // publisher done (or never started)
        } else {
            core::hint::spin_loop();
        }
    }

    if latencies.is_empty() {
        println!("no samples received");
        return;
    }

    latencies.sort();
    let n = latencies.len();
    let min = latencies[0];
    let p50 = latencies[n / 2];
    let p99 = latencies[n * 99 / 100];
    let p999 = latencies[n * 999 / 1000];
    let max = latencies[n - 1];
    let avg: u64 = latencies.iter().sum::<u64>() / n as u64;

    println!("\n=== Cross-process pub/sub latency ({n} samples) ===");
    println!("  min:  {min} ns");
    println!("  avg:  {avg} ns");
    println!("  p50:  {p50} ns");
    println!("  p99:  {p99} ns");
    println!("  p999: {p999} ns");
    println!("  max:  {max} ns");
}
