//! Cross-process latency benchmark — crossbar vs iceoryx2, rigorous.
//!
//! Methodology:
//! - 8B payload (u64 timestamp via CLOCK_MONOTONIC)
//! - Publisher paced at 1 µs/msg (avoids ring saturation)
//! - Subscriber busy-spins (lowest latency path)
//! - 50K warmup + 500K measured samples PER RUN
//! - 5 independent runs, results aggregated with mean ± stddev
//! - Core-pinned via sched_setaffinity (pub=core 0, sub=core 2)
//! - CPU governor check (warns if not 'performance')
//!
//! Usage:
//!   cargo build --release --example cross_process_iceoryx2
//!   sudo cpupower frequency-set -g performance  # optional but recommended
//!   target/release/examples/cross_process_iceoryx2 run

use std::time::Duration;

const WARMUP: u64 = 50_000;
const SAMPLES: u64 = 500_000;
const RUNS: usize = 5;
const PACE_NS: u64 = 1_000; // 1 µs between publishes
const PUB_CORE: usize = 0;
const SUB_CORE: usize = 2;

// ── Platform helpers ──────────────────────────────────────────

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

fn spin_wait_ns(ns: u64) {
    let start = mono_nanos();
    while mono_nanos() - start < ns {
        core::hint::spin_loop();
    }
}

/// Pin current thread to a specific CPU core.
#[cfg(target_os = "linux")]
fn pin_to_core(core: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_core(_core: usize) {
    // No-op on non-Linux (macOS doesn't support sched_setaffinity)
}

/// Check CPU frequency governor.
#[cfg(target_os = "linux")]
fn check_governor() {
    if let Ok(gov) =
        std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    {
        let gov = gov.trim();
        if gov != "performance" {
            eprintln!("WARNING: CPU governor is '{}', not 'performance'.", gov);
            eprintln!("         Results may have higher variance.");
            eprintln!("         Fix: sudo cpupower frequency-set -g performance");
        } else {
            println!("[info] CPU governor: performance ✓");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn check_governor() {}

// ── Stats ─────────────────────────────────────────────────────

#[derive(Clone)]
struct RunStats {
    min: u64,
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
    avg: u64,
    samples: usize,
}

fn compute_stats(latencies: &mut [u64]) -> RunStats {
    latencies.sort();
    let n = latencies.len();
    RunStats {
        min: latencies[0],
        p50: latencies[n / 2],
        p99: latencies[n * 99 / 100],
        p999: latencies[n * 999 / 1000],
        max: latencies[n - 1],
        avg: latencies.iter().sum::<u64>() / n as u64,
        samples: n,
    }
}

fn print_aggregated(name: &str, runs: &[RunStats]) {
    let n = runs.len() as f64;

    let mean =
        |f: fn(&RunStats) -> u64| -> f64 { runs.iter().map(|r| f(r) as f64).sum::<f64>() / n };
    let stddev = |f: fn(&RunStats) -> u64| -> f64 {
        let m = mean(f);
        (runs
            .iter()
            .map(|r| {
                let d = f(r) as f64 - m;
                d * d
            })
            .sum::<f64>()
            / n)
            .sqrt()
    };

    let min_mean = mean(|r| r.min);
    let min_std = stddev(|r| r.min);
    let p50_mean = mean(|r| r.p50);
    let p50_std = stddev(|r| r.p50);
    let p99_mean = mean(|r| r.p99);
    let p99_std = stddev(|r| r.p99);
    let p999_mean = mean(|r| r.p999);
    let p999_std = stddev(|r| r.p999);
    let max_mean = mean(|r| r.max);
    let max_std = stddev(|r| r.max);
    let avg_mean = mean(|r| r.avg);
    let avg_std = stddev(|r| r.avg);
    let total_samples: usize = runs.iter().map(|r| r.samples).sum();

    println!("\n╔═══════════════════════════════════════════════════════╗");
    println!(
        "║  {:<53} ║",
        format!("{name} — {RUNS} runs × {SAMPLES} samples")
    );
    println!("╠═══════════════════════════════════════════════════════╣");
    println!(
        "║  {:>8}  {:>10}  {:>10}                       ║",
        "metric", "mean (ns)", "± stddev"
    );
    println!(
        "║  {:>8}  {:>10.0}  {:>10.1}                       ║",
        "min", min_mean, min_std
    );
    println!(
        "║  {:>8}  {:>10.0}  {:>10.1}                       ║",
        "avg", avg_mean, avg_std
    );
    println!(
        "║  {:>8}  {:>10.0}  {:>10.1}                       ║",
        "p50", p50_mean, p50_std
    );
    println!(
        "║  {:>8}  {:>10.0}  {:>10.1}                       ║",
        "p99", p99_mean, p99_std
    );
    println!(
        "║  {:>8}  {:>10.0}  {:>10.1}                       ║",
        "p999", p999_mean, p999_std
    );
    println!(
        "║  {:>8}  {:>10.0}  {:>10.1}                       ║",
        "max", max_mean, max_std
    );
    println!(
        "║  total samples: {:>10}                            ║",
        total_samples
    );
    println!("╚═══════════════════════════════════════════════════════╝");
}

// ── crossbar ──────────────────────────────────────────────────

mod crossbar_bench {
    use super::*;
    use crossbar::*;

    const REGION: &str = "xproc-rigorous-crossbar";
    const TOPIC: &str = "/bench";

    pub fn publisher(run_id: usize) {
        pin_to_core(PUB_CORE);
        let region = format!("{REGION}-{run_id}");
        let mut pub_ = Publisher::create(
            &region,
            Config {
                ring_depth: 256,
                block_count: 512,
                max_topics: 1,
                ..Config::default()
            },
        )
        .unwrap();
        let topic = pub_.register(TOPIC).unwrap();

        // Wait for subscriber
        std::thread::sleep(Duration::from_secs(2));

        // Warmup
        for _ in 0..WARMUP {
            let mut loan = pub_.loan(&topic).unwrap();
            loan.as_mut_slice()[0..8].copy_from_slice(&mono_nanos().to_le_bytes());
            loan.set_len(8).unwrap();
            loan.publish();
            spin_wait_ns(PACE_NS);
        }
        // Brief pause between warmup and measured
        std::thread::sleep(Duration::from_millis(50));

        // Measured
        for _ in 0..SAMPLES {
            let mut loan = pub_.loan(&topic).unwrap();
            loan.as_mut_slice()[0..8].copy_from_slice(&mono_nanos().to_le_bytes());
            loan.set_len(8).unwrap();
            loan.publish();
            spin_wait_ns(PACE_NS);
        }
        std::thread::sleep(Duration::from_secs(3));
    }

    pub fn subscriber(run_id: usize) -> RunStats {
        pin_to_core(SUB_CORE);
        let region = format!("{REGION}-{run_id}");
        let sub = loop {
            match Subscriber::connect(&region) {
                Ok(s) => break s,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        let stream = sub.subscribe(TOPIC).unwrap();

        let mut latencies: Vec<u64> = Vec::with_capacity(SAMPLES as usize);
        let mut warmup = WARMUP;
        let mut idle_since = std::time::Instant::now();

        loop {
            if let Some(sample) = stream.try_recv() {
                let recv_ts = mono_nanos();
                let send_ts = u64::from_le_bytes(sample[0..8].try_into().unwrap());
                let latency = recv_ts.saturating_sub(send_ts);
                if warmup > 0 {
                    warmup -= 1;
                } else {
                    latencies.push(latency);
                }
                idle_since = std::time::Instant::now();
            } else if idle_since.elapsed().as_secs() > 5 {
                break;
            } else {
                core::hint::spin_loop();
            }
        }
        compute_stats(&mut latencies)
    }
}

// ── iceoryx2 ──────────────────────────────────────────────────

#[cfg(unix)]
mod iceoryx2_bench {
    use super::*;
    use iceoryx2::prelude::*;

    pub fn publisher(run_id: usize) {
        pin_to_core(PUB_CORE);
        let svc_name: ServiceName = format!("xproc-rigorous-ix2-{run_id}")
            .as_str()
            .try_into()
            .unwrap();
        let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
        let service = node
            .service_builder(&svc_name)
            .publish_subscribe::<[u8]>()
            .enable_safe_overflow(true)
            .open_or_create()
            .unwrap();
        let publisher = service
            .publisher_builder()
            .initial_max_slice_len(8)
            .create()
            .unwrap();

        std::thread::sleep(Duration::from_secs(2));

        // Warmup
        for _ in 0..WARMUP {
            let sample = publisher.loan_slice_uninit(8).unwrap();
            let sample = sample.write_from_slice(&mono_nanos().to_le_bytes());
            sample.send().unwrap();
            spin_wait_ns(PACE_NS);
        }
        std::thread::sleep(Duration::from_millis(50));

        // Measured
        for _ in 0..SAMPLES {
            let sample = publisher.loan_slice_uninit(8).unwrap();
            let sample = sample.write_from_slice(&mono_nanos().to_le_bytes());
            sample.send().unwrap();
            spin_wait_ns(PACE_NS);
        }
        std::thread::sleep(Duration::from_secs(3));
    }

    pub fn subscriber(run_id: usize) -> RunStats {
        pin_to_core(SUB_CORE);
        let svc_name: ServiceName = format!("xproc-rigorous-ix2-{run_id}")
            .as_str()
            .try_into()
            .unwrap();
        let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
        let service = node
            .service_builder(&svc_name)
            .publish_subscribe::<[u8]>()
            .enable_safe_overflow(true)
            .open_or_create()
            .unwrap();
        let subscriber = service.subscriber_builder().create().unwrap();

        let mut latencies: Vec<u64> = Vec::with_capacity(SAMPLES as usize);
        let mut warmup = WARMUP;
        let mut idle_since = std::time::Instant::now();

        loop {
            match subscriber.receive().unwrap() {
                Some(sample) => {
                    let recv_ts = mono_nanos();
                    let send_ts = u64::from_le_bytes(sample[0..8].try_into().unwrap());
                    let latency = recv_ts.saturating_sub(send_ts);
                    if warmup > 0 {
                        warmup -= 1;
                    } else {
                        latencies.push(latency);
                    }
                    idle_since = std::time::Instant::now();
                }
                None => {
                    if idle_since.elapsed().as_secs() > 5 {
                        break;
                    }
                    core::hint::spin_loop();
                }
            }
        }
        compute_stats(&mut latencies)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("run");

    // Internal dispatch for child processes
    match mode {
        "pub-crossbar" => {
            let run_id: usize = args[2].parse().unwrap();
            crossbar_bench::publisher(run_id);
            return;
        }
        "sub-crossbar" => {
            let run_id: usize = args[2].parse().unwrap();
            let stats = crossbar_bench::subscriber(run_id);
            // Serialize stats to stdout for parent to collect
            println!(
                "STATS:{},{},{},{},{},{},{}",
                stats.min, stats.avg, stats.p50, stats.p99, stats.p999, stats.max, stats.samples
            );
            return;
        }
        #[cfg(unix)]
        "pub-iceoryx2" => {
            let run_id: usize = args[2].parse().unwrap();
            iceoryx2_bench::publisher(run_id);
            return;
        }
        #[cfg(unix)]
        "sub-iceoryx2" => {
            let run_id: usize = args[2].parse().unwrap();
            let stats = iceoryx2_bench::subscriber(run_id);
            println!(
                "STATS:{},{},{},{},{},{},{}",
                stats.min, stats.avg, stats.p50, stats.p99, stats.p999, stats.max, stats.samples
            );
            return;
        }
        _ => {}
    }

    // ── Orchestrator ──────────────────────────────────────────

    println!("╔══════════════════════════════════════════════╗");
    println!("║  Cross-Process Latency: Rigorous Benchmark   ║");
    println!("║  8B payload, paced 1µs/msg, busy-spin sub   ║");
    println!("║  {RUNS} runs × {SAMPLES} samples each              ║");
    println!("║  Core pinned: pub=core {PUB_CORE}, sub=core {SUB_CORE}        ║");
    println!("╚══════════════════════════════════════════════╝\n");

    check_governor();

    let exe = std::env::current_exe().unwrap();

    let run_system = |system: &str| -> Vec<RunStats> {
        let mut all_stats = Vec::new();
        for run in 0..RUNS {
            print!("[{system}] run {}/{}...", run + 1, RUNS);

            // Spawn subscriber first, then publisher
            let mut sub = std::process::Command::new(&exe)
                .args([&format!("sub-{system}"), &run.to_string()])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();

            std::thread::sleep(Duration::from_millis(500));

            let mut pub_ = std::process::Command::new(&exe)
                .args([&format!("pub-{system}"), &run.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();

            pub_.wait().unwrap();
            let output = sub.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);

            for line in stdout.lines() {
                if let Some(data) = line.strip_prefix("STATS:") {
                    let parts: Vec<u64> = data.split(',').filter_map(|s| s.parse().ok()).collect();
                    if parts.len() == 7 {
                        all_stats.push(RunStats {
                            min: parts[0],
                            avg: parts[1],
                            p50: parts[2],
                            p99: parts[3],
                            p999: parts[4],
                            max: parts[5],
                            samples: parts[6] as usize,
                        });
                        println!(" p50={} ns, min={} ns", parts[2], parts[0]);
                    }
                }
            }

            // Brief pause between runs
            std::thread::sleep(Duration::from_secs(1));
        }
        all_stats
    };

    let crossbar_stats = run_system("crossbar");
    println!();
    #[cfg(unix)]
    let iceoryx2_stats = run_system("iceoryx2");

    print_aggregated("crossbar", &crossbar_stats);
    #[cfg(unix)]
    print_aggregated("iceoryx2", &iceoryx2_stats);

    // Comparison
    #[cfg(unix)]
    if !crossbar_stats.is_empty() && !iceoryx2_stats.is_empty() {
        let cb_p50: f64 = crossbar_stats.iter().map(|r| r.p50 as f64).sum::<f64>() / RUNS as f64;
        let ix_p50: f64 = iceoryx2_stats.iter().map(|r| r.p50 as f64).sum::<f64>() / RUNS as f64;
        let cb_min: f64 = crossbar_stats.iter().map(|r| r.min as f64).sum::<f64>() / RUNS as f64;
        let ix_min: f64 = iceoryx2_stats.iter().map(|r| r.min as f64).sum::<f64>() / RUNS as f64;
        println!("\n── Summary ─────────────────────────────────");
        println!(
            "  p50 speedup: {:.2}× (crossbar {:.0} ns vs iceoryx2 {:.0} ns)",
            ix_p50 / cb_p50,
            cb_p50,
            ix_p50
        );
        println!(
            "  min speedup: {:.2}× (crossbar {:.0} ns vs iceoryx2 {:.0} ns)",
            ix_min / cb_min,
            cb_min,
            ix_min
        );
    }
}
