// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

/// @file subscriber.cpp
/// @brief Subscribes to price data and prints latency statistics.
///
/// Run alongside `publisher` to demonstrate cross-process pub/sub.
///
/// Usage:
///   ./subscriber
///
/// Connects to the "cpp-demo" region, subscribes to "/tick/AAPL",
/// and collects one-way latency measurements. Prints a percentile
/// histogram when the publisher finishes (detected via idle timeout).

#include <crossbar/crossbar.hpp>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <numeric>
#include <vector>

/// Must match the publisher's layout.
struct PriceTick {
    uint64_t timestamp_ns;
    uint64_t sequence;
    double   price;
};

static uint64_t mono_nanos() {
    auto now = std::chrono::steady_clock::now();
    return static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::nanoseconds>(
            now.time_since_epoch()
        ).count()
    );
}

int main() {
    try {
        crossbar::PoolSubscriber sub("cpp-demo");
        auto stream = sub.subscribe("/tick/AAPL");

        std::printf("subscriber connected, polling...\n");

        std::vector<uint64_t> latencies;
        latencies.reserve(100'000);

        uint64_t last_seq = 0;
        auto idle_since = std::chrono::steady_clock::now();

        for (;;) {
            if (auto sample = stream.try_recv()) {
                uint64_t recv_ts = mono_nanos();

                if (sample->size() < sizeof(PriceTick)) continue;

                PriceTick tick{};
                std::memcpy(&tick, sample->data(), sizeof(tick));

                if (tick.sequence > last_seq + 1 && last_seq > 0) {
                    // Skipped samples (ring overwrite) — acceptable.
                }
                last_seq = tick.sequence;

                uint64_t latency = recv_ts - tick.timestamp_ns;
                latencies.push_back(latency);
                idle_since = std::chrono::steady_clock::now();

            } else {
                auto elapsed = std::chrono::steady_clock::now() - idle_since;
                if (elapsed > std::chrono::seconds(3)) {
                    break;  // publisher done (or never started)
                }
                // Spin — matches the Rust subscriber's polling strategy.
#if defined(__x86_64__) || defined(_M_X64)
                __builtin_ia32_pause();
#elif defined(__aarch64__)
                asm volatile("yield");
#endif
            }
        }

        if (latencies.empty()) {
            std::printf("no samples received\n");
            return 0;
        }

        std::sort(latencies.begin(), latencies.end());

        size_t n = latencies.size();
        uint64_t min  = latencies.front();
        uint64_t max  = latencies.back();
        uint64_t p50  = latencies[n / 2];
        uint64_t p99  = latencies[n * 99 / 100];
        uint64_t p999 = latencies[n * 999 / 1000];
        uint64_t avg  = std::accumulate(latencies.begin(), latencies.end(),
                                         uint64_t{0}) / n;

        std::printf("\n=== Cross-process pub/sub latency (%zu samples) ===\n", n);
        std::printf("  min:  %llu ns\n", static_cast<unsigned long long>(min));
        std::printf("  avg:  %llu ns\n", static_cast<unsigned long long>(avg));
        std::printf("  p50:  %llu ns\n", static_cast<unsigned long long>(p50));
        std::printf("  p99:  %llu ns\n", static_cast<unsigned long long>(p99));
        std::printf("  p999: %llu ns\n", static_cast<unsigned long long>(p999));
        std::printf("  max:  %llu ns\n", static_cast<unsigned long long>(max));

    } catch (const crossbar::Error& e) {
        std::fprintf(stderr, "crossbar error: %s\n", e.what());
        return 1;
    }

    return 0;
}
