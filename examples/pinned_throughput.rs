use crossbar::*;
use std::time::Instant;

fn main() {
    let mut pub_ = ShmPublisher::create("tp-bench", PubSubConfig::default()).unwrap();
    let handle = pub_.register("/throughput").unwrap();
    let sub = ShmSubscriber::connect("tp-bench").unwrap();
    let stream = sub.subscribe("/throughput").unwrap();

    // Warmup
    for _ in 0..1000 {
        let mut loan = pub_.loan_pinned(&handle);
        loan.as_mut_slice()[..8].copy_from_slice(&42u64.to_le_bytes());
        loan.set_len(8);
        loan.publish();
        let _ = stream.try_recv_pinned();
    }

    // Measure: publish-only throughput
    let n = 10_000_000u64;
    let start = Instant::now();
    for i in 0..n {
        let mut loan = pub_.loan_pinned(&handle);
        loan.as_mut_slice()[..8].copy_from_slice(&i.to_le_bytes());
        loan.set_len(8);
        loan.publish();
    }
    let elapsed = start.elapsed();
    let ns_per = elapsed.as_nanos() as f64 / n as f64;
    let msgs_per_sec = n as f64 / elapsed.as_secs_f64();
    println!(
        "Publish-only (8B):  {ns_per:.1} ns/msg  {:.1}M msg/s",
        msgs_per_sec / 1e6
    );

    // Measure: full roundtrip
    let start = Instant::now();
    for i in 0..n {
        let mut loan = pub_.loan_pinned(&handle);
        loan.as_mut_slice()[..8].copy_from_slice(&i.to_le_bytes());
        loan.set_len(8);
        loan.publish();
        let g = stream.try_recv_pinned().unwrap();
        std::hint::black_box(&*g);
    }
    let elapsed = start.elapsed();
    let ns_per = elapsed.as_nanos() as f64 / n as f64;
    let msgs_per_sec = n as f64 / elapsed.as_secs_f64();
    println!(
        "Full roundtrip (8B): {ns_per:.1} ns/msg  {:.1}M msg/s",
        msgs_per_sec / 1e6
    );

    // Data throughput at 64KB
    let n_64k = 500_000u64;
    let start = Instant::now();
    for _ in 0..n_64k {
        let mut loan = pub_.loan_pinned(&handle);
        let cap = loan.capacity();
        loan.as_mut_slice()[..cap].fill(42u8);
        loan.set_len(cap);
        loan.publish();
        let g = stream.try_recv_pinned().unwrap();
        std::hint::black_box(&*g);
    }
    let elapsed = start.elapsed();
    let gb_per_sec = (n_64k as f64 * 65528.0) / elapsed.as_secs_f64() / 1e9;
    let msgs_per_sec = n_64k as f64 / elapsed.as_secs_f64();
    println!(
        "64KB roundtrip:      {:.1} GB/s  {:.0}K msg/s",
        gb_per_sec,
        msgs_per_sec / 1e3
    );
}
