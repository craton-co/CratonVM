// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! [`ThinArcStr`]: an `Arc<str>` held in 12 bytes with 4-byte alignment.
//!
//! An `Arc<str>` is a 16-byte fat pointer with 8-byte alignment. Inside an
//! enum or a struct beside narrower fields that costs a whole extra word: the
//! runtime constant pool's `ConstantPoolEntry` was 24 bytes a slot only because
//! its `Utf8(Arc<str>)` variant needed 16 bytes of payload at offset 8 (zs s2/fp2,
//! `docs/internal/gc/zgc-zr11md-constant-pool-slots-are-24-bytes-20261013.md`).
//!
//! `ThinArcStr` keeps the same allocation and the same strong count as the
//! `Arc<str>` it was made from (no copy, no second allocation), but stores the
//! data pointer and a `u32` length in a `#[repr(C, packed(4))]` pair: 12 bytes,
//! alignment 4. An enum whose widest other payload is 8 bytes then fits in 16
//! bytes. [`ThinArcStr::to_arc`] hands back an `Arc<str>` that is pointer-equal
//! to the original (a refcount bump), so interning identity
//! (`Arc::ptr_eq`) and `StringPool::prune_unreferenced`'s strong-count rule are
//! unchanged.
//!
//! Equality, ordering, hashing, `Debug` and `Display` are those of the `str`,
//! exactly as for `Arc<str>`.

use std::borrow::Borrow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;

/// An owned, reference-counted string: an `Arc<str>` in 12 bytes, alignment 4.
/// See the module documentation.
#[repr(C, packed(4))]
pub struct ThinArcStr {
    /// The `str` data pointer of an `Arc<str>` produced by `Arc::into_raw`.
    ptr: NonNull<u8>,
    /// The `str`'s length in bytes.
    len: u32,
}

// SAFETY: a `ThinArcStr` is an `Arc<str>` in a different encoding; `Arc<str>`
// is `Send + Sync` because `str` is.
unsafe impl Send for ThinArcStr {}
// SAFETY: as above.
unsafe impl Sync for ThinArcStr {}

impl ThinArcStr {
    /// Take ownership of `arc` without touching its count.
    ///
    /// # Panics
    ///
    /// If the string is 4 GiB or longer (no class-file string comes near: a
    /// `CONSTANT_Utf8` is at most 65 535 bytes).
    #[inline]
    pub fn from_arc(arc: Arc<str>) -> Self {
        let len = u32::try_from(arc.len()).expect("ThinArcStr: string of 4 GiB or more");
        let raw = Arc::into_raw(arc) as *const u8 as *mut u8;
        // SAFETY: `Arc::into_raw` never returns null.
        let ptr = unsafe { NonNull::new_unchecked(raw) };
        Self { ptr, len }
    }

    #[inline]
    fn raw(&self) -> *const str {
        let ptr = self.ptr;
        let len = self.len;
        std::ptr::slice_from_raw_parts(ptr.as_ptr() as *const u8, len as usize) as *const str
    }

    /// The string.
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `raw()` rebuilds the exact fat pointer `Arc::into_raw`
        // returned, and `self` holds one strong count, so the data is alive
        // and immutable for `&self`'s lifetime.
        unsafe { &*self.raw() }
    }

    /// A new `Arc<str>` sharing this string's allocation (one refcount bump);
    /// `Arc::ptr_eq` with the `Arc<str>` this was made from.
    #[inline]
    pub fn to_arc(&self) -> Arc<str> {
        let raw = self.raw();
        // SAFETY: `raw` came from `Arc::into_raw` and `self` keeps one count
        // alive; incrementing first and then adopting it yields an
        // independently owned `Arc`.
        unsafe {
            Arc::increment_strong_count(raw);
            Arc::from_raw(raw)
        }
    }

    /// Give the held count back as an `Arc<str>` without touching it.
    #[inline]
    pub fn into_arc(self) -> Arc<str> {
        let raw = self.raw();
        std::mem::forget(self);
        // SAFETY: `raw` came from `Arc::into_raw`; ownership of its count
        // moves to the returned `Arc` (`self` was forgotten).
        unsafe { Arc::from_raw(raw) }
    }

    /// `true` when both share one allocation (as `Arc::ptr_eq`).
    #[inline]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        let (pa, pb) = (a.ptr, b.ptr);
        pa == pb
    }

    /// `true` when `self` shares `arc`'s allocation.
    #[inline]
    pub fn ptr_eq_arc(&self, arc: &Arc<str>) -> bool {
        let p = self.ptr;
        std::ptr::eq(p.as_ptr() as *const u8, arc.as_ptr())
    }
}

impl Drop for ThinArcStr {
    #[inline]
    fn drop(&mut self) {
        let raw = self.raw();
        // SAFETY: `self` owns one strong count of the `Arc<str>` at `raw`.
        unsafe { drop(Arc::from_raw(raw)) }
    }
}

impl Clone for ThinArcStr {
    #[inline]
    fn clone(&self) -> Self {
        let raw = self.raw();
        // SAFETY: `self` keeps the allocation alive; the new value owns the
        // count added here.
        unsafe { Arc::increment_strong_count(raw) };
        Self {
            ptr: self.ptr,
            len: self.len,
        }
    }
}

impl Deref for ThinArcStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for ThinArcStr {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ThinArcStr {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<Arc<str>> for ThinArcStr {
    #[inline]
    fn from(arc: Arc<str>) -> Self {
        Self::from_arc(arc)
    }
}

impl From<&Arc<str>> for ThinArcStr {
    #[inline]
    fn from(arc: &Arc<str>) -> Self {
        Self::from_arc(Arc::clone(arc))
    }
}

impl From<&str> for ThinArcStr {
    /// A fresh, un-interned allocation, as `Arc::<str>::from(&str)`.
    #[inline]
    fn from(s: &str) -> Self {
        Self::from_arc(Arc::from(s))
    }
}

impl From<String> for ThinArcStr {
    #[inline]
    fn from(s: String) -> Self {
        Self::from_arc(Arc::from(s))
    }
}

impl From<&String> for ThinArcStr {
    #[inline]
    fn from(s: &String) -> Self {
        Self::from_arc(Arc::from(s.as_str()))
    }
}

impl From<Box<str>> for ThinArcStr {
    #[inline]
    fn from(s: Box<str>) -> Self {
        Self::from_arc(Arc::from(s))
    }
}

impl From<ThinArcStr> for Arc<str> {
    #[inline]
    fn from(s: ThinArcStr) -> Self {
        s.into_arc()
    }
}

impl From<&ThinArcStr> for Arc<str> {
    #[inline]
    fn from(s: &ThinArcStr) -> Self {
        s.to_arc()
    }
}

impl PartialEq for ThinArcStr {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for ThinArcStr {}

impl PartialEq<str> for ThinArcStr {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ThinArcStr {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialOrd for ThinArcStr {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ThinArcStr {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for ThinArcStr {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl fmt::Debug for ThinArcStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for ThinArcStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl Default for ThinArcStr {
    fn default() -> Self {
        Self::from("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zs2fp2_thin_arc_str_is_12_bytes_align_4() {
        assert_eq!(std::mem::size_of::<ThinArcStr>(), 12);
        assert_eq!(std::mem::align_of::<ThinArcStr>(), 4);
        // The null niche survives `packed`.
        assert_eq!(std::mem::size_of::<Option<ThinArcStr>>(), 12);
        // An enum beside an 8-byte payload fits in 16 bytes.
        #[allow(dead_code)]
        enum E {
            A(ThinArcStr),
            B(i64),
            C { x: u16, y: u16 },
        }
        assert_eq!(std::mem::size_of::<E>(), 16);
    }

    #[test]
    fn zs2fp2_thin_arc_str_shares_the_arc_and_its_count() {
        let arc: Arc<str> = Arc::from("java/lang/Object");
        let thin = ThinArcStr::from(Arc::clone(&arc));
        assert_eq!(Arc::strong_count(&arc), 2);
        assert!(thin.ptr_eq_arc(&arc));
        let back = thin.to_arc();
        assert!(Arc::ptr_eq(&back, &arc));
        assert_eq!(Arc::strong_count(&arc), 3);
        let c = thin.clone();
        assert_eq!(Arc::strong_count(&arc), 4);
        assert!(ThinArcStr::ptr_eq(&c, &thin));
        drop(c);
        drop(back);
        assert_eq!(Arc::strong_count(&arc), 2);
        let moved = thin.into_arc();
        assert_eq!(Arc::strong_count(&arc), 2);
        drop(moved);
        assert_eq!(Arc::strong_count(&arc), 1);
    }

    #[test]
    fn zs2fp2_thin_arc_str_compares_hashes_and_prints_as_str() {
        use std::collections::hash_map::DefaultHasher;
        let a = ThinArcStr::from("x\u{e9}y");
        let b = ThinArcStr::from(String::from("x\u{e9}y"));
        assert!(!ThinArcStr::ptr_eq(&a, &b));
        assert_eq!(a, b);
        assert_eq!(a, *"x\u{e9}y");
        assert_eq!(&*a, "x\u{e9}y");
        assert_eq!(a.len(), 4);
        let h = |s: &dyn Fn(&mut DefaultHasher)| {
            let mut st = DefaultHasher::new();
            s(&mut st);
            st.finish()
        };
        assert_eq!(h(&|st| a.hash(st)), h(&|st| "x\u{e9}y".hash(st)));
        let arc: Arc<str> = Arc::from("x\u{e9}y");
        assert_eq!(format!("{a:?}"), format!("{arc:?}"));
        assert_eq!(format!("{a}"), format!("{arc}"));
        let empty = ThinArcStr::default();
        assert_eq!(&*empty, "");
        assert_eq!(&*empty.to_arc(), "");
        assert!(ThinArcStr::from("a") < ThinArcStr::from("b"));
    }

    #[test]
    fn zs2fp2_thin_arc_str_keeps_interning_identity() {
        let pool = crate::intern::StringPool::new();
        let a = ThinArcStr::from(pool.intern_arc("java/lang/String"));
        let b = ThinArcStr::from(pool.intern_arc("java/lang/String"));
        assert!(ThinArcStr::ptr_eq(&a, &b));
        // Held by `a` and `b`: not prunable.
        assert_eq!(pool.prune_unreferenced(), 0);
        drop(a);
        drop(b);
        assert_eq!(pool.prune_unreferenced(), 1);
    }
}
