//! Cache-line-aligned wrapper to prevent false sharing.
//!
//! Equivalent to `crossbeam_utils::CachePadded`. Alignment is 128 bytes
//! on x86_64, aarch64, and powerpc64 (where adjacent cache lines can be
//! prefetched together), and 64 bytes on all other architectures.
//!
//! Source: crossbeam-utils (MIT/Apache-2.0), inlined to drop the dependency.

use std::ops::{Deref, DerefMut};

#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
    ),
    repr(align(128))
)]
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
    )),
    repr(align(64))
)]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    pub const fn new(t: T) -> CachePadded<T> {
        CachePadded { value: t }
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}
