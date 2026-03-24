//! Cross-process latency benchmark — measures TRUE end-to-end IPC latency.
//!
//! Spawns a publisher child process and a subscriber child process,
//! connected via crossbar SHM. Measures one-way latency using shared
//! monotonic timestamps written into the payload.
//!
//! Usage:
//!   cargo build --release --example cross_process_latency
//!   target/release/examples/cross_process_latency
//!
//! Or with taskset to pin to specific cores:
//!   taskset -c 0 target/release/examples/cross_process_latency pub &
//!   taskset -c 2 target/release/examples/cross_process_latency sub

use crossbar::*;
use std::time::{Duration, Instant};

const REGION: &str = "xproc-latency-bench";
const TOPIC: &str = "/bench";
const WARMUP: u64 = 10_000;
const SAMPLES: u64 = 1_000_000;

/// High-resolution monotonic nanoseconds (same clock across processes).
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

fn run_publisher() {
    let mut pub_ = Publisher::create(
        REGION,
        Config {
            ring_depth: 64,
            block_count: 256,
            max_topics: 1,
            ..Config::default()
        },
    )
    .expect("failed to create publisher");
    let topic = pub_.register(TOPIC).unwrap();

    println!("[pub] ready, waiting 2s for subscriber...");
    std::thread::sleep(Duration::from_secs(2));

    // Warmup
    println!("[pub] warmup ({WARMUP} messages)...");
    for _ in 0..WARMUP {
        let mut loan = pub_.loan(&topic).unwrap();
        let ts = mono_nanos();
        loan.as_mut_slice()[0..8].copy_from_slice(&ts.to_le_bytes());
        loan.set_len(8).unwrap();
        loan.publish();
        // Pace to avoid ring overwrite during warmup
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(100));

    // Measured run — publish as fast as possible
    println!("[pub] publishing {SAMPLES} samples...");
    let start = Instant::now();
    for _ in 0..SAMPLES {
        let mut loan = pub_.loan(&topic).unwrap();
        let ts = mono_nanos();
        loan.as_mut_slice()[0..8].copy_from_slice(&ts.to_le_bytes());
        loan.set_len(8).unwrap();
        loan.publish();
    }
    let elapsed = start.elapsed();
    let ns_per = elapsed.as_nanos() as f64 / SAMPLES as f64;
    println!(
        "[pub] done. publish rate: {ns_per:.1} ns/msg ({:.1}M msg/s)",
        SAMPLES as f64 / elapsed.as_secs_f64() / 1e6
    );
    std::thread::sleep(Duration::from_secs(3));
}

fn run_subscriber() {
    println!("[sub] connecting...");
    // Retry until publisher creates the region
    let sub = loop {
        match Subscriber::connect(REGION) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let stream = sub.subscribe(TOPIC).unwrap();
    println!("[sub] connected, polling...");

    let mut latencies: Vec<u64> = Vec::with_capacity(SAMPLES as usize);
    let mut idle_since = Instant::now();
    let mut warmup_remaining = WARMUP;

    loop {
        if let Some(sample) = stream.try_recv() {
            let recv_ts = mono_nanos();
            let send_ts = u64::from_le_bytes(sample[0..8].try_into().unwrap());
            let latency = recv_ts.saturating_sub(send_ts);

            if warmup_remaining > 0 {
                warmup_remaining -= 1;
            } else {
                latencies.push(latency);
            }
            idle_since = Instant::now();
        } else if idle_since.elapsed().as_secs() > 5 {
            break;
        } else {
            // Busy-spin for lowest latency measurement
            core::hint::spin_loop();
        }
    }

    if latencies.is_empty() {
        println!("[sub] no samples received!");
        return;
    }

    latencies.sort();
    let n = latencies.len();
    let min = latencies[0];
    let max = latencies[n - 1];
    let avg = latencies.iter().sum::<u64>() / n as u64;
    let p50 = latencies[n / 2];
    let p99 = latencies[n * 99 / 100];
    let p999 = latencies[n * 999 / 1000];

    println!("\n=== Cross-Process Latency ({n} samples) ===");
    println!("  min:   {:>6} ns", min);
    println!("  avg:   {:>6} ns", avg);
    println!("  p50:   {:>6} ns", p50);
    println!("  p99:   {:>6} ns", p99);
    println!("  p999:  {:>6} ns", p999);
    println!("  max:   {:>6} ns", max);

    // Step-by-step breakdown estimate
    println!("\n=== Estimated Step Breakdown ===");
    println!("  Publisher side:");
    println!("    alloc block (Treiber CAS):    ~15 ns");
    println!("    memcpy 8B into block:          ~2 ns");
    println!("    write ring entry (seqlock):   ~10 ns");
    println!("    total publish:                ~27 ns");
    println!("  Cross-core transfer:");
    println!("    cache line invalidation:    ~30-60 ns");
    println!("  Subscriber side:");
    println!("    detect new seq (spin):         ~0 ns (already spinning)");
    println!("    read ring entry (seqlock):    ~10 ns");
    println!("    CAS refcount:                  ~8 ns");
    println!("    deref data (zero-copy):        ~1 ns");
    println!("    total subscribe:              ~19 ns");
    println!("  ─────────────────────────────────────");
    println!("  Estimated total:              ~76-106 ns");
    println!("  Measured p50:                {:>6} ns", p50);
}

fn run_both() {
    // Spawn publisher as child, run subscriber in this process
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(&exe)
        .arg("pub")
        .spawn()
        .expect("failed to spawn publisher");

    std::thread::sleep(Duration::from_millis(500));
    run_subscriber();
    child.wait().unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("pub") => run_publisher(),
        Some("sub") => run_subscriber(),
        _ => run_both(), // Default: spawn both
    }
}
