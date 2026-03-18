// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

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
pub use loan::{PinnedLoan, ShmLoan, TopicHandle, TypedShmLoan};
pub use shm::{ShmPublisher, ShmSubscriber};
pub use subscription::{PinnedGuard, SampleGuard, Subscription, TypedSampleGuard};
