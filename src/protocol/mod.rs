// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core protocol types and layout constants for crossbar pub/sub.
//!
//! Everything in this module is `no_std`-compatible: pure atomics, raw pointer
//! math, and lock-free data structures (Treiber stack, seqlock, ring).

mod config;
pub(crate) mod layout;
mod region;

pub use config::PubSubConfig;
pub use region::Region;
