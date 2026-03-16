// Copyright (c) 2026 The Crossbar Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Kairos-style market data pub/sub demo.

use crossbar_inproc::prelude::*;
use std::sync::Arc;
use std::thread;

#[derive(Debug)]
struct MarketQuote {
    symbol: &'static str,
    price: f64,
    volume: u64,
}

fn main() {
    let bus = Bus::<MarketQuote>::new();

    // Pre-resolve topic handles (no hash lookup on publish)
    let aapl_topic = bus.topic("quote:stock:AAPL");
    let all_quotes = bus.topic("all:quotes");

    // Subscriber 1: AAPL-only feed
    let aapl_sub = bus.subscribe("quote:stock:AAPL");

    // Subscriber 2: all-quotes aggregator
    let all_sub = bus.subscribe("all:quotes");

    // Publisher thread
    let handle = thread::spawn(move || {
        for i in 0..5 {
            let quote = Arc::new(MarketQuote {
                symbol: "AAPL",
                price: 150.0 + i as f64,
                volume: 1000 * (i + 1),
            });

            // Fan-out: same Arc to both topics (~3 ns per Arc::clone)
            aapl_topic.publish(Arc::clone(&quote));
            all_quotes.publish(quote);
        }
    });

    handle.join().unwrap();

    // Drain AAPL subscriber
    println!("=== AAPL subscriber ===");
    while let Some(msg) = aapl_sub.try_recv() {
        println!("  {} @ {:.2} vol={}", msg.symbol, msg.price, msg.volume);
    }

    // Drain all-quotes subscriber
    println!("\n=== All quotes subscriber ===");
    while let Some(msg) = all_sub.try_recv() {
        println!("  {} @ {:.2} vol={}", msg.symbol, msg.price, msg.volume);
    }

    println!("\nTopics: {:?}", bus.topics());
    println!("AAPL drops: {}", aapl_sub.drops());
    println!("All-quotes drops: {}", all_sub.drops());
}
