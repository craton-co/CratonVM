//! Persistent boot image (Phase 5 fast boot): the on-disk format, the writer a
//! capture run uses at the snapshot point P1, and the loader a restored run
//! uses to map the image back before it would otherwise boot.
//!
//! The process-wide "which side of an image am I on" bit lives in
//! [`crate::image_layout`]; this module holds no state of its own (no statics),
//! so it can be called from the launcher prologue before anything else exists.
//!
//! # What an image holds
//!
//! 1. **Segments** -- fixed-address mappings recorded at capture (the capture
//!    arena, the address broker's VM mappings). An anonymous segment's bytes
//!    are stored in the image at a page-aligned file offset, and the loader maps
//!    the *image file itself* there with `MAP_PRIVATE | MAP_FIXED_NOREPLACE`, so
//!    pages fault in lazily and copy-on-write; nothing is read and copied. A
//!    trailing run of all-zero pages is not stored: it is mapped anonymous. A
//!    file-backed segment stores no bytes: the loader re-maps the original file
//!    after checking its identity (dev/inode/size/mtime).
//! 2. **Data diff** -- the bytes of the launcher binary's own writable segment
//!    (`.data` + `.bss`, minus the RELRO prefix) that changed between P0 and
//!    P1, as `(offset from load base, bytes)` runs. Applied with plain copies,
//!    and only after every segment mapped.
//! 3. **Root** -- one pointer (the `SharedVm`) handed back to the caller.
//! 4. **Key** -- every input that, if different, makes the image wrong for this
//!    process ([`BootKey`]). Any mismatch refuses the image; the caller cold
//!    boots.
//!
//! The byte layout is documented in [`format`].
//!
//! # Platforms
//!
//! Mapping, the writable-segment snapshot and most of the key are Linux-only
//! (`target_os = "linux"`, 64-bit). Elsewhere the format and writer still
//! compile and round-trip (the unit tests run on Windows), but
//! [`RwSnapshot::take`] returns `Unsupported` and [`try_map`] refuses with
//! [`Refusal::Unsupported`].

pub mod format;
mod hash;
mod key;
mod loader;
pub mod process_state;
mod rw_segment;
pub mod statics;
mod writer;

pub use format::{
    DataDiff, DiffRun, ImageMeta, Refusal, RwRegionRecord, SegEntry, FORMAT_VERSION, MAGIC,
};
pub use hash::hash64;
pub use key::{cache_dir, image_path, BootKey, KeyInputs, StatIdentity};
pub use loader::{try_map, try_map_with, MapOptions, MappedImage};
pub use rw_segment::RwSnapshot;
pub use writer::{segments_at_p1, split_by_maps, write_image, ImageSpec, SegmentSpec, WriteReport};

/// Segment protection bits. Numerically equal to Linux `PROT_*` so the loader
/// passes them to `mmap` unchanged.
pub mod prot {
    pub const NONE: u32 = 0;
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const EXEC: u32 = 4;
}

/// Segment kinds as stored. Opaque to the format; the values are WP2's
/// [`crate::image_layout::SegmentKind`] discriminants, plus [`kind::ARENA`] for
/// WP1's capture arena (which is not a brokered mapping).
pub mod kind {
    use crate::image_layout::SegmentKind;

    /// WP1's capture arena (all Rust allocations P0..P1).
    pub const ARENA: u32 = 0x100;
    /// Anything else the caller images.
    pub const OTHER: u32 = 0x1FF;

    /// The stored kind of a brokered segment.
    pub const fn of(k: SegmentKind) -> u32 {
        k as u32
    }
}

/// The page size of the running process.
pub fn page_size() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions.
        let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if v > 0 {
            return v as usize;
        }
    }
    4096
}

#[cfg(test)]
mod tests;
