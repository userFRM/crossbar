// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Platform-specific glue: mmap, futex, file creation, SHM publisher/subscriber.
//!
//! Everything in this module requires `std`.

mod channel;
pub(crate) mod loan;
mod mmap;
pub(crate) mod notify;
mod shm;
pub(crate) mod subscription;

pub use channel::ShmChannel;
pub use loan::{ShmLoan, TopicHandle, TypedShmLoan};
pub use shm::{ShmPublisher, ShmSubscriber};
pub use subscription::{SampleGuard, Subscription, TypedSampleGuard};
