# macOS Benchmark Results

## Machine Info

| Property | Value |
|----------|-------|
| CPU | Apple M1 Pro |
| Memory | 32 GB |
| OS | macOS 26.3.1 (Build 25D2128) |
| Rust | rustc 1.92.0 (ded5c06cf 2025-12-08) |
| Date | 2026-03-15 |

## crossbar-ipc

### Pub/Sub Transport Only

| Benchmark | Time (median) |
|-----------|--------------|
| pubsub_transport_only/smart_wake | 54.753 ns |
| pubsub_transport_only/silent_no_wake | 54.488 ns |

### Pub/Sub O(1)

| Benchmark | Time (median) |
|-----------|--------------|
| pubsub_o1/8B | 54.718 ns |
| pubsub_o1/64KB | 1.2487 µs |
| pubsub_o1/1MB | 22.967 µs |

### Throughput Pub/Sub

| Benchmark | Time (median) | Throughput |
|-----------|--------------|------------|
| throughput_pubsub/64kb | 1.2515 µs | 48.768 GiB/s |
| throughput_pubsub_1mb/1mb | 22.566 µs | 43.275 GiB/s |

### Head-to-Head O(1) (crossbar vs iceoryx2)

| Benchmark | iceoryx2 | crossbar | Speedup |
|-----------|----------|----------|---------|
| 8B on 64B buf | 188.54 ns | 51.772 ns | 3.64x |
| 8B on 4KB buf | 189.07 ns | 51.784 ns | 3.65x |
| 8B on 64KB buf | 189.26 ns | 51.766 ns | 3.66x |
| 8B on 256KB buf | 188.44 ns | 52.273 ns | 3.61x |
| 8B on 1MB buf | 188.41 ns | 54.766 ns | 3.44x |

### Head-to-Head E2E (crossbar vs iceoryx2)

| Benchmark | iceoryx2 | crossbar | Speedup |
|-----------|----------|----------|---------|
| 8B | 188.66 ns | 54.685 ns | 3.45x |
| 1KB | 209.82 ns | 77.482 ns | 2.71x |
| 64KB | 1.3510 µs | 1.2663 µs | 1.07x |
| 256KB | 5.1956 µs | 5.0968 µs | 1.02x |
| 1MB | 23.463 µs | 23.873 µs | 0.98x |

## crossbar-inproc

### Dispatch

| Benchmark | Time (median) |
|-----------|--------------|
| dispatch/health | 147.76 ns |
| dispatch/ohlc_with_params | 1.0804 µs |
| dispatch/404 | 270.50 ns |

### In-Process Router

| Benchmark | Time (median) |
|-----------|--------------|
| inproc/health | 148.43 ns |
| inproc/ohlc | 1.0891 µs |
| inproc/post_json | 1.3365 µs |
| inproc/large_64kb | 2.2211 µs |
| inproc/large_1mb | 19.012 µs |

---

<details>
<summary>Raw output: crossbar-ipc</summary>

```
pubsub_transport_only/smart_wake
                        time:   [54.686 ns 54.753 ns 54.820 ns]

pubsub_transport_only/silent_no_wake
                        time:   [54.426 ns 54.488 ns 54.554 ns]

pubsub_o1/8B            time:   [54.656 ns 54.718 ns 54.781 ns]

pubsub_o1/64KB          time:   [1.2447 µs 1.2487 µs 1.2540 µs]

pubsub_o1/1MB           time:   [22.687 µs 22.967 µs 23.242 µs]

throughput_pubsub/64kb  time:   [1.2500 µs 1.2515 µs 1.2532 µs]
                        thrpt:  [48.704 GiB/s 48.768 GiB/s 48.829 GiB/s]

throughput_pubsub_1mb/1mb
                        time:   [22.398 µs 22.566 µs 22.728 µs]
                        thrpt:  [42.967 GiB/s 43.275 GiB/s 43.601 GiB/s]

head_to_head_o1/iceoryx2/8B_on_64B_buf   time:   [188.32 ns 188.54 ns 188.79 ns]
head_to_head_o1/iceoryx2/8B_on_4KB_buf   time:   [188.70 ns 189.07 ns 189.63 ns]
head_to_head_o1/iceoryx2/8B_on_64KB_buf  time:   [188.89 ns 189.26 ns 189.71 ns]
head_to_head_o1/iceoryx2/8B_on_256KB_buf time:   [188.32 ns 188.44 ns 188.57 ns]
head_to_head_o1/iceoryx2/8B_on_1MB_buf   time:   [188.24 ns 188.41 ns 188.58 ns]

head_to_head_o1/crossbar/8B_on_64B_buf   time:   [51.731 ns 51.772 ns 51.812 ns]
head_to_head_o1/crossbar/8B_on_4KB_buf   time:   [51.744 ns 51.784 ns 51.823 ns]
head_to_head_o1/crossbar/8B_on_64KB_buf  time:   [51.724 ns 51.766 ns 51.810 ns]
head_to_head_o1/crossbar/8B_on_256KB_buf time:   [52.208 ns 52.273 ns 52.338 ns]
head_to_head_o1/crossbar/8B_on_1MB_buf   time:   [54.725 ns 54.766 ns 54.806 ns]

head_to_head_e2e/iceoryx2/8B   time:   [188.51 ns 188.66 ns 188.81 ns]
head_to_head_e2e/iceoryx2/1KB  time:   [209.66 ns 209.82 ns 209.99 ns]
head_to_head_e2e/iceoryx2/64KB time:   [1.3473 µs 1.3510 µs 1.3559 µs]
head_to_head_e2e/iceoryx2/256KB time:  [5.1889 µs 5.1956 µs 5.2033 µs]
head_to_head_e2e/iceoryx2/1MB  time:   [23.296 µs 23.463 µs 23.638 µs]

head_to_head_e2e/crossbar/8B   time:   [54.642 ns 54.685 ns 54.728 ns]
head_to_head_e2e/crossbar/1KB  time:   [77.420 ns 77.482 ns 77.546 ns]
head_to_head_e2e/crossbar/64KB time:   [1.2582 µs 1.2663 µs 1.2762 µs]
head_to_head_e2e/crossbar/256KB time:  [5.0890 µs 5.0968 µs 5.1058 µs]
head_to_head_e2e/crossbar/1MB  time:   [23.500 µs 23.873 µs 24.282 µs]
```

</details>

<details>
<summary>Raw output: crossbar-inproc</summary>

```
dispatch/health         time:   [146.61 ns 147.76 ns 149.16 ns]

dispatch/ohlc_with_params
                        time:   [1.0740 µs 1.0804 µs 1.0896 µs]

dispatch/404            time:   [266.46 ns 270.50 ns 275.34 ns]

inproc/health           time:   [146.82 ns 148.43 ns 150.55 ns]

inproc/ohlc             time:   [1.0776 µs 1.0891 µs 1.1045 µs]

inproc/post_json        time:   [1.3234 µs 1.3365 µs 1.3511 µs]

inproc/large_64kb       time:   [2.2161 µs 2.2211 µs 2.2264 µs]

inproc/large_1mb        time:   [18.950 µs 19.012 µs 19.074 µs]
```

</details>
