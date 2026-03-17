// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The [`Pod`] marker trait for types safe to transmit through shared memory.

/// Plain-old-data types where every bit pattern is valid.
///
/// # Safety
///
/// Implementors must ensure:
/// 1. `T: Copy` (no destructor)
/// 2. `T: Send` (safe across threads/processes)
/// 3. Every bit pattern of `size_of::<T>()` bytes is a valid `T`
/// 4. No padding with validity constraints
///
/// # Example
///
/// ```
/// use crossbar::Pod;
///
/// #[repr(C)]
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// struct Tick {
///     price: f64,
///     volume: u32,
///     _pad: u32,
/// }
/// unsafe impl Pod for Tick {}
/// ```
pub unsafe trait Pod: Copy + Send + 'static {}

// Primitives
unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for u128 {}
unsafe impl Pod for i8 {}
unsafe impl Pod for i16 {}
unsafe impl Pod for i32 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for i128 {}
unsafe impl Pod for f32 {}
unsafe impl Pod for f64 {}
unsafe impl Pod for usize {}
unsafe impl Pod for isize {}

// Arrays
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

// Tuples (up to 12)
unsafe impl Pod for () {}
unsafe impl<A: Pod> Pod for (A,) {}
unsafe impl<A: Pod, B: Pod> Pod for (A, B) {}
unsafe impl<A: Pod, B: Pod, C: Pod> Pod for (A, B, C) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod> Pod for (A, B, C, D) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod> Pod for (A, B, C, D, E) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod> Pod for (A, B, C, D, E, F) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod> Pod for (A, B, C, D, E, F, G) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod> Pod
    for (A, B, C, D, E, F, G, H)
{
}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod, I: Pod> Pod
    for (A, B, C, D, E, F, G, H, I)
{
}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod, I: Pod, J: Pod> Pod
    for (A, B, C, D, E, F, G, H, I, J)
{
}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod, I: Pod, J: Pod, K: Pod>
    Pod for (A, B, C, D, E, F, G, H, I, J, K)
{
}
unsafe impl<
        A: Pod,
        B: Pod,
        C: Pod,
        D: Pod,
        E: Pod,
        F: Pod,
        G: Pod,
        H: Pod,
        I: Pod,
        J: Pod,
        K: Pod,
        L: Pod,
    > Pod for (A, B, C, D, E, F, G, H, I, J, K, L)
{
}
