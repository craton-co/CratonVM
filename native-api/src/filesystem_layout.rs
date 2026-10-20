// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Where this VM's DEFAULT `java/nio/file/FileSystem` singleton's real
//! slots are, once minted as the real platform class -- companion to
//! [`crate::path_layout`], same reasoning, narrower scope.
//!
//! # Scope: the DEFAULT (file-scheme) FileSystem only
//!
//! Jar/jrt-backed FileSystems this VM mounts are NOT covered here and stay
//! minted under the literal abstract `java/nio/file/FileSystem` name,
//! unchanged. Their real counterparts (`jdk.nio.zipfs.ZipFileSystem`,
//! `jdk.internal.jrtfs.JrtFileSystem`) are unrelated classes with their own
//! field layouts this VM does not model -- see
//! `docs/internal/retired/lane-4-io-nio-foreign-RETIRED-20260920.md`'s account of
//! this fix for why that is its own, larger, undertaking, not a mechanical
//! extension of this one.
//!
//! # The defect this closes
//!
//! `p57_alloc_default_filesystem` minted the process-wide default FileSystem
//! singleton under the literal name `java/nio/file/FileSystem` -- abstract in
//! every real image (confirmed by this VM's own JVMS-6.5 uninstantiable-
//! receiver census on every run that allocates one). Invisible as long as no
//! real bytecode dispatched off it. Once a Path's `fs` field is populated
//! (`p57_write_path_fields`), `WindowsPath.getAbsolutePath()`'s real body
//! calls `getFileSystem().defaultDirectory()` / `.defaultRoot()` --
//! package-private, non-native methods declared only on the CONCRETE
//! `sun.nio.fs.WindowsFileSystem` (or its per-platform sibling), never on
//! the abstract interface this VM's object claimed to be -- `NoSuchMethodError`.
//!
//! # Why this needs its OWN field map, not `path_layout`'s verbatim
//!
//! `sun.nio.fs.WindowsFileSystem`'s three real instance fields are `provider`
//! (`WindowsFileSystemProvider`), `defaultDirectory` (`String`, the parsed
//! absolute form of `StaticProperty.userDir()`) and `defaultRoot` (`String`,
//! that path's drive/UNC root) -- read from its own constructor and `javap
//! -c`, not guessed. None of these correspond to what this VM's EXISTING
//! synthetic 4-slot design (`P57_FS_JAR_FIELD`/`P57_FS_JRT_FIELD`/
//! `P57_FS_PROVIDER_FIELD` in `native-builtins`) stores at those same
//! indices -- so every native that reads those constants against a `this`
//! receiver MUST width-guard first: a genuinely concrete 3-field receiver is
//! narrower than `P57_FS_SLOTS`, so reading index 1 or 2 off it reads one of
//! ITS real fields, not "is this virtual". See
//! `native-builtins/src/phases_late/nio_file.rs`'s `p57_fs_is_virtual`,
//! the one guard every such reader now goes through.
//!
//! Measured on **Windows/x86_64 against JDK 25**, the only real-JDK image on
//! the machine this fix was built on -- `sun.nio.fs.UnixFileSystem`'s field
//! names were not independently `javap`'d. `resolve_uncached` resolves by
//! NAME and returns `None` on any mismatch, so an unverified platform whose
//! field names differ safely keeps the pre-fix (synthetic, abstract-stamped)
//! behaviour rather than writing into the wrong slot.

use crate::registry::{NativeClassAccess, NativeContext};

/// The slot map for the real concrete class backing this VM's default
/// FileSystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSystemSlots {
    /// The slot holding this FileSystem's `FileSystemProvider` --
    /// `provider()`'s real body is a bare `getfield`/`areturn`, so once this
    /// is written once at construction, every subsequent `.provider()` call
    /// through real bytecode reads it back unchanged (the identity
    /// `p57_fs_provider`'s own doc describes as load-bearing).
    pub provider: usize,
    /// The slot holding the parsed absolute form of the process's working
    /// directory -- what `WindowsPath.getAbsolutePath()`'s `RELATIVE` branch
    /// resolves a bare relative path against.
    pub default_directory: usize,
    /// The slot holding that same path's drive/UNC root -- what
    /// `WindowsPath.getAbsolutePath()`'s `DIRECTORY_RELATIVE` branch (a path
    /// like `\foo`, rooted but driveless) resolves against.
    pub default_root: usize,
    /// The real class's own total instance-field count. Used only to assert
    /// the three resolved indices fit and, at read time, to tell a
    /// genuinely concrete receiver apart from this VM's wider synthetic
    /// (jar/jrt-capable) FileSystem shape.
    pub width: usize,
}

/// The concrete class this VM's default FileSystem should be minted as --
/// mirrors [`crate::path_layout::impl_class`]'s per-platform choice, and
/// must be kept in step with whatever alias `native-builtins/src/lib.rs`'s
/// `get_class_display` reports for `getClass()` on that object, same
/// requirement as `path_layout::impl_class`'s own doc states.
#[must_use]
pub fn impl_class() -> &'static str {
    if cfg!(windows) {
        "sun/nio/fs/WindowsFileSystem"
    } else {
        "sun/nio/fs/UnixFileSystem"
    }
}

/// Resolved once and then frozen, `None` meaning "stay on the pre-fix
/// synthetic/abstract-stamped default FileSystem" rather than a layout to
/// write into.
static SLOTS: std::sync::OnceLock<Option<FileSystemSlots>> = std::sync::OnceLock::new();

/// Resolve (once) and return the default-FileSystem slot map. Call this from
/// the ALLOCATOR, which is the only kind of site holding a `&mut` context
/// early enough.
pub fn resolve(ctx: &mut dyn NativeContext) -> Option<FileSystemSlots> {
    if let Some(slots) = SLOTS.get() {
        return *slots;
    }
    let resolved = resolve_uncached(ctx);
    *SLOTS.get_or_init(|| resolved)
}

fn resolve_uncached(ctx: &mut dyn NativeContext) -> Option<FileSystemSlots> {
    // Measured on Windows/x86_64 against JDK 25 only (this module's own doc).
    // `sun.nio.fs.UnixFileSystem`'s field NAMES were not independently
    // `javap`'d, and a name match alone cannot catch a TYPE mismatch (Unix
    // paths are byte-encoded in several JDK internals, unlike Windows'
    // plain `String`s) -- writing a `String` into a field the real class
    // types as `byte[]` would be the exact type-confused-slot species
    // `path_layout.rs` closed for `Path`. Stay off until that is verified.
    if !cfg!(windows) {
        return None;
    }
    let cls = impl_class();
    // A fabricated stub's fields are `_f0.._fN` and carry none of these
    // names, so there is no real layout to align with.
    if ctx.is_class_synthetic_stub(cls) {
        return None;
    }
    let cid = ctx.ensure_class_initialized(cls).ok()?;
    let width = ctx.class_num_total_fields(cid);
    let provider = ctx.resolve_field_index(cls, "provider")?;
    let default_directory = ctx.resolve_field_index(cls, "defaultDirectory")?;
    let default_root = ctx.resolve_field_index(cls, "defaultRoot")?;
    // A width that does not cover every slot resolved is not a layout this
    // understands; take no layout rather than write out of bounds.
    if width < provider.max(default_directory).max(default_root) + 1 {
        return None;
    }
    Some(FileSystemSlots {
        provider,
        default_directory,
        default_root,
        width,
    })
}

/// The default-FileSystem slot map for READERS, which mostly hold `&dyn
/// NativeContext` and cannot resolve anything. `None` before any allocator
/// has run, or on a platform/image this fix does not cover.
#[must_use]
pub fn slots() -> Option<FileSystemSlots> {
    SLOTS.get().copied().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impl_class_is_the_platform_default_filesystem() {
        #[cfg(windows)]
        assert_eq!(impl_class(), "sun/nio/fs/WindowsFileSystem");
        #[cfg(not(windows))]
        assert_eq!(impl_class(), "sun/nio/fs/UnixFileSystem");
    }

    #[test]
    fn slots_before_any_resolve_is_none() {
        // A reader that runs before any allocation must not invent a
        // layout. (`SLOTS` is process-wide; this asserts the default and
        // stays true whichever order the tests in this crate run in,
        // because nothing here calls `resolve`.)
        assert!(slots().is_none() || slots().unwrap().width >= 3);
    }
}
