// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared, zero-copy byte view into a parent `Arc<[u8]>` allocation.
//!
//! `ByteView` exists to fix the round-4 regression where
//! `Arc::from(&source[range])` was claimed to share the parent class-file
//! buffer but actually allocated a brand-new `ArcInner<[u8]>` and memcpy'd
//! the slice (see `impl From<&[T]> for Arc<[T]>`). `ByteView` stores a
//! refcount-bumped clone of the parent `Arc<[u8]>` plus a `start..end`
//! range; constructing one from the class-file hot path is a single
//! atomic `Arc::clone` and no memcpy.
//!
//! Most consumers read these bytes as `&[u8]`; the `Deref<Target=[u8]>`
//! and `AsRef<[u8]>` impls below make `ByteView` a drop-in replacement
//! for `Arc<[u8]>` at use sites that only need slice access.
//!
//! For consumers that genuinely need an owned `Arc<[u8]>` (e.g.
//! `VtableMethodSnapshot.code`, `Frame.code`), call [`ByteView::to_arc`].
//! That path *does* allocate + memcpy, but it does so exactly once at
//! vtable installation rather than on every parse, matching the cost
//! profile the round-4 fix originally claimed.

use std::ops::{Deref, Range};
use std::sync::Arc;

use crate::class_reader_error::ClassReaderError;

/// Reference-counted bytes whose storage may be either an ordinary
/// `Arc<[u8]>` or an external immutable backing such as a file mapping.
///
/// Keeping the storage abstraction in the reader lets class-file `ByteView`s
/// point directly into stored ZIP entries without copying the whole archive
/// or class into a second allocation.
#[derive(Clone)]
pub enum SharedBytes {
    Owned(Arc<[u8]>),
    External(Arc<dyn AsRef<[u8]> + Send + Sync>),
}

impl SharedBytes {
    #[inline]
    pub fn from_external<T>(source: T) -> Self
    where
        T: AsRef<[u8]> + Send + Sync + 'static,
    {
        Self::External(Arc::new(source))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    /// Whether the bytes retain an external immutable backing such as a
    /// read-only JAR mapping instead of owning a copied allocation.
    #[inline]
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }
}

impl From<Vec<u8>> for SharedBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Owned(Arc::from(bytes))
    }
}

impl From<Arc<[u8]>> for SharedBytes {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self::Owned(bytes)
    }
}

impl From<&[u8]> for SharedBytes {
    fn from(bytes: &[u8]) -> Self {
        Self::Owned(Arc::from(bytes))
    }
}

impl AsRef<[u8]> for SharedBytes {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::External(bytes) => bytes.as_ref().as_ref(),
        }
    }
}

impl Deref for SharedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl std::fmt::Debug for SharedBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBytes")
            .field("len", &self.len())
            .field(
                "storage",
                &match self {
                    Self::Owned(_) => "owned",
                    Self::External(_) => "external",
                },
            )
            .finish()
    }
}

/// A zero-copy view into a shared `Arc<[u8]>` buffer.
///
/// Cloning a `ByteView` is a single atomic refcount bump on the parent
/// `Arc<[u8]>` plus two `usize` copies. Dereferencing yields the slice
/// `&source[start..end]`.
#[derive(Clone)]
pub struct ByteView {
    source: SharedBytes,
    start: usize,
    end: usize,
}

impl ByteView {
    /// Construct a view into `source[range]`.
    ///
    /// # Panics
    ///
    /// Panics if `range.start > range.end`, or if `range.end` exceeds
    /// `source.len()`. This constructor is retained for static fixtures
    /// (tests, hand-built buffers) where the range is provably in bounds
    /// at compile time. Any call site that derives the range from
    /// untrusted bytes — class-file payload offsets, attribute lengths,
    /// `body_offset + buf.position()` arithmetic, etc. — MUST use
    /// [`ByteView::try_new`] instead so an OOB range surfaces as a
    /// [`ClassReaderError`] rather than aborting the process.
    ///
    /// A round-11 off-by-`buf.position()` bug shipped a panic via this
    /// constructor that was only caught by a regression test; the
    /// runtime-offset call sites in `attribute.rs` have since been moved
    /// to `try_new` for defense-in-depth.
    ///
    /// Audit fix (LOW — DoS surface): narrowed from `pub` to
    /// `pub(crate)` so a downstream crate cannot reach this panicking
    /// constructor and turn a malformed class file into a process abort.
    /// In-crate callers that have a provably in-bounds range may still
    /// use it; everything deriving a range from untrusted bytes must use
    /// [`ByteView::try_new`], which returns a [`ClassReaderError`].
    #[deprecated(note = "prefer try_new for runtime-derived offsets")]
    #[track_caller]
    #[inline]
    pub(crate) fn new(source: impl Into<SharedBytes>, range: Range<usize>) -> Self {
        let source = source.into();
        assert!(range.start <= range.end, "ByteView range start > end");
        assert!(
            range.end <= source.len(),
            "ByteView range end {} exceeds source length {}",
            range.end,
            source.len()
        );
        Self {
            source,
            start: range.start,
            end: range.end,
        }
    }

    /// Checked constructor: construct a view into `source[range]`, or
    /// return [`ClassReaderError::InvalidClassData`] if the range is
    /// malformed (`start > end`) or falls outside `source`.
    ///
    /// Use this instead of [`ByteView::new`] whenever the range is not
    /// already known to be in bounds — it never panics. The reader
    /// hot path in `attribute.rs` (StackMapTable entries, Code bytecode,
    /// Unknown attribute data) uses this constructor so a malformed
    /// class file produces an error instead of aborting the process.
    #[inline]
    pub fn try_new(
        source: impl Into<SharedBytes>,
        range: Range<usize>,
    ) -> Result<Self, ClassReaderError> {
        let source = source.into();
        if range.start > range.end || range.end > source.len() {
            return Err(ClassReaderError::InvalidClassData {
                message: format!(
                    "ByteView range {}..{} out of bounds for source of length {}",
                    range.start,
                    range.end,
                    source.len()
                ),
            });
        }
        Ok(Self {
            source,
            start: range.start,
            end: range.end,
        })
    }

    /// Build a view from an owned `Vec<u8>` — convenience for tests and
    /// callers that don't have a pre-existing shared buffer. The vector
    /// is converted into an `Arc<[u8]>` (single allocation) and the view
    /// covers its whole length.
    #[inline]
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        let source = SharedBytes::from(bytes);
        Self {
            source,
            start: 0,
            end: len,
        }
    }

    /// Build a view from a borrowed slice — copies once into a fresh
    /// `Arc<[u8]>`. Convenience for test fixtures; production code on
    /// the reader hot path should use [`ByteView::try_new`] with the
    /// shared class-file buffer.
    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Self {
        let len = bytes.len();
        let source = SharedBytes::from(bytes);
        Self {
            source,
            start: 0,
            end: len,
        }
    }

    /// Empty view — does not allocate.
    #[inline]
    pub fn empty() -> Self {
        let empty = SharedBytes::from(Vec::<u8>::new());
        Self {
            source: empty,
            start: 0,
            end: 0,
        }
    }

    /// Returns the underlying byte slice.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY of bounds: enforced by `ByteView::new`.
        &self.source.as_ref()[self.start..self.end]
    }

    /// Number of bytes in the view.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// `true` if the view is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// Materialize a fresh, standalone `Arc<[u8]>` containing only the
    /// view's bytes. Allocates + memcpys (`Arc::from(&[u8])`); intended
    /// for downstream consumers like `VtableMethodSnapshot.code` and
    /// `Frame.code` whose lifetimes are decoupled from the class-file
    /// buffer.
    ///
    /// Same one-time cost as the broken `Arc::from(&source[range])`
    /// previously paid on every parse — except this happens at vtable
    /// installation, not during the parse hot path.
    #[inline]
    pub fn to_arc(&self) -> Arc<[u8]> {
        Arc::from(self.as_bytes())
    }
}

impl Default for ByteView {
    /// Empty view with no allocation pressure beyond the shared empty Arc.
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl Deref for ByteView {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for ByteView {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl std::fmt::Debug for ByteView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Mirror the `Arc<[u8]>` Debug shape so format!("{:?}", view)
        // stays terse in transitive Debug output.
        f.debug_struct("ByteView")
            .field("len", &self.len())
            .finish()
    }
}

impl PartialEq for ByteView {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for ByteView {}

impl PartialEq<[u8]> for ByteView {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_bytes() == other
    }
}

impl PartialEq<&[u8]> for ByteView {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_bytes() == *other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for ByteView {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.as_bytes() == other.as_slice()
    }
}

impl PartialEq<Vec<u8>> for ByteView {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.as_bytes() == other.as_slice()
    }
}

impl From<Vec<u8>> for ByteView {
    /// Convert from a `Vec<u8>` via `ByteView::from_vec`. Allocates a
    /// single `Arc<[u8]>` and the view covers the whole buffer. Convenience
    /// for test fixtures (`vec![...].into()`) and producers that already
    /// own the bytes.
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl From<&[u8]> for ByteView {
    /// Convert from a borrowed slice via `ByteView::from_slice`. Copies
    /// once into a fresh `Arc<[u8]>`.
    #[inline]
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl<const N: usize> From<[u8; N]> for ByteView {
    /// Convert from a fixed-size array (test convenience for
    /// `[0xB1u8].into()`-style call sites).
    #[inline]
    fn from(bytes: [u8; N]) -> Self {
        Self::from_slice(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_view_shares_parent_arc() {
        let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
        let strong_before = Arc::strong_count(&parent);
        let view = ByteView::try_new(Arc::clone(&parent), 2..6).expect("in-bounds range");
        assert_eq!(Arc::strong_count(&parent), strong_before + 1);
        assert_eq!(view.as_bytes(), &[3u8, 4, 5, 6][..]);
        assert_eq!(view.len(), 4);
        assert!(!view.is_empty());

        // Clone is a refcount bump, no extra allocation.
        let cloned = view.clone();
        assert_eq!(Arc::strong_count(&parent), strong_before + 2);
        assert_eq!(cloned.as_bytes(), view.as_bytes());
    }

    #[test]
    fn deref_yields_slice() {
        let view = ByteView::from_vec(vec![10u8, 20, 30]);
        let slice: &[u8] = &view;
        assert_eq!(slice, &[10u8, 20, 30][..]);
        assert_eq!(view[0], 10);
        assert_eq!(view.len(), 3);
    }

    #[test]
    fn from_slice_and_from_vec_round_trip() {
        let v = ByteView::from_slice(&[1u8, 2, 3]);
        assert_eq!(v.as_bytes(), &[1u8, 2, 3][..]);
        let v2 = ByteView::from_vec(vec![4, 5]);
        assert_eq!(v2.as_bytes(), &[4u8, 5][..]);
    }

    #[test]
    fn empty_view_is_empty() {
        let v = ByteView::empty();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
        assert_eq!(v.as_bytes(), &[] as &[u8]);
    }

    #[test]
    fn to_arc_returns_independent_arc() {
        let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4]);
        let view = ByteView::try_new(Arc::clone(&parent), 1..3).expect("in-bounds range");
        let owned: Arc<[u8]> = view.to_arc();
        assert_eq!(&*owned, &[2u8, 3][..]);
        // owned is decoupled from parent.
        drop(view);
        drop(parent);
        assert_eq!(&*owned, &[2u8, 3][..]);
    }

    #[test]
    #[should_panic]
    #[allow(deprecated)]
    fn new_panics_on_out_of_bounds_range() {
        let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3]);
        let _ = ByteView::new(parent, 0..10);
    }

    #[test]
    fn try_new_returns_err_for_invalid_ranges() {
        let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4]);
        // Out-of-bounds end produces an InvalidClassData error rather
        // than panicking — this is the primary contract that lets the
        // reader's hot path tolerate malformed offsets.
        let err = ByteView::try_new(Arc::clone(&parent), 0..10).unwrap_err();
        match err {
            ClassReaderError::InvalidClassData { message } => {
                assert!(
                    message.contains("out of bounds"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected InvalidClassData, got {other:?}"),
        }
        // Inverted range.
        assert!(ByteView::try_new(Arc::clone(&parent), 3..1).is_err());
        // Valid range succeeds and yields the expected slice.
        let v = ByteView::try_new(Arc::clone(&parent), 1..3).expect("in-bounds range");
        assert_eq!(v.as_bytes(), &[2u8, 3][..]);
        // Whole-buffer and empty-at-end ranges are valid.
        assert!(ByteView::try_new(Arc::clone(&parent), 0..4).is_ok());
        assert!(ByteView::try_new(parent, 4..4).is_ok());
    }

    /// Direct OOB test required by the soundness task: passing an
    /// `offset > buf.len()` must return Err (never panic).
    #[test]
    fn try_new_offset_past_end_returns_err() {
        let buf: Arc<[u8]> = Arc::from(vec![0u8; 4]);
        let err = ByteView::try_new(Arc::clone(&buf), 5..5).unwrap_err();
        assert!(matches!(err, ClassReaderError::InvalidClassData { .. }));
        let err = ByteView::try_new(buf, 2..100).unwrap_err();
        assert!(matches!(err, ClassReaderError::InvalidClassData { .. }));
    }
}
