// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s6/rs: fixed-capacity side tables allocated ZEROED instead of written
//! element by element (`CRATONVM_LAZY_ZERO_TABLES`, default ON, `=0` kill).
//!
//! # Why
//!
//! Several lock-free side tables are sized to a bound on class ids (1 M, 128 K,
//! 64 K ...) so their slot addresses stay stable, and are only sparsely used:
//! a Hello world defines ~1 000 classes. Their docs say the untouched pages
//! stay untouched, but every one was built with
//! `(0..N).map(|_| AtomicX::new(0)).collect()`, which WRITES every element:
//! the whole table is resident from its first use. The biggest are
//! `field_layout`'s replace counters (4 MiB, first JIT compile) and the VM's
//! statics index (2 MiB, first class with statics).
//!
//! An all-zero allocation (`Box::new_zeroed_slice` / `Arc::new_zeroed_slice`,
//! i.e. `alloc_zeroed`) gets fresh pages from the allocator (mimalloc's arena
//! slices are known-zero on first use; the system allocator's large blocks
//! are fresh mappings) and nothing is written: only the pages a slot write
//! reaches become resident. The values are the same: every converted
//! element's eager value is all-zero bytes ([`ZeroInit`]).
//!
//! The kill switch builds each table the old way, element by element.
//! A REUSED allocator block is still zeroed by the allocator (`memset`), so
//! the switch can never make a table cost more than before.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::{Arc, OnceLock};

/// A type for which the all-zero bit pattern is a valid value, equal to the
/// value the table's eager constructor writes.
///
/// # Safety
///
/// Implement only for types whose every byte may be zero (no references,
/// no `NonNull`, no enums without a zero discriminant) and whose
/// zero-valued instance is the one the table's eager path builds.
pub unsafe trait ZeroInit {}

// SAFETY: integer atomics are plain integers in memory; 0 is `new(0)`.
unsafe impl ZeroInit for AtomicU32 {}
// SAFETY: as above.
unsafe impl ZeroInit for AtomicU64 {}
// SAFETY: as above.
unsafe impl ZeroInit for AtomicUsize {}
// SAFETY: as above.
unsafe impl ZeroInit for AtomicI32 {}
// SAFETY: as above.
unsafe impl ZeroInit for AtomicI64 {}
// SAFETY: `AtomicBool` has the layout of `bool`; 0 is `false`.
unsafe impl ZeroInit for AtomicBool {}
// SAFETY: `AtomicPtr<T>` has the layout of `*mut T`; all-zero is null.
unsafe impl<T> ZeroInit for std::sync::atomic::AtomicPtr<T> {}
// SAFETY: plain integers.
unsafe impl ZeroInit for u32 {}
// SAFETY: plain integers.
unsafe impl ZeroInit for u64 {}
// SAFETY: plain integers.
unsafe impl ZeroInit for usize {}

/// `CRATONVM_LAZY_ZERO_TABLES`: default ON; `0` / `false` / `off` / `no`
/// build every table element by element, as before. Read once per process.
pub fn lazy_zero_tables() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::flags::runtime_flag_default_on("CRATONVM_LAZY_ZERO_TABLES"))
}

/// A boxed slice of `n` zero values: allocated zeroed by default, built with
/// `eager` element by element under the kill switch.
pub fn zeroed_boxed_slice<T: ZeroInit>(n: usize, eager: impl FnMut() -> T) -> Box<[T]> {
    zeroed_boxed_slice_with(lazy_zero_tables(), n, eager)
}

/// [`zeroed_boxed_slice`] with the switch decided by the caller (tests).
pub fn zeroed_boxed_slice_with<T: ZeroInit>(
    lazy: bool,
    n: usize,
    mut eager: impl FnMut() -> T,
) -> Box<[T]> {
    if lazy {
        // SAFETY: `T: ZeroInit`, so the zeroed memory is `n` valid `T`s.
        unsafe { Box::<[T]>::new_zeroed_slice(n).assume_init() }
    } else {
        (0..n).map(|_| eager()).collect()
    }
}

/// An `Arc`'d slice of `n` zero values; see [`zeroed_boxed_slice`].
pub fn zeroed_arc_slice<T: ZeroInit>(n: usize, eager: impl FnMut() -> T) -> Arc<[T]> {
    zeroed_arc_slice_with(lazy_zero_tables(), n, eager)
}

/// [`zeroed_arc_slice`] with the switch decided by the caller (tests).
pub fn zeroed_arc_slice_with<T: ZeroInit>(
    lazy: bool,
    n: usize,
    mut eager: impl FnMut() -> T,
) -> Arc<[T]> {
    if lazy {
        // SAFETY: `T: ZeroInit`, so the zeroed memory is `n` valid `T`s.
        unsafe { Arc::<[T]>::new_zeroed_slice(n).assume_init() }
    } else {
        (0..n).map(|_| eager()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn zs6rs_both_arms_build_the_same_zero_table() {
        for lazy in [true, false] {
            let b = zeroed_boxed_slice_with(lazy, 1 << 16, || AtomicU32::new(0));
            assert_eq!(b.len(), 1 << 16);
            assert!(b.iter().all(|a| a.load(Ordering::Relaxed) == 0));
            b[12345].store(7, Ordering::Relaxed);
            assert_eq!(b[12345].load(Ordering::Relaxed), 7);
            let a = zeroed_arc_slice_with(lazy, 4096, || AtomicU64::new(0));
            assert_eq!(a.len(), 4096);
            assert!(a.iter().all(|x| x.load(Ordering::Relaxed) == 0));
            let p = zeroed_boxed_slice_with(lazy, 64, || {
                std::sync::atomic::AtomicPtr::<u8>::new(std::ptr::null_mut())
            });
            assert!(p.iter().all(|x| x.load(Ordering::Relaxed).is_null()));
        }
        // An empty table is fine in both arms.
        assert!(zeroed_boxed_slice_with(true, 0, || AtomicU64::new(0)).is_empty());
        assert!(zeroed_arc_slice_with(false, 0, || AtomicU64::new(0)).is_empty());
    }

    #[test]
    fn zs6rs_the_lazy_arm_does_not_call_the_eager_builder() {
        let mut calls = 0;
        let _ = zeroed_boxed_slice_with(true, 1000, || {
            calls += 1;
            AtomicU64::new(0)
        });
        assert_eq!(calls, 0);
        let _ = zeroed_boxed_slice_with(false, 1000, || {
            calls += 1;
            AtomicU64::new(0)
        });
        assert_eq!(calls, 1000);
    }
}
