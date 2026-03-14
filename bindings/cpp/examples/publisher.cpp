// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

/// @file publisher.cpp
/// @brief Publishes simulated price data over crossbar zero-copy IPC.
///
/// Run alongside `subscriber` to demonstrate cross-process pub/sub.
///
/// Usage:
///   ./publisher
///
/// The publisher creates a shared memory region named "cpp-demo" and
/// publishes 100,000 price ticks at ~10 us intervals on "/tick/AAPL".

#include <crossbar/crossbar.hpp>

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <thread>

/// Packed price tick — 24 bytes, written directly into shared memory.
struct PriceTick {
    uint64_t timestamp_ns;  // monotonic clock (for latency measurement)
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
        // Configure: single topic, deep ring so the subscriber can keep up.
        crossbar::Config cfg;
        cfg.max_topics   = 1;
        cfg.block_count  = 256;
        cfg.block_size   = 65536;
        cfg.ring_depth   = 64;

        crossbar::PoolPublisher pub_("cpp-demo", cfg);
        auto topic = pub_.register_topic("/tick/AAPL");

        std::printf("publisher ready — start subscriber now, publishing in 2s...\n");
        std::this_thread::sleep_for(std::chrono::seconds(2));

        constexpr uint64_t N = 100'000;
        std::printf("publishing %llu samples at ~10us intervals...\n",
                     static_cast<unsigned long long>(N));

        for (uint64_t i = 0; i < N; ++i) {
            auto loan = pub_.loan(topic);

            PriceTick tick{};
            tick.timestamp_ns = mono_nanos();
            tick.sequence     = i;
            tick.price        = 182.35 + static_cast<double>(i) * 0.01;

            loan.write(&tick, sizeof(tick));
            loan.publish();

            // Pace at ~10 us to avoid ring overwrite.
            std::this_thread::sleep_for(std::chrono::microseconds(10));
        }

        std::printf("done publishing %llu samples\n",
                     static_cast<unsigned long long>(N));

        // Keep region alive briefly so the subscriber can drain.
        std::this_thread::sleep_for(std::chrono::seconds(2));

    } catch (const crossbar::Error& e) {
        std::fprintf(stderr, "crossbar error: %s\n", e.what());
        return 1;
    }

    return 0;
}
