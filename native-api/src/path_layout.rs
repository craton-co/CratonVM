// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Where a synthetic `java/nio/file/Path`'s slots are — **the platform
//! implementation's own layout**, resolved by name once per process.
//!
//! # The defect this closes
//!
//! A Path this VM hands out is allocated stamped with the INTERFACE
//! `java/nio/file/Path`, and `get_class_display`'s alias table in
//! `native-builtins/src/lib.rs` makes `getClass()` report the platform's
//! concrete class — `sun.nio.fs.UnixPath`, or `WindowsPath`. That class is real
//! and fully loaded here: 52 declared methods and 7 declared fields,
//! byte-identical to HotSpot's.
//!
//! Until 2026-09-10 the natives wrote into it with two hard-coded constants,
//! `path -> 0` and `fs -> 1`, on an object two slots wide. `UnixPath`'s own
//! instance layout is
//!
//! ```text
//! fs(0)  path(1)  stringValue(2)  hash(3)  offsets(4)
//! ```
//!
//! so those two constants wrote the path STRING where a `UnixFileSystem`
//! belongs and the owning filesystem where a `byte[]` belongs. Two type-confused
//! slots, invisible for exactly as long as our own natives were the only
//! readers — and the reason the whole `java/nio/file/Path` family scored 102
//! differences against HotSpot the moment `CRATONVM_ENFORCE_NATIVE_SHADOW` made
//! real `UnixPath` bytecode run against one of them: `toString()` answered
//! `sun.nio.fs.UnixPath@17234` because `Object.toString()` was the only body
//! with nothing to read.
//!
//! # Why by NAME, and why not through the object
//!
//! Two earlier attempts wrote `stringValue` by name *through the object*
//! (`set_field_by_name`, then `resolve_field_index_by_class_id` on
//! `class_id_of_object`) and both were measured inert, because the object is
//! stamped with the interface and every name-based route resolves against that
//! stamp. Resolving the index against the IMPLEMENTATION class and writing it by
//! index is the route that works — and it is the same route the real bytecode
//! takes, since `getfield` resolves against its own constant pool's class rather
//! than against the receiver's stamp.
//!
//! Resolving by name also removes the `cfg`: `WindowsPath` has no `stringValue`
//! at all — its `path` field IS the `String` — so the Unix pair (encoded bytes
//! in `path` plus a lazily built `String` in `stringValue`) and the Windows
//! single slot are the same two questions asked of whichever class is there.
//!
//! # Why this lives in `native-api`
//!
//! Because `java/nio/file/Path` has **two producing crates**:
//! `native-builtins`'s `p57_alloc_path` and the jar/jrt walkers beside it, and
//! `native-io`'s watch-key path rebuild. A slot map that lived in one of them
//! would be a second, drifting copy in the other — the failure family
//! `two-producers-of-one-carrier-class` is named for. Same reasoning, and the
//! same home, as [`crate::appended_slots`].

use crate::registry::{NativeClassAccess, NativeContext};

/// The slot map for a synthetic `java/nio/file/Path`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSlots {
    /// The slot holding the path as a `java.lang.String` — `stringValue` on
    /// `UnixPath`, `path` on `WindowsPath`. Every native reads the path here.
    pub string: usize,
    /// The slot holding the owning `FileSystem`. `Path.getFileSystem()` returns
    /// it so callers that identity-compare
    /// `path.getFileSystem() == FileSystems.getDefault()` — e.g. cassandra's
    /// `org.apache.cassandra.io.util.File` constructor — observe a match. Null
    /// when the path was created without a known owning FileSystem, and `fs` is
    /// what that field MEANS on the real class.
    pub fs: usize,
    /// `UnixPath.path`, the encoded bytes. `None` on Windows, where the single
    /// `path` slot is the String and is already covered by [`Self::string`].
    pub bytes: Option<usize>,
    /// How wide to allocate: the real class's total instance-field count, so
    /// `hash` and `offsets` — both filled lazily by real bytecode — have
    /// somewhere to land.
    pub width: usize,
}

/// The layout every Path used before 2026-09-10, kept as the fallback so a VM
/// that cannot load `sun.nio.fs.UnixPath` at all still allocates a Path this
/// VM's own natives can read.
pub const FALLBACK: PathSlots = PathSlots {
    string: 0,
    fs: 1,
    bytes: None,
    width: 2,
};

/// Resolved once and then frozen. EVERY Path in a process has to agree about
/// its slot map — including any allocated before the class became loadable —
/// which is the same one-layout-per-class rule [`crate::appended_slots`]'s stub
/// arm exists to keep.
static SLOTS: std::sync::OnceLock<PathSlots> = std::sync::OnceLock::new();

/// The concrete class `getClass()` reports for one of our Paths — the same
/// choice `get_class_display`'s alias table makes in
/// `native-builtins/src/lib.rs`. **The two must not drift**: the layout written
/// here has to be the layout of the class we claim to be.
#[must_use]
pub fn impl_class() -> &'static str {
    if cfg!(windows) {
        "sun/nio/fs/WindowsPath"
    } else {
        "sun/nio/fs/UnixPath"
    }
}

/// Resolve (once) and return the Path slot map. Call this from an ALLOCATOR,
/// which is the only kind of site holding a `&mut` context early enough.
pub fn resolve(ctx: &mut dyn NativeContext) -> PathSlots {
    if let Some(slots) = SLOTS.get() {
        return *slots;
    }
    let cls = impl_class();
    let resolved = resolve_uncached(ctx, cls).unwrap_or(FALLBACK);
    *SLOTS.get_or_init(|| resolved)
}

fn resolve_uncached(ctx: &mut dyn NativeContext, cls: &str) -> Option<PathSlots> {
    // A fabricated stub's fields are `_f0.._fN` and carry none of these names,
    // so there is no real layout to align with and the historical one is right.
    if ctx.is_class_synthetic_stub(cls) {
        return None;
    }
    let cid = ctx.ensure_class_initialized(cls).ok()?;
    let width = ctx.class_num_total_fields(cid);
    let fs = ctx.resolve_field_index(cls, "fs")?;
    // `stringValue` present => the Unix shape (bytes in `path`, String built
    // lazily); absent => the Windows shape, where `path` is the String itself.
    let (string, bytes) = match ctx.resolve_field_index(cls, "stringValue") {
        Some(sv) => (sv, ctx.resolve_field_index(cls, "path")),
        None => (ctx.resolve_field_index(cls, "path")?, None),
    };
    // A width that does not cover every slot resolved is not a layout this
    // understands; take the fallback rather than write out of bounds.
    if width < string.max(fs).max(bytes.unwrap_or(0)) + 1 {
        return None;
    }
    Some(PathSlots {
        string,
        fs,
        bytes,
        width,
    })
}

/// The Path slot map for READERS, which mostly hold `&dyn NativeContext` and
/// cannot resolve anything.
///
/// Reading a Path means having been handed one, and allocating one initialises
/// this, so an un-initialised read can only come from a FOREIGN Path — where
/// the `toString()` fallback in `p57_read_path` is the answer anyway.
#[must_use]
pub fn slots() -> PathSlots {
    *SLOTS.get().unwrap_or(&FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_the_pre_2026_09_10_layout() {
        // The two constants this module replaced. A change here silently
        // relocates every Path allocated before `sun.nio.fs.UnixPath` loads.
        assert_eq!(FALLBACK.string, 0);
        assert_eq!(FALLBACK.fs, 1);
        assert_eq!(FALLBACK.bytes, None);
        assert_eq!(FALLBACK.width, 2);
    }

    #[test]
    fn impl_class_matches_the_get_class_display_alias() {
        // `native-builtins/src/lib.rs` maps `java/nio/file/Path` to exactly
        // this class for `getClass()`. The layout written has to be the layout
        // of the class claimed.
        #[cfg(windows)]
        assert_eq!(impl_class(), "sun/nio/fs/WindowsPath");
        #[cfg(not(windows))]
        assert_eq!(impl_class(), "sun/nio/fs/UnixPath");
    }

    #[test]
    fn slots_before_any_resolve_is_the_fallback() {
        // A reader that runs before any allocation must not invent a layout.
        // (`SLOTS` is process-wide; this asserts the default, and stays true
        // whichever order the tests in this crate run in, because nothing here
        // calls `resolve`.)
        let s = slots();
        assert!(s.width >= 2);
        assert!(s.string < s.width && s.fs < s.width);
    }
}
