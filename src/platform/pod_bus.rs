// Copyright (c) 2026 The Crossbar Contributors
// This source code is licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for details.

// SPDX-License-Identifier: Apache-2.0

//! Single-producer, multi-consumer (SPMC) broadcast ring for [`Pod`] types.
//!
//! [`PodBus`] publishes values into a fixed-size ring buffer. Any number of
//! [`PodSubscriber`]s can independently read from the ring without blocking
//! the publisher or each other. Publish is O(1) regardless of subscriber count.
//!
//! The ring uses a seqlock-style stamp protocol:
//! - `0` — slot never written
//! - odd  (`seq * 2 + 1`) — write in progress
//! - even (`(seq + 1) * 2`) — committed and readable

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::Pod;

/// A slot in the broadcast ring.
#[repr(C)]
pub struct Slot<T: Pod> {
    stamp: AtomicU64,
    data: UnsafeCell<T>,
}

/// SPMC broadcast ring for [`Pod`] types.
///
/// The publisher writes into a power-of-two ring. Each publish is O(1)
/// regardless of how many subscribers exist. Subscribers that fall behind
/// will skip to the newest available data (lossy).
pub struct PodBus<T: Pod> {
    ring: Box<[Slot<T>]>,
    mask: u64,
    write_seq: AtomicU64,
}

// Safety: T: Pod implies T: Send + Copy, and all shared state uses atomics.
unsafe impl<T: Pod> Send for PodBus<T> {}
unsafe impl<T: Pod> Sync for PodBus<T> {}

/// A cursor into a [`PodBus`] ring.
///
/// Each subscriber tracks its own read position independently.
pub struct PodSubscriber<T: Pod> {
    cursor: u64,
    _marker: core::marker::PhantomData<T>,
}

impl<T: Pod> PodBus<T> {
    /// Create a new broadcast ring with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `ring_size` is zero or not a power of two.
    pub fn new(ring_size: usize) -> Self {
        assert!(
            ring_size > 0 && ring_size.is_power_of_two(),
            "ring_size must be a power of two"
        );

        let mut slots = alloc::vec::Vec::with_capacity(ring_size);
        for _ in 0..ring_size {
            slots.push(Slot {
                stamp: AtomicU64::new(0),
                data: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            });
        }

        Self {
            ring: slots.into_boxed_slice(),
            mask: (ring_size as u64) - 1,
            write_seq: AtomicU64::new(0),
        }
    }

    /// Publish a value into the ring. O(1) regardless of subscriber count.
    pub fn publish(&self, value: T) {
        let seq = self.write_seq.load(Ordering::Relaxed);
        let idx = (seq & self.mask) as usize;
        let slot = &self.ring[idx];

        // Mark slot as being written (odd stamp).
        slot.stamp.store(seq * 2 + 1, Ordering::Release);

        // Write data.
        // Safety: we are the sole writer and the odd stamp signals readers to retry.
        unsafe {
            core::ptr::write_volatile(slot.data.get(), value);
        }

        // Mark slot as committed (even stamp >= 2).
        slot.stamp.store((seq + 1) * 2, Ordering::Release);

        // Advance sequence.
        self.write_seq.store(seq + 1, Ordering::Release);
    }

    /// Create a new subscriber starting from the current write position.
    pub fn subscriber(&self) -> PodSubscriber<T> {
        PodSubscriber {
            cursor: self.write_seq.load(Ordering::Acquire),
            _marker: core::marker::PhantomData,
        }
    }

    /// Return the total number of values published so far.
    pub fn published_count(&self) -> u64 {
        self.write_seq.load(Ordering::Acquire)
    }

    /// Return the ring capacity.
    pub fn ring_size(&self) -> usize {
        self.ring.len()
    }
}

impl<T: Pod> PodSubscriber<T> {
    /// Try to receive the next value from the ring.
    ///
    /// Returns `None` if no new data is available. If the subscriber has
    /// fallen behind (the slot it wants has been overwritten), it advances
    /// its cursor to the oldest still-available data.
    pub fn try_recv(&mut self, bus: &PodBus<T>) -> Option<T> {
        let head = bus.write_seq.load(Ordering::Acquire);

        if self.cursor >= head {
            return None; // caught up
        }

        // If subscriber is more than ring_size behind, skip ahead.
        let ring_size = bus.ring.len() as u64;
        if head - self.cursor > ring_size {
            self.cursor = head - ring_size;
        }

        let idx = (self.cursor & bus.mask) as usize;
        let slot = &bus.ring[idx];

        // Seqlock read: snapshot stamp, read data, verify stamp unchanged.
        let stamp_before = slot.stamp.load(Ordering::Acquire);

        // Odd stamp means write in progress — skip this attempt.
        if stamp_before & 1 != 0 {
            return None;
        }

        // Stamp 0 means never written.
        if stamp_before == 0 {
            return None;
        }

        // Read the data.
        // Safety: T: Pod (Copy), volatile read through UnsafeCell.
        let value = unsafe { core::ptr::read_volatile(slot.data.get()) };

        let stamp_after = slot.stamp.load(Ordering::Acquire);

        if stamp_after != stamp_before {
            // Stamp changed during read — data was torn, retry next call.
            return None;
        }

        // Verify this stamp corresponds to the sequence we expected.
        // Expected committed stamp for `self.cursor` is `(self.cursor + 1) * 2`.
        let expected_stamp = (self.cursor + 1) * 2;
        if stamp_before != expected_stamp {
            // Slot has been overwritten with a newer sequence — advance cursor.
            // Derive the sequence from the stamp: stamp = (seq + 1) * 2 => seq = stamp / 2 - 1
            let written_seq = stamp_before / 2 - 1;
            self.cursor = written_seq + 1;
            return None;
        }

        self.cursor += 1;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_publish_subscribe() {
        let bus = PodBus::<u64>::new(4);
        let mut sub = bus.subscriber();

        assert!(sub.try_recv(&bus).is_none());

        bus.publish(42u64);
        assert_eq!(sub.try_recv(&bus), Some(42));
        assert!(sub.try_recv(&bus).is_none());
    }

    #[test]
    fn multiple_subscribers() {
        let bus = PodBus::<u32>::new(8);
        let mut s1 = bus.subscriber();
        let mut s2 = bus.subscriber();
        let mut s3 = bus.subscriber();

        bus.publish(10);
        bus.publish(20);

        assert_eq!(s1.try_recv(&bus), Some(10));
        assert_eq!(s1.try_recv(&bus), Some(20));

        assert_eq!(s2.try_recv(&bus), Some(10));
        assert_eq!(s2.try_recv(&bus), Some(20));

        assert_eq!(s3.try_recv(&bus), Some(10));
        assert_eq!(s3.try_recv(&bus), Some(20));
    }

    #[test]
    fn ring_overwrite() {
        let bus = PodBus::<u64>::new(4);
        let mut sub = bus.subscriber();

        // Publish more than ring_size to force overwrite.
        for i in 0..10u64 {
            bus.publish(i);
        }

        // Subscriber should skip ahead and still be able to read.
        let mut received = alloc::vec::Vec::new();
        while let Some(v) = sub.try_recv(&bus) {
            received.push(v);
        }
        // Should get the last ring_size values (6, 7, 8, 9).
        assert!(!received.is_empty());
        assert!(received.len() <= 4);
        assert_eq!(*received.last().unwrap(), 9);
    }

    #[test]
    #[should_panic]
    fn rejects_non_power_of_two() {
        let _ = PodBus::<u64>::new(3);
    }

    #[test]
    #[should_panic]
    fn rejects_zero() {
        let _ = PodBus::<u64>::new(0);
    }
}
