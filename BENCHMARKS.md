# Benchmarks

All numbers on this page come from [Criterion](https://github.com/bheisler/criterion.rs) microbenchmarks
shipped in `benches/transport.rs`. You can reproduce them on your own hardware:

```sh
cargo bench --features shm
```

## Test environment

| | |
|---|---|
| **CPU** | Intel Core i7-10700KF @ 3.80 GHz (8C / 16T) |
| **RAM** | 128 GB DDR4 |
| **OS** | Ubuntu, kernel 6.8.0-101-generic |
| **Rust** | 1.93.1 (stable) |
| **Profile** | `release` (Criterion default) |

> [!IMPORTANT]
> These numbers are from **one machine on one day**. Actual latency depends on your CPU, kernel
> version, system load, and NUMA topology. The relative ordering between transports is what
> matters — not the absolute values. **Run the benchmarks yourself.**

## Methodology

Each benchmark:

1. Creates a shared `Router` with the same five endpoints (health, OHLC JSON, POST order, 64 KB response, 1 MB response).
2. Spins up the relevant transport server (if any).
3. Runs the Criterion harness: automatic warm-up, then 100 statistical samples.

Benchmark functions use `criterion::black_box` to prevent the compiler from eliding work.
All transports use a **single persistent connection** (no connect/disconnect per request).
SHM benchmarks use the default configuration (64 slots, 64 KiB slot capacity).

### What is measured

The timing covers the **full client-side round-trip**: serialize request, transmit, route, execute handler, serialize response, receive. For Memory transport, this is a direct `Arc<Router>` function call. For network transports (UDS, TCP), this includes kernel syscalls.

### What is NOT measured

- Connection setup / teardown (all benchmarks reuse a persistent connection)
- Concurrent client contention (each benchmark uses a single client)
- Handler compute time beyond trivial JSON serialization

## Results

### Latency by transport (single client, sequential requests)

| Benchmark | Memory | Channel | UDS | TCP |
|---|---|---|---|---|
| `/health` (2-byte response) | 151 ns | 6.2 µs | 13.4 µs | 32.3 µs |
| JSON + path params (OHLC) | 1.17 µs | 8.4 µs | 16.2 µs | 34.1 µs |
| POST JSON body (order) | 1.32 µs | 8.3 µs | 17.0 µs | 35.2 µs |
| 64 KB response | 1.22 µs | 8.0 µs | 27.2 µs | 43.9 µs |
| 1 MB response | 18.5 µs | 24.7 µs | 215.8 µs | 229.1 µs |

### Latency by transport — SHM (shared memory)

SHM benchmarks run separately because the spin-poll server has higher CPU overhead
during Criterion's measurement phase. These numbers come from the throughput benchmark group
which uses a dedicated SHM server instance.

| Benchmark | SHM |
|---|---|
| 64 KB response | 6.2 µs |
| 1 MB response | 26.3 µs |

### Throughput (bytes/sec, large payloads)

| Payload | Memory | SHM | UDS | TCP |
|---|---|---|---|---|
| 64 KB | 47.9 GiB/s | 9.9 GiB/s | 2.2 GiB/s | 1.3 GiB/s |
| 1 MB | 51.8 GiB/s | 37.2 GiB/s | 4.5 GiB/s | 4.2 GiB/s |

## Interpretation

**Memory** is a direct function call through `Arc<Router>` — no serialization, no copying,
no kernel involvement. This is the theoretical floor.

**Channel** adds tokio `mpsc` + `oneshot` overhead (~6 µs base). Good for cross-task
communication within one process.

**SHM** uses `mmap` + atomics + futex for cross-process IPC. On large payloads it's
competitive with Channel because both avoid kernel data-path syscalls. On small payloads,
the spin-wait synchronization adds a few microseconds.

**UDS** and **TCP** go through the kernel. UDS avoids the TCP/IP stack but still requires
`write`/`read` syscalls. TCP adds protocol overhead and `TCP_NODELAY` latency.

### Why UDS and TCP converge on large payloads

At 1 MB, UDS (216 µs) and TCP (229 µs) are within 6% of each other. The bottleneck shifts
from protocol overhead to memory copying — both transports copy the full payload through
kernel buffers. SHM avoids this kernel copy, which is why it stays at 26 µs.

### Scaling note

These benchmarks measure **single-client sequential latency**. Under concurrent load:

- Memory and Channel scale linearly with tokio worker threads
- SHM supports up to 64 concurrent requests (one per slot) with no contention
- UDS and TCP are limited by kernel socket buffer sizes and context-switch overhead

## Reproducing

```sh
# All transports (requires Unix + shm feature)
cargo bench --features shm

# Specific transport group
cargo bench -- "memory/"
cargo bench -- "channel/"
cargo bench -- "tcp/"
cargo bench -- "uds/"

# Throughput only
cargo bench --features shm -- "throughput/"
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html`
for interactive graphs (requires a browser).

> [!TIP]
> For the most stable results, close other applications, disable CPU frequency scaling
> (`sudo cpupower frequency-set -g performance`), and run multiple times to check
> for consistency.
