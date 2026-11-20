// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Boot image (Phase 5 fast boot, `docs/feature-designs/real-jdk-mode.md`):
//! which side of a persistent boot image this process is on.
//!
//! The image is a property of the process's memory layout, not of a VM's
//! compatibility policy: the capture run places every allocation and mapping
//! made between the launcher prologue (P0) and the snapshot point (P1) at
//! addresses a restore run can map back, so the answer has to be readable by
//! the allocator and the mapping sites before any `SharedVm` exists. It is set
//! once, by the launcher, before P0; everything else only reads it.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// The process's relation to a boot image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ImageMode {
    /// No image: allocations and mappings go wherever the OS puts them.
    Off = 0,
    /// Booting to write an image: P0..P1 allocations and mappings are placed
    /// in the image's fixed ranges, and nothing that cannot be imaged (an OS
    /// thread, a JIT install) starts before P1.
    Capture = 1,
    /// Running from a mapped image: the fixed ranges are already populated;
    /// later allocations and mappings go wherever the OS puts them.
    Restored = 2,
}

static MODE: AtomicU8 = AtomicU8::new(ImageMode::Off as u8);

/// The process's image mode. `Off` unless the launcher set it.
#[inline]
pub fn mode() -> ImageMode {
    match MODE.load(Ordering::Acquire) {
        1 => ImageMode::Capture,
        2 => ImageMode::Restored,
        _ => ImageMode::Off,
    }
}

/// `true` while a capture run is between P0 and P1 (and after, until exit:
/// a capture process writes the image and exits without running `main`).
#[inline]
pub fn capturing() -> bool {
    mode() == ImageMode::Capture
}

/// Set the image mode. Launcher only, before P0. Panics on a second
/// transition other than `Capture`/`Off` -> `Restored`, which is the only
/// change after startup (the restore path flips a cold-started process once
/// its image is mapped).
pub fn set_mode(next: ImageMode) {
    let prev = mode();
    let legal = prev == next
        || prev == ImageMode::Off
        || (prev == ImageMode::Capture && next == ImageMode::Restored);
    assert!(legal, "image_layout: illegal mode change {prev:?} -> {next:?}");
    MODE.store(next as u8, Ordering::Release);
}

// ---------------------------------------------------------------------------
// The address broker (Phase 5 WP2)
// ---------------------------------------------------------------------------
//
// A capture run has to put every mapping it makes between P0 and P1 where a
// restore run can map it back. The kernel's choice (`mmap(NULL, ...)`) is
// randomised per process (ASLR), so each mapping site asks [`place`] for an
// address instead: a bump cursor over a fixed per-kind region, which gives the
// same answers for the same sequence of requests -- and a capture boot makes
// the same sequence, single-threaded, every time. The site then maps there
// with `MAP_FIXED_NOREPLACE` (never over something that is already there) and
// [`record`]s what it made, so the image writer knows what to dump (anonymous)
// or name (file-backed) and the restore run can put it back.
//
// Outside `Capture` every entry point answers `None` after one atomic load and
// the site does exactly what it did before this module existed.
//
// Nothing here panics a capture run that cannot be placed: the site falls back
// to its ordinary path (the VM still boots correctly) and [`spoil`] records why
// the image must not be written. The writer refuses a spoiled capture.

/// The page size every placement is rounded to (x86-64 / aarch64 4 KiB).
pub const PAGE: usize = 4096;

/// The address of WP1's capture arena (the allocator's P0..P1 heap). The
/// broker's regions sit above it, well clear.
pub const CAPTURE_ARENA_BASE: usize = 0x2000_0000_0000;

/// What a brokered mapping is for. Each kind has its own region
/// ([`SegmentKind::region`]), so a change in how many bytes one kind asks for
/// does not move every other kind's addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SegmentKind {
    /// A GC heap reservation (`gc/src/reservation.rs` `platform_reserve`):
    /// every collector's arena, 2 MiB aligned.
    Heap = 0,
    /// A GC side table on its own mapping (`map_side_table`: mark bitmaps).
    GcSideTable = 1,
    /// A hugetlbfs pool hold (`HugetlbHold::take`).
    HugetlbHold = 2,
    /// JIT code-cache chunks and the code-adjacent data cells (one region, so
    /// a cell stays within `disp32` reach of the code that polls it).
    JitCode = 3,
    /// The safepoint poll-page pairs, which must end below 2 GiB
    /// (`jit/src/poll_page.rs`, `MAP_32BIT`).
    PollPage = 4,
    /// A class-path archive (jar / jmod) mapped read-only.
    ClassPathArchive = 5,
    /// The runtime image (`lib/modules`) mapped for the class loader.
    RuntimeImage = 6,
    /// The runtime image mapped for Java (`ImageNativeSubstrate`'s direct
    /// map, whose address escapes into a `ByteBuffer`).
    JimageDirect = 7,
}

/// Number of [`SegmentKind`]s.
const KINDS: usize = 8;

impl SegmentKind {
    /// Every kind, in discriminant order.
    pub const ALL: [SegmentKind; KINDS] = [
        SegmentKind::Heap,
        SegmentKind::GcSideTable,
        SegmentKind::HugetlbHold,
        SegmentKind::JitCode,
        SegmentKind::PollPage,
        SegmentKind::ClassPathArchive,
        SegmentKind::RuntimeImage,
        SegmentKind::JimageDirect,
    ];

    /// `(base, size)` of the kind's region.
    ///
    /// The high regions start 8 TiB above [`CAPTURE_ARENA_BASE`] and end at
    /// 0x4C00_0000_0000, under the PIE load base (`ELF_ET_DYN_BASE`,
    /// ~0x5555_5555_0000 on x86-64) and far under the kernel's top-down
    /// `mmap_base` (just below the stack, ~0x7F..), so neither the executable,
    /// its `brk` heap, nor a kernel-placed mapping lands in them. The poll
    /// pages need the low 2 GiB; 0x5000_0000.. sits above a non-PIE
    /// executable's image and `brk` start (0x40_0000..).
    pub const fn region(self) -> (usize, usize) {
        const TIB: usize = 1 << 40;
        match self {
            SegmentKind::Heap => (0x2800_0000_0000, 24 * TIB),
            SegmentKind::GcSideTable => (0x4000_0000_0000, 2 * TIB),
            SegmentKind::HugetlbHold => (0x4200_0000_0000, 2 * TIB),
            SegmentKind::JitCode => (0x4400_0000_0000, 2 * TIB),
            SegmentKind::ClassPathArchive => (0x4600_0000_0000, 2 * TIB),
            SegmentKind::RuntimeImage => (0x4800_0000_0000, 2 * TIB),
            SegmentKind::JimageDirect => (0x4A00_0000_0000, 2 * TIB),
            SegmentKind::PollPage => (0x5000_0000, 0x2000_0000),
        }
    }
}

/// Bump cursors, one per [`SegmentKind`] region. [`place`] uses the process's
/// one instance; tests build their own.
pub struct Broker {
    used: [AtomicUsize; KINDS],
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    /// A broker with every region empty.
    pub const fn new() -> Self {
        Self {
            used: [const { AtomicUsize::new(0) }; KINDS],
        }
    }

    /// The next `len` bytes of `kind`'s region at an `align`-aligned address
    /// (`align` is raised to [`PAGE`] and to a power of two; `len` is rounded
    /// up to whole pages). `None` for `len == 0` or when the region is full.
    /// The same sequence of calls on a fresh broker gives the same answers.
    pub fn place(&self, kind: SegmentKind, len: usize, align: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let align = align.max(PAGE).checked_next_power_of_two()?;
        let len = len.checked_next_multiple_of(PAGE)?;
        let (base, size) = kind.region();
        let cell = &self.used[kind as usize];
        let mut cur = cell.load(Ordering::Relaxed);
        loop {
            let start = base.checked_add(cur)?.checked_next_multiple_of(align)?;
            let end = start.checked_add(len)?;
            if end > base + size {
                return None;
            }
            match cell.compare_exchange_weak(cur, end - base, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return Some(start),
                Err(seen) => cur = seen,
            }
        }
    }

    /// Bytes of `kind`'s region handed out so far (alignment gaps included).
    pub fn used(&self, kind: SegmentKind) -> usize {
        self.used[kind as usize].load(Ordering::Relaxed)
    }
}

static BROKER: Broker = Broker::new();

/// A fixed address for a mapping of `len` bytes of `kind`, `align`-aligned,
/// in a capture run; `None` in any other mode (map wherever you did before).
///
/// A capture whose region is full also gets `None`, and the capture is
/// [`spoil`]ed: the site maps as it always did and the image is not written.
pub fn place(kind: SegmentKind, len: usize, align: usize) -> Option<usize> {
    if !capturing() {
        return None;
    }
    let at = BROKER.place(kind, len, align);
    if at.is_none() && len != 0 {
        spoil("image_layout: a brokered region is full");
    }
    at
}

static SPOILED: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Mark this capture as unusable: some P0..P1 mapping could not be placed or
/// recorded. The first reason is kept. The image writer must refuse to write
/// an image when [`spoiled`] answers `Some`.
pub fn spoil(reason: &'static str) {
    let _ = SPOILED.set(reason);
}

/// Why this capture cannot be imaged, if it cannot.
pub fn spoiled() -> Option<&'static str> {
    SPOILED.get().copied()
}

// ---------------------------------------------------------------------------
// The segment registry
// ---------------------------------------------------------------------------

/// What backs a recorded segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Anonymous memory: the image writer dumps its bytes. `hugetlb` marks a
    /// `MAP_HUGETLB` mapping (which a restore must take from the pool again).
    Anonymous {
        /// The mapping is (currently) `MAP_HUGETLB`.
        hugetlb: bool,
    },
    /// A file mapping: the restore run maps the same file again.
    File(FileIdentity),
}

/// Which file a file-backed segment maps, and the stat a restore checks
/// before mapping it again ([`FileIdentity::check`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// The file's path (canonical when it could be resolved at capture).
    pub path: std::path::PathBuf,
    /// The file offset of the segment's first byte.
    pub offset: u64,
    /// `MAP_SHARED` (else `MAP_PRIVATE`).
    pub shared: bool,
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// `st_size`.
    pub size: u64,
    /// `st_mtime`.
    pub mtime_sec: i64,
    /// `st_mtime_nsec`.
    pub mtime_nsec: i64,
}

impl FileIdentity {
    /// The identity of the file `meta` describes, mapped from `offset`.
    #[cfg(unix)]
    pub fn from_metadata(
        path: std::path::PathBuf,
        meta: &std::fs::Metadata,
        offset: u64,
        shared: bool,
    ) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            path,
            offset,
            shared,
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }

    /// `Ok` when `meta` describes the same file, unchanged; else the name of
    /// the first field that differs.
    #[cfg(unix)]
    pub fn check(&self, meta: &std::fs::Metadata) -> Result<(), &'static str> {
        use std::os::unix::fs::MetadataExt;
        if meta.dev() != self.dev {
            return Err("dev");
        }
        if meta.ino() != self.ino {
            return Err("inode");
        }
        if meta.size() != self.size {
            return Err("size");
        }
        if meta.mtime() != self.mtime_sec || meta.mtime_nsec() != self.mtime_nsec {
            return Err("mtime");
        }
        Ok(())
    }
}

/// One mapping made between P0 and P1, as it was made. `prot` is the raw
/// `PROT_*` of the `mmap` call: protections a site changes later (a heap
/// granule committed with `mprotect`, a poll page armed) are the writer's to
/// read at P1 (`/proc/self/maps` inside the recorded ranges).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// What the mapping is for.
    pub kind: SegmentKind,
    /// Its first byte.
    pub addr: usize,
    /// Its length in bytes (whole pages).
    pub len: usize,
    /// The `PROT_*` bits it was mapped with.
    pub prot: i32,
    /// Anonymous or which file.
    pub backing: Backing,
}

/// A list of [`Segment`]s. [`record`] / [`segments`] use the process's one
/// instance; tests build their own. A boot makes a few dozen mappings, so a
/// `Vec` under a mutex, reserved once, is all it needs.
pub struct Registry {
    segs: std::sync::Mutex<Vec<Segment>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry.
    pub const fn new() -> Self {
        Self {
            segs: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Segment>> {
        self.segs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add `seg`.
    pub fn record(&self, seg: Segment) {
        let mut segs = self.lock();
        if segs.capacity() == 0 {
            segs.reserve(64);
        }
        segs.push(seg);
    }

    /// Remove and return the segment starting at `addr` (it was unmapped).
    pub fn forget(&self, addr: usize) -> Option<Segment> {
        let mut segs = self.lock();
        let i = segs.iter().position(|s| s.addr == addr)?;
        Some(segs.swap_remove(i))
    }

    /// Mark the anonymous segment starting at `addr` as (not) `MAP_HUGETLB`,
    /// after its owner replaced the mapping in place. `false` if there is no
    /// such anonymous segment.
    pub fn retag_hugetlb(&self, addr: usize, hugetlb: bool) -> bool {
        let mut segs = self.lock();
        match segs.iter_mut().find(|s| s.addr == addr) {
            Some(Segment {
                backing: Backing::Anonymous { hugetlb: h },
                ..
            }) => {
                *h = hugetlb;
                true
            }
            _ => false,
        }
    }

    /// Every recorded segment, in address order.
    pub fn segments(&self) -> Vec<Segment> {
        let mut out = self.lock().clone();
        out.sort_by_key(|s| s.addr);
        out
    }
}

static REGISTRY: Registry = Registry::new();

/// Record a mapping made at a brokered address.
pub fn record(seg: Segment) {
    REGISTRY.record(seg);
}

/// Forget the recorded segment starting at `addr`, because its owner unmapped
/// it. A no-op (one atomic load) outside a capture run, so it can sit on every
/// release path.
#[inline]
pub fn forget(addr: usize) {
    if capturing() {
        REGISTRY.forget(addr);
    }
}

/// [`Registry::retag_hugetlb`] on the process registry; a no-op outside a
/// capture run.
#[inline]
pub fn retag_hugetlb(addr: usize, hugetlb: bool) {
    if capturing() {
        REGISTRY.retag_hugetlb(addr, hugetlb);
    }
}

/// Every segment recorded so far, in address order (the image writer's input).
pub fn segments() -> Vec<Segment> {
    REGISTRY.segments()
}

// ---------------------------------------------------------------------------
// Mapping at a brokered address (Linux), and mapping it back
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod sys {
    pub const PROT_READ: i32 = 0x1;
    pub const MAP_SHARED: i32 = 0x01;
    pub const MAP_PRIVATE: i32 = 0x02;
    pub const MAP_FIXED: i32 = 0x10;
    pub const MAP_HUGETLB: i32 = 0x40000;
    /// Linux 4.17+. An older kernel ignores it and treats the address as a
    /// hint, which [`super::map_fixed_noreplace`] detects.
    pub const MAP_FIXED_NOREPLACE: i32 = 0x10_0000;
    pub const MAP_FAILED: *mut u8 = !0usize as *mut u8;

    extern "C" {
        pub fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64)
            -> *mut u8;
        pub fn munmap(addr: *mut u8, len: usize) -> i32;
    }
}

/// `mmap` exactly at `addr`, never over an existing mapping. `None` (and the
/// capture spoiled) when the kernel refuses or places it elsewhere.
#[cfg(target_os = "linux")]
fn map_fixed_noreplace(
    addr: usize,
    len: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: i64,
) -> Option<*mut u8> {
    // SAFETY: `MAP_FIXED_NOREPLACE` never replaces an existing mapping; the
    // kernel either maps the free range `[addr, addr + len)` or fails.
    let p = unsafe {
        sys::mmap(
            addr as *mut u8,
            len,
            prot,
            (flags & !sys::MAP_FIXED) | sys::MAP_FIXED_NOREPLACE,
            fd,
            offset,
        )
    };
    if p == sys::MAP_FAILED || p.is_null() {
        spoil("image_layout: a brokered address was taken (MAP_FIXED_NOREPLACE refused)");
        return None;
    }
    if p as usize != addr {
        // A pre-4.17 kernel took the address as a hint.
        // SAFETY: `p` is the mapping just made, which nothing else names.
        unsafe { sys::munmap(p, len) };
        spoil("image_layout: the kernel does not honour MAP_FIXED_NOREPLACE");
        return None;
    }
    Some(p)
}

/// In a capture run, make an anonymous mapping of `len` bytes for `kind` at a
/// brokered `align`-aligned address and record it; `prot` / `flags` are the
/// site's own `mmap` arguments (`MAP_PRIVATE | MAP_ANONYMOUS | ...`; a
/// `MAP_FIXED` in them is dropped, `MAP_32BIT` is redundant with
/// [`SegmentKind::PollPage`]). `None` outside a capture run, off Linux, or
/// when the mapping could not be placed (the capture is then spoiled): the
/// site makes its ordinary mapping instead.
pub fn map_anonymous(
    kind: SegmentKind,
    len: usize,
    align: usize,
    prot: i32,
    flags: i32,
) -> Option<*mut u8> {
    #[cfg(target_os = "linux")]
    {
        if !capturing() {
            return None;
        }
        map_anonymous_in(&BROKER, &REGISTRY, kind, len, align, prot, flags)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (kind, len, align, prot, flags);
        None
    }
}

#[cfg(target_os = "linux")]
fn map_anonymous_in(
    broker: &Broker,
    registry: &Registry,
    kind: SegmentKind,
    len: usize,
    align: usize,
    prot: i32,
    flags: i32,
) -> Option<*mut u8> {
    let Some(addr) = broker.place(kind, len, align) else {
        if len != 0 {
            spoil("image_layout: a brokered region is full");
        }
        return None;
    };
    let len = len.next_multiple_of(PAGE);
    let p = map_fixed_noreplace(addr, len, prot, flags, -1, 0)?;
    registry.record(Segment {
        kind,
        addr,
        len,
        prot,
        backing: Backing::Anonymous {
            hugetlb: flags & sys::MAP_HUGETLB != 0,
        },
    });
    Some(p)
}

/// A whole file mapped read-only (`PROT_READ`, `MAP_SHARED`, as
/// `memmap2::MmapOptions::map` maps it) at a brokered address
/// ([`map_file_readonly`]). Unmapped on drop, unless [`FileMapping::leak`]ed.
pub struct FileMapping {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: a read-only mapping owned by this value alone; `&FileMapping` only
// reads it.
unsafe impl Send for FileMapping {}
// SAFETY: as above.
unsafe impl Sync for FileMapping {}

impl FileMapping {
    /// The first mapped byte.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// The mapped length (the file's length at mapping time).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always `false`: an empty file is never mapped.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Keep the mapping for the life of the process.
    pub fn leak(self) -> &'static [u8] {
        let me = std::mem::ManuallyDrop::new(self);
        // SAFETY: `[ptr, ptr + len)` is a live read-only mapping that is now
        // never unmapped.
        unsafe { std::slice::from_raw_parts(me.ptr, me.len) }
    }
}

impl std::ops::Deref for FileMapping {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `[ptr, ptr + len)` is a read-only mapping this value owns
        // until drop.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl AsRef<[u8]> for FileMapping {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Drop for FileMapping {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            forget(self.ptr as usize);
            // SAFETY: the mapping this value owns; nothing borrows it past
            // the value (the `Deref` borrow ends with it).
            unsafe { sys::munmap(self.ptr, self.len) };
        }
    }
}

/// In a capture run, map all of `file` (at `path`) read-only for `kind` at a
/// brokered address and record its identity; `None` outside a capture run,
/// off Linux, for an empty file, or when the mapping could not be placed (the
/// capture is then spoiled). The site then maps it as it always did.
pub fn map_file_readonly(
    kind: SegmentKind,
    file: &std::fs::File,
    path: &std::path::Path,
) -> Option<FileMapping> {
    #[cfg(target_os = "linux")]
    {
        if !capturing() {
            return None;
        }
        map_file_readonly_in(&BROKER, &REGISTRY, kind, file, path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (kind, file, path);
        None
    }
}

#[cfg(target_os = "linux")]
fn map_file_readonly_in(
    broker: &Broker,
    registry: &Registry,
    kind: SegmentKind,
    file: &std::fs::File,
    path: &std::path::Path,
) -> Option<FileMapping> {
    use std::os::unix::io::AsRawFd;
    let Ok(meta) = file.metadata() else {
        spoil("image_layout: could not stat a file mapped before P1");
        return None;
    };
    let len = usize::try_from(meta.len()).ok()?;
    if len == 0 {
        return None;
    }
    let Some(addr) = broker.place(kind, len, PAGE) else {
        spoil("image_layout: a brokered region is full");
        return None;
    };
    let ptr = map_fixed_noreplace(
        addr,
        len,
        sys::PROT_READ,
        sys::MAP_SHARED,
        file.as_raw_fd(),
        0,
    )?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    registry.record(Segment {
        kind,
        addr,
        len: len.next_multiple_of(PAGE),
        prot: sys::PROT_READ,
        backing: Backing::File(FileIdentity::from_metadata(canonical, &meta, 0, true)),
    });
    Some(FileMapping { ptr, len })
}

/// Why [`remap_file_segments`] refused; the launcher then cold boots.
#[derive(Debug)]
pub enum RemapError {
    /// A recorded file could not be opened or stat'ed.
    Open {
        /// The recorded path.
        path: std::path::PathBuf,
        /// The OS error.
        error: std::io::Error,
    },
    /// A recorded file is not the one the image was captured with.
    Changed {
        /// The recorded path.
        path: std::path::PathBuf,
        /// The first stat field that differs.
        field: &'static str,
    },
    /// The kernel refused to map a file back at its address.
    Map {
        /// The recorded path.
        path: std::path::PathBuf,
        /// The recorded address.
        addr: usize,
        /// The OS error.
        error: std::io::Error,
    },
    /// File segments cannot be restored on this platform.
    Unsupported,
}

impl std::fmt::Display for RemapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemapError::Open { path, error } => {
                write!(f, "boot image: cannot open {}: {error}", path.display())
            }
            RemapError::Changed { path, field } => write!(
                f,
                "boot image: {} changed since capture ({field} differs)",
                path.display()
            ),
            RemapError::Map { path, addr, error } => write!(
                f,
                "boot image: cannot map {} at {addr:#x}: {error}",
                path.display()
            ),
            RemapError::Unsupported => {
                write!(f, "boot image: file segments are not restorable here")
            }
        }
    }
}

impl std::error::Error for RemapError {}

/// Restore side: map every file-backed segment of `segments` back at its
/// recorded address (`MAP_FIXED`, recorded protection, offset and
/// shared/private), after checking every file's identity (dev, inode, size,
/// mtime) first -- so a changed file refuses before anything is mapped.
/// Anonymous segments are skipped (the image loader maps those). Returns the
/// number of segments mapped.
///
/// `MAP_FIXED` replaces whatever is at the address: the caller must own the
/// ranges (free, or its own placeholder reservations). A `Map` error after
/// earlier segments were mapped leaves those mapped; the caller cold boots in
/// a fresh process.
pub fn remap_file_segments(segments: &[Segment]) -> Result<usize, RemapError> {
    let files: Vec<(&Segment, &FileIdentity)> = segments
        .iter()
        .filter_map(|s| match &s.backing {
            Backing::File(id) => Some((s, id)),
            Backing::Anonymous { .. } => None,
        })
        .collect();
    if files.is_empty() {
        return Ok(0);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut opened = Vec::with_capacity(files.len());
        for &(seg, id) in &files {
            let open = |error| RemapError::Open {
                path: id.path.clone(),
                error,
            };
            let file = std::fs::File::open(&id.path).map_err(open)?;
            let meta = file.metadata().map_err(open)?;
            id.check(&meta).map_err(|field| RemapError::Changed {
                path: id.path.clone(),
                field,
            })?;
            opened.push((seg, id, file));
        }
        for (seg, id, file) in &opened {
            let flags = sys::MAP_FIXED
                | if id.shared {
                    sys::MAP_SHARED
                } else {
                    sys::MAP_PRIVATE
                };
            // SAFETY: `MAP_FIXED` at a range the caller owns (this function's
            // contract); the descriptor is open and its identity checked.
            let p = unsafe {
                sys::mmap(
                    seg.addr as *mut u8,
                    seg.len,
                    seg.prot,
                    flags,
                    file.as_raw_fd(),
                    id.offset as i64,
                )
            };
            if p == sys::MAP_FAILED || p as usize != seg.addr {
                return Err(RemapError::Map {
                    path: id.path.clone(),
                    addr: seg.addr,
                    error: std::io::Error::last_os_error(),
                });
            }
        }
        Ok(opened.len())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(RemapError::Unsupported)
    }
}

/// One OS thread of this process, as the capture run's P1 census sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsThread {
    /// The kernel thread id (`gettid`).
    pub tid: u64,
    /// The thread's name as the kernel keeps it (`comm`: at most 15 bytes on
    /// Linux, so a `std::thread::Builder::name` is truncated there).
    pub name: String,
}

/// Every OS thread of this process (Phase 5 fast boot, WP3): the capture
/// run's check at P1, where no thread but the launcher thread and `main-vm`
/// may exist (an OS thread cannot be imaged). Linux reads
/// `/proc/self/task/*/comm`; `None` where the platform has no such listing
/// (or `/proc` is not mounted). Sorted by tid. A thread can exit between the
/// directory read and its `comm` read; such a thread is left out.
pub fn os_thread_census() -> Option<Vec<OsThread>> {
    #[cfg(target_os = "linux")]
    {
        // Collect tids from two fast `read_dir` passes before opening `comm`
        // files: `proc_task_readdir` can skip a live sibling when another
        // thread exits mid-iteration, and keeping `open/read` outside the
        // directory walk shrinks that window.
        let mut tids = std::collections::BTreeSet::new();
        let mut saw_dir = false;
        for _ in 0..2 {
            let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
                continue;
            };
            saw_dir = true;
            for entry in entries.flatten() {
                if let Some(tid) = entry.file_name().to_str().and_then(|t| t.parse::<u64>().ok()) {
                    tids.insert(tid);
                }
            }
        }
        if !saw_dir {
            return None;
        }
        let mut threads = Vec::new();
        for tid in tids {
            let Ok(comm) = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")) else {
                continue;
            };
            threads.push(OsThread {
                tid,
                name: comm.trim_end_matches('\n').to_string(),
            });
        }
        Some(threads)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The calling thread's kernel thread id, to match against
/// [`os_thread_census`] (Linux only; `None` elsewhere).
pub fn current_os_tid() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // `/proc/thread-self` links to `<pid>/task/<tid>` (Linux 3.17+); no
        // `libc` dependency for one syscall.
        let link = std::fs::read_link("/proc/thread-self").ok()?;
        link.file_name()?.to_str()?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The capture run's P1 check: the threads of [`os_thread_census`] that are
/// neither the process's initial thread (the launcher; tid == pid) nor one of
/// `allowed_tids` (the caller passes `main-vm`'s, normally its own
/// [`current_os_tid`]). Empty means the process may be imaged; `None` where
/// no census exists. A non-empty answer names what still has to be deferred.
pub fn threads_blocking_capture(allowed_tids: &[u64]) -> Option<Vec<OsThread>> {
    let pid = u64::from(std::process::id());
    Some(
        os_thread_census()?
            .into_iter()
            .filter(|t| t.tid != pid && !allowed_tids.contains(&t.tid))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
