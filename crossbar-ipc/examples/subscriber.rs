//! Cross-process pub/sub latency benchmark — subscriber side.
//! Reads timestamps written by `pubsub_publisher` and computes one-way latency.

use crossbar_ipc::*;

/// Returns monotonic nanoseconds shared across processes.
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

/// Returns monotonic nanoseconds shared across processes.
#[cfg(windows)]
fn mono_nanos() -> u64 {
    extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }
    let mut freq = 0i64;
    let mut count = 0i64;
    unsafe {
        QueryPerformanceFrequency(&mut freq);
        QueryPerformanceCounter(&mut count);
    }
    (count as u64).wrapping_mul(1_000_000_000) / (freq as u64)
}

fn main() {
    let sub = ShmSubscriber::connect("bench-xproc").unwrap();
    let mut stream = sub.subscribe("/tick").unwrap();

    println!("subscriber connected, polling...");

    let mut latencies: Vec<u64> = Vec::with_capacity(100_000);
    let mut last_seq = 0u64;
    let mut idle_since = std::time::Instant::now();

    loop {
        if let Some(guard) = stream.try_recv() {
            let recv_ts = mono_nanos();
            let data: &[u8] = &guard;
            let send_ts = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let seq = u64::from_le_bytes(data[8..16].try_into().unwrap());

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
