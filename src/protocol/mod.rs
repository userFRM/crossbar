// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Core protocol types and layout constants for crossbar pub/sub.
//!
//! Everything in this module is `no_std`-compatible: pure atomics, raw pointer
//! math, and lock-free data structures (Treiber stack, seqlock, ring).

mod config;
pub(crate) mod layout;
mod region;

pub use config::PubSubConfig;
#[cfg(feature = "std")]
pub(crate) use region::release_block;
pub use region::Region;
