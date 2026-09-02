// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap backing store: reserve address space, commit it in granules, and give
//! it back.
//!
//! # Why this exists
//!
//! [`crate::arena::Arena`] was a `Vec<u8>` of `capacity` bytes obtained from
//! `alloc_zeroed`, and every collector in this crate is built on it. That has
//! three consequences that are properties of the ALLOCATION, not of any
//! collector:
//!
//! 1. **The whole of `-Xmx` is charged at startup.** On Linux a multi-gigabyte
//!    `alloc_zeroed` falls through to `mmap` and the pages are demand-faulted,
//!    so the resident cost tracks use -- but on **Windows** the allocator hands
//!    a block that size to `VirtualAlloc(MEM_COMMIT | MEM_RESERVE)`, which
//!    takes the full commit charge against the page file immediately. A
//!    `-Xmx4g` process reserves 4 GB of commit before it has executed a
//!    bytecode, and `-Xms` has no meaning at all.
//! 2. **Nothing is ever returned to the OS.** The sweep retracts the bump
//!    cursor and the compactor empties whole megabytes, and the pages stay
//!    resident for the life of the process. A JVM that peaks and then idles
//!    holds its peak forever.
//! 3. **The capacity is fixed at construction.** `Arena::grow` exists but the
//!    ZGC path never calls it, and cannot: the object-start bitmap is gridded
//!    over the envelope captured at construction.
//!
//! A reservation fixes (1) and (2) directly, and it is the shape (3) needs.
//!
//! # The model
//!
//! Reserve `capacity` bytes of ADDRESS SPACE up front -- cheap, and it is what
//! makes the base pointer stable for the life of the heap, which the
//! object-start bitmap, `MOVABLE_BOUNDS` and `conservative_addr_span` all
//! depend on. Then commit in [`GRANULE`]-sized units as the allocator's cursors
//! reach them, and decommit a granule once the collector can prove nothing in
//! it is live.
//!
//! Committed memory reads as zero, on both platforms and in the fallback, which
//! is the contract `Arena` already relied on (`alloc_zeroed`) and which
//! `alloc_raw`'s per-object `write_bytes` exists to restore for REUSED blocks.
//!
//! # The fallback is not a lesser mode
//!
//! [`HeapStore::Owned`] is the `alloc_zeroed` block this replaces, kept for
//! three cases: a platform whose syscalls we do not declare, a reservation the
//! OS refuses, and `CRATONVM_GC_RESERVE=0`. It behaves exactly as before --
//! committing is a no-op because everything is already committed, and
//! decommitting is a no-op because there is nothing to give back. Callers
//! cannot tell the two apart except through [`HeapStore::committed_bytes`],
//! which is the point: nothing else in the crate has to know.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Commit granularity, in bytes.
///
/// 2 MiB, matching `ZgcRealHeap::Z_LOGICAL_PAGE_BYTES` so a granule and a
/// logical page are the same unit and a page the collector has emptied is a
/// granule this can hand back. Also a hugepage on x86-64, so a committed run is
/// a candidate for one rather than being split down the middle.
///
/// Smaller would track the live set more tightly and cost more syscalls; larger
/// would make the first allocation commit more than a fresh JVM ever uses.
pub const GRANULE: usize = 2 * 1024 * 1024;

/// Is the reserve/commit backing store in use? `CRATONVM_GC_RESERVE=0` forces
/// the `alloc_zeroed` block this replaced, byte for byte, so an A/B is a re-run
/// rather than a rebuild.
///
/// Read once per process: it decides how the heap is MAPPED, and a heap cannot
/// change its backing store mid-run.
fn reserve_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_GC_RESERVE") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    })
}

/// Bytes this process has committed across every live heap store. Diagnostics
/// only -- `--verbose:gc` reports it so "the heap is 4 GB" and "the process is
/// holding 4 GB" stop being the same sentence.
static COMMITTED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Total committed heap bytes, across every [`HeapStore`] alive in this
/// process.
pub fn process_committed_bytes() -> usize {
    COMMITTED_BYTES.load(Ordering::Relaxed)
}

/// The heap's backing bytes.
pub enum HeapStore {
    /// The pre-existing `alloc_zeroed` block. Wholly committed at construction.
    Owned(Vec<u8>),
    /// Reserved address space, committed in [`GRANULE`] units.
    Reserved(Reservation),
}

impl HeapStore {
    /// Back a heap of `capacity` bytes.
    ///
    /// Falls back to [`HeapStore::Owned`] when the kill switch is set, when the
    /// capacity is below one granule (a reservation would round it up and the
    /// bookkeeping would cost more than the heap), or when the OS refuses the
    /// reservation.
    pub fn new(capacity: usize, region: &str) -> Self {
        if capacity == 0 {
            return HeapStore::Owned(Vec::new());
        }
        if reserve_enabled() && capacity >= GRANULE {
            if let Some(res) = Reservation::reserve(capacity) {
                return HeapStore::Reserved(res);
            }
            // A REFUSED RESERVATION IS NOT A FAILURE HERE. `alloc_zeroed` may
            // still succeed -- it can serve from an already-mapped arena the
            // process holds -- so falling through gives the run the same chance
            // it had before this module existed. `heap_reservation_failed` is
            // still reached if that fails too.
            tracing::warn!(
                target: "cratonvm::gc",
                capacity,
                region,
                "gc: could not reserve address space for the heap; falling back \
                 to a wholly-committed block (no lazy commit, no uncommit)"
            );
        }
        HeapStore::Owned(crate::arena::alloc_zeroed_heap(capacity, region))
    }

    /// Base of the backing bytes. Stable for the life of the store.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        match self {
            HeapStore::Owned(v) => v.as_ptr(),
            HeapStore::Reserved(r) => r.base,
        }
    }

    /// Base of the backing bytes, mutably.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            HeapStore::Owned(v) => v.as_mut_ptr(),
            HeapStore::Reserved(r) => r.base,
        }
    }

    /// Capacity in bytes. Every offset in `[0, len())` is addressable; only
    /// the committed prefix/suffix is WRITABLE, which is what
    /// [`Self::commit_range`] is for.
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            HeapStore::Owned(v) => v.len(),
            HeapStore::Reserved(r) => r.len,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes currently committed. Equal to [`Self::len`] on the owned arm.
    pub fn committed_bytes(&self) -> usize {
        match self {
            HeapStore::Owned(v) => v.len(),
            HeapStore::Reserved(r) => r.committed_granules() * GRANULE,
        }
    }

    /// Make `[offset, offset + len)` writable, committing whatever granules it
    /// touches.
    ///
    /// **Every write into the store must be preceded by one of these**, and the
    /// allocator's two cursors are where that obligation is discharged: a bump
    /// commits what it is about to hand out, and a free-list block is by
    /// construction below a cursor that already committed it.
    ///
    /// Idempotent and cheap when the range is already committed -- a granule
    /// bitmap test per granule, no syscall. Returns `false` when the OS refuses,
    /// which the caller must treat as an allocation failure rather than
    /// ignoring: writing into uncommitted reserved space is a segfault, not a
    /// zero.
    #[must_use = "an uncommitted range must not be written to"]
    pub fn commit_range(&mut self, offset: usize, len: usize) -> bool {
        match self {
            HeapStore::Owned(_) => true,
            HeapStore::Reserved(r) => r.commit_range(offset, len),
        }
    }

    /// Zero `[start, end)`.
    ///
    /// Replaces `self.data[start..end].fill(0)` on the `Vec` this used to be.
    /// On the reserved arm it skips uncommitted granules, which is both the
    /// correct answer (they already read as zero, and writing to one would
    /// fault) and the fast one: `Arena::reset`'s full clear on a heap that has
    /// only ever touched its first megabyte costs a `memset` of that megabyte
    /// rather than of `-Xmx`.
    pub fn fill_zero(&mut self, start: usize, end: usize) {
        match self {
            HeapStore::Owned(v) => {
                let end = end.min(v.len());
                if end > start {
                    v[start..end].fill(0);
                }
            }
            HeapStore::Reserved(r) => r.fill_zero(start, end),
        }
    }

    /// The COMMITTED parts of `[start, end)` as absolute `(addr, len)` spans,
    /// adjacent ones coalesced — exactly the bytes [`Self::fill_zero`] would
    /// touch, handed to a caller that will zero them somewhere else (the
    /// generational heap's off-pause wipe of its evacuated semi-space).
    pub fn committed_spans(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        match self {
            HeapStore::Owned(v) => {
                let end = end.min(v.len());
                if end > start {
                    vec![(v.as_ptr() as usize + start, end - start)]
                } else {
                    Vec::new()
                }
            }
            HeapStore::Reserved(r) => r.committed_spans(start, end),
        }
    }

    /// Grow the usable capacity to `new_capacity`, returning the (possibly
    /// new) base pointer.
    ///
    /// `Arena::grow`'s backing operation. On the reserved arm a growth that
    /// still fits inside the reservation is free -- the address space is
    /// already there and the granules commit on demand -- which is the case
    /// that matters, because a reservation is rounded UP to a granule. A growth
    /// past the reservation takes a fresh one and copies, exactly as
    /// `Vec::resize` would reallocate; callers already treat the returned
    /// pointer as authoritative for that reason.
    pub fn grow_to(&mut self, new_capacity: usize) -> *const u8 {
        match self {
            HeapStore::Owned(v) => {
                if new_capacity > v.len() {
                    v.resize(new_capacity, 0);
                }
                v.as_ptr()
            }
            HeapStore::Reserved(r) => {
                if new_capacity <= r.reserved_len {
                    r.len = r.len.max(new_capacity);
                    return r.base;
                }
                match Reservation::reserve(new_capacity) {
                    Some(mut next) => {
                        // COPY ONLY WHAT IS COMMITTED IN THE SOURCE.
                        //
                        // A first version copied `min(old_len, new_len)` bytes,
                        // reasoning that everything inside the usable length is
                        // readable. It is not: with a lazy commit the source's
                        // uncommitted granules are `PROT_NONE` / reserved-only,
                        // so the READ faults -- and it faults inside a
                        // `copy_nonoverlapping`, which is a bare
                        // `STATUS_ACCESS_VIOLATION` with the collector's own
                        // frames on the stack and no clue that a commit is what
                        // was missing. `large_array_survives_gc` found it.
                        //
                        // Granule by granule, skipping the uncommitted ones,
                        // which is exactly `fill_zero`'s shape and for the same
                        // reason: an uncommitted granule holds nothing, so
                        // there is nothing to move and the destination's own
                        // fresh granule already reads as zero.
                        let live = r.len.min(next.len);
                        r.copy_committed_to(&mut next, live);
                        *r = next;
                        r.base
                    }
                    // The OS refused a bigger reservation. Keep what we have --
                    // the caller reads the capacity back and will fail its own
                    // allocation rather than write past the end.
                    None => r.base,
                }
            }
        }
    }

    /// Is the granule holding `offset` committed, i.e. safe to READ?
    ///
    /// Lazy commit turns a speculative read of untouched heap space from
    /// "returns zero" into "faults", which is a new hazard for the handful of
    /// callers that deliberately look at an address the allocator has retracted
    /// past. Always `true` on the owned arm, where everything is committed.
    pub fn is_committed_at(&self, offset: usize) -> bool {
        match self {
            HeapStore::Owned(v) => offset < v.len(),
            HeapStore::Reserved(r) => offset < r.len && r.is_committed(offset / GRANULE),
        }
    }

    /// Hand `[offset, offset + len)` back to the OS, keeping the address space
    /// reserved.
    ///
    /// Only WHOLE granules inside the range are released; a partial granule at
    /// either end is left committed, because the bytes beside it are still in
    /// use. Decommitted memory reads as zero again when re-committed, which is
    /// the same contract a fresh granule has.
    ///
    /// **The caller must prove the range holds nothing live.** There is no
    /// check here and there cannot be one: this type knows about bytes, not
    /// objects.
    pub fn decommit_range(&mut self, offset: usize, len: usize) -> usize {
        match self {
            HeapStore::Owned(_) => 0,
            HeapStore::Reserved(r) => r.decommit_range(offset, len),
        }
    }
}

/// Reserved address space with per-granule commit tracking.
pub struct Reservation {
    base: *mut u8,
    /// The capacity the heap ASKED for. `-Xmx` must mean what it says, so this
    /// is what `HeapStore::len` reports and what every offset is bounded by --
    /// not [`Self::reserved_len`], which is rounded up and would let a heap
    /// report up to one granule more than it was given.
    len: usize,
    /// Reserved address space, rounded up to a whole number of granules.
    reserved_len: usize,
    /// One bit per granule; set means committed.
    granules: Vec<u64>,
    committed: usize,
}

// SAFETY: the same argument `Arena` already makes about its `Vec<u8>` -- the
// pointer is owned exclusively by this value, freed exactly once in `Drop`, and
// every access through it is serialised by the `Mutex<Arena>` that holds it.
unsafe impl Send for Reservation {}
// SAFETY: as above. `&Reservation` exposes no interior mutability.
unsafe impl Sync for Reservation {}

impl Reservation {
    fn reserve(capacity: usize) -> Option<Self> {
        let reserved_len = capacity.div_ceil(GRANULE) * GRANULE;
        let base = platform_reserve(reserved_len)?;
        let granule_count = reserved_len / GRANULE;
        Some(Self {
            base,
            len: capacity,
            reserved_len,
            granules: vec![0u64; granule_count.div_ceil(64)],
            committed: 0,
        })
    }

    /// Zero `[start, end)`, touching only the granules that are COMMITTED.
    ///
    /// An uncommitted granule already reads as zero and writing to it would
    /// fault, so skipping it is both the correct answer and the fast one -- a
    /// full-arena clear on a heap that has only ever used its first megabyte
    /// costs one `memset` of that megabyte rather than of `-Xmx`.
    fn fill_zero(&mut self, start: usize, end: usize) {
        let end = end.min(self.len);
        if end <= start {
            return;
        }
        let first = start / GRANULE;
        let last = (end - 1) / GRANULE;
        for g in first..=last {
            if !self.is_committed(g) {
                continue;
            }
            let lo = (g * GRANULE).max(start);
            let hi = ((g + 1) * GRANULE).min(end);
            // SAFETY: `[lo, hi)` lies inside a committed granule of this
            // reservation, so the bytes are mapped read-write.
            unsafe { std::ptr::write_bytes(self.base.add(lo), 0, hi - lo) };
        }
    }

    /// See [`HeapStore::committed_spans`].
    fn committed_spans(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let end = end.min(self.len);
        let mut out: Vec<(usize, usize)> = Vec::new();
        if end <= start {
            return out;
        }
        let first = start / GRANULE;
        let last = (end - 1) / GRANULE;
        for g in first..=last {
            if !self.is_committed(g) {
                continue;
            }
            let lo = (g * GRANULE).max(start);
            let hi = ((g + 1) * GRANULE).min(end);
            let addr = self.base as usize + lo;
            match out.last_mut() {
                Some((a, l)) if *a + *l == addr => *l += hi - lo,
                _ => out.push((addr, hi - lo)),
            }
        }
        out
    }

    fn committed_granules(&self) -> usize {
        self.committed
    }

    #[inline]
    fn is_committed(&self, g: usize) -> bool {
        self.granules
            .get(g >> 6)
            .is_some_and(|w| w & (1u64 << (g & 63)) != 0)
    }

    #[inline]
    fn set_committed(&mut self, g: usize, on: bool) {
        if let Some(w) = self.granules.get_mut(g >> 6) {
            if on {
                *w |= 1u64 << (g & 63);
            } else {
                *w &= !(1u64 << (g & 63));
            }
        }
    }

    /// Copy the COMMITTED granules of `[0, len)` into `dst`, committing each
    /// one there first. Uncommitted source granules are skipped: they hold
    /// nothing, and `dst`'s corresponding granules read as zero when they are
    /// eventually committed, which is the same thing.
    fn copy_committed_to(&self, dst: &mut Reservation, len: usize) {
        let len = len.min(self.len).min(dst.len);
        if len == 0 {
            return;
        }
        let last = (len - 1) / GRANULE;
        for g in 0..=last {
            if !self.is_committed(g) {
                continue;
            }
            let lo = g * GRANULE;
            let hi = ((g + 1) * GRANULE).min(len);
            if hi <= lo || !dst.commit_range(lo, hi - lo) {
                continue;
            }
            // SAFETY: `[lo, hi)` is committed in both -- in `self` by the test
            // above, in `dst` by the `commit_range` -- and the two reservations
            // are distinct mappings, so the ranges cannot overlap.
            unsafe { std::ptr::copy_nonoverlapping(self.base.add(lo), dst.base.add(lo), hi - lo) };
        }
    }

    fn commit_range(&mut self, offset: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let end = match offset.checked_add(len) {
            Some(e) if e <= self.reserved_len => e,
            // Out of the reservation entirely. Refusing is the only safe
            // answer: the caller is about to write there.
            _ => return false,
        };
        let first = offset / GRANULE;
        let last = (end - 1) / GRANULE;
        // ONE SYSCALL PER RUN, not per granule. A bump that crosses several
        // granules at once (a large array) is the common case for the high end.
        let mut run_start: Option<usize> = None;
        for g in first..=last {
            match (self.is_committed(g), run_start) {
                (false, None) => run_start = Some(g),
                (true, Some(s)) => {
                    if !self.commit_granules(s, g - s) {
                        return false;
                    }
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            if !self.commit_granules(s, last + 1 - s) {
                return false;
            }
        }
        true
    }

    fn commit_granules(&mut self, first: usize, count: usize) -> bool {
        if count == 0 {
            return true;
        }
        let bytes = count * GRANULE;
        // SAFETY: `first + count <= self.reserved_len / GRANULE` by the bounds
        // check, so the range is inside the reservation.
        let ok = unsafe { platform_commit(self.base.add(first * GRANULE), bytes) };
        if !ok {
            return false;
        }
        for g in first..first + count {
            if !self.is_committed(g) {
                self.set_committed(g, true);
                self.committed += 1;
            }
        }
        COMMITTED_BYTES.fetch_add(bytes, Ordering::Relaxed);
        true
    }

    fn decommit_range(&mut self, offset: usize, len: usize) -> usize {
        if len == 0 || offset >= self.reserved_len {
            return 0;
        }
        let end = offset.saturating_add(len).min(self.reserved_len);
        // WHOLE granules only, so the round is INWARD at both ends: a partial
        // granule at either edge has live bytes beside it.
        let first = offset.div_ceil(GRANULE);
        let last = end / GRANULE; // exclusive
        if last <= first {
            return 0;
        }
        let mut released = 0usize;
        let mut run_start: Option<usize> = None;
        for g in first..last {
            match (self.is_committed(g), run_start) {
                (true, None) => run_start = Some(g),
                (false, Some(s)) => {
                    released += self.decommit_granules(s, g - s);
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            released += self.decommit_granules(s, last - s);
        }
        released
    }

    fn decommit_granules(&mut self, first: usize, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        let bytes = count * GRANULE;
        // SAFETY: bounded by the caller, as `commit_granules`.
        if !unsafe { platform_decommit(self.base.add(first * GRANULE), bytes) } {
            // A REFUSED DECOMMIT IS NOT AN ERROR. The bytes stay committed and
            // usable; the only cost is that they stay resident. Reporting zero
            // keeps the accounting honest.
            return 0;
        }
        for g in first..first + count {
            if self.is_committed(g) {
                self.set_committed(g, false);
                self.committed -= 1;
            }
        }
        COMMITTED_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.committed != 0 {
            COMMITTED_BYTES.fetch_sub(self.committed * GRANULE, Ordering::Relaxed);
        }
        // SAFETY: `base`/`len` are exactly what `platform_reserve` returned and
        // this value owns them uniquely.
        unsafe { platform_release(self.base, self.reserved_len) };
    }
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------
//
// Declared directly rather than through `libc` / `windows-sys`, following
// `jit/src/platform.rs`, which does the same for the code cache: the gc crate
// has no platform dependency today and these are four functions with a stable
// ABI on each side.

#[cfg(target_os = "windows")]
const MEM_COMMIT: u32 = 0x1000;
#[cfg(target_os = "windows")]
const MEM_RESERVE: u32 = 0x2000;
#[cfg(target_os = "windows")]
const MEM_DECOMMIT: u32 = 0x4000;
#[cfg(target_os = "windows")]
const MEM_RELEASE: u32 = 0x8000;
#[cfg(target_os = "windows")]
const PAGE_READWRITE: u32 = 0x04;
#[cfg(target_os = "windows")]
const PAGE_NOACCESS: u32 = 0x01;

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn VirtualAlloc(
        lpAddress: *mut u8,
        dwSize: usize,
        flAllocationType: u32,
        flProtect: u32,
    ) -> *mut u8;
    fn VirtualFree(lpAddress: *mut u8, dwSize: usize, dwFreeType: u32) -> i32;
}

/// Reserve `len` bytes of address space with no commit charge.
#[cfg(target_os = "windows")]
fn platform_reserve(len: usize) -> Option<*mut u8> {
    // `PAGE_NOACCESS` because reserved-only pages have no protection of their
    // own; the commit below is what installs `PAGE_READWRITE`.
    let p = unsafe { VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_NOACCESS) };
    (!p.is_null()).then_some(p)
}

#[cfg(target_os = "windows")]
unsafe fn platform_commit(addr: *mut u8, len: usize) -> bool {
    // Committing an already-committed page is legal and a no-op, so the granule
    // bitmap is an optimisation rather than a correctness requirement here.
    !VirtualAlloc(addr, len, MEM_COMMIT, PAGE_READWRITE).is_null()
}

#[cfg(target_os = "windows")]
unsafe fn platform_decommit(addr: *mut u8, len: usize) -> bool {
    // MEM_DECOMMIT, never MEM_RELEASE: the address space must stay reserved or
    // another allocation could take it and the heap's base would no longer
    // describe a contiguous region.
    VirtualFree(addr, len, MEM_DECOMMIT) != 0
}

#[cfg(target_os = "windows")]
unsafe fn platform_release(addr: *mut u8, _len: usize) {
    // `MEM_RELEASE` requires a zero size and the original base.
    let _ = VirtualFree(addr, 0, MEM_RELEASE);
}

#[cfg(unix)]
const PROT_NONE: i32 = 0x0;
#[cfg(unix)]
const PROT_READ: i32 = 0x1;
#[cfg(unix)]
const PROT_WRITE: i32 = 0x2;
#[cfg(unix)]
const MAP_PRIVATE: i32 = 0x02;
#[cfg(all(unix, not(target_os = "macos")))]
const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(unix)]
const MAP_FAILED: *mut u8 = !0usize as *mut u8;
/// `MADV_DONTNEED` on Linux. Drops the pages; the next touch faults in a fresh
/// zero page, which is the contract [`HeapStore::decommit_range`] states.
#[cfg(all(unix, not(target_os = "macos")))]
const MADV_DONTNEED: i32 = 4;
/// macOS spells the same intent `MADV_FREE`.
#[cfg(target_os = "macos")]
const MADV_DONTNEED: i32 = 5;

#[cfg(unix)]
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// Reserve `len` bytes with `PROT_NONE`, which charges no commit against the
/// overcommit accounting on any Unix and faults nothing in.
#[cfg(unix)]
fn platform_reserve(len: usize) -> Option<*mut u8> {
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    (p != MAP_FAILED && !p.is_null()).then_some(p)
}

#[cfg(unix)]
unsafe fn platform_commit(addr: *mut u8, len: usize) -> bool {
    mprotect(addr, len, PROT_READ | PROT_WRITE) == 0
}

#[cfg(unix)]
unsafe fn platform_decommit(addr: *mut u8, len: usize) -> bool {
    // `madvise` FIRST, then drop the protection. The advise is what actually
    // returns the physical pages; the `mprotect` back to `PROT_NONE` is what
    // makes a write to a decommitted granule a fault rather than a silent
    // resurrection of a page the collector has written off -- the same
    // behaviour Windows gives for free after `MEM_DECOMMIT`.
    let advised = madvise(addr, len, MADV_DONTNEED) == 0;
    let protected = mprotect(addr, len, PROT_NONE) == 0;
    advised && protected
}

#[cfg(unix)]
unsafe fn platform_release(addr: *mut u8, len: usize) {
    let _ = munmap(addr, len);
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_reserve(_len: usize) -> Option<*mut u8> {
    None
}
#[cfg(not(any(unix, target_os = "windows")))]
unsafe fn platform_commit(_addr: *mut u8, _len: usize) -> bool {
    false
}
#[cfg(not(any(unix, target_os = "windows")))]
unsafe fn platform_decommit(_addr: *mut u8, _len: usize) -> bool {
    false
}
#[cfg(not(any(unix, target_os = "windows")))]
unsafe fn platform_release(_addr: *mut u8, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A reservation commits nothing until it is asked to.**
    ///
    /// The whole point on Windows, where the `alloc_zeroed` block this replaces
    /// takes the full `-Xmx` commit charge before the VM has run a bytecode.
    #[test]
    fn a_fresh_reservation_has_committed_nothing() {
        let Some(res) = Reservation::reserve(16 * GRANULE) else {
            return; // no reservation on this platform; the fallback covers it
        };
        assert_eq!(res.committed_granules(), 0);
        assert_eq!(res.len, 16 * GRANULE);
        assert_eq!(res.reserved_len, 16 * GRANULE);
    }

    /// **Committing is idempotent, and only the granules touched are charged.**
    #[test]
    fn committing_a_range_charges_only_the_granules_it_spans() {
        let Some(mut res) = Reservation::reserve(16 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 1));
        assert_eq!(res.committed_granules(), 1, "one byte must commit one granule");
        assert!(res.commit_range(0, 1));
        assert_eq!(res.committed_granules(), 1, "committing twice must not double-charge");
        // Spanning a boundary takes both.
        assert!(res.commit_range(GRANULE - 8, 16));
        assert_eq!(res.committed_granules(), 2);
    }

    /// **Committed memory is writable and reads as zero.**
    ///
    /// `Arena` inherited that contract from `alloc_zeroed` and depends on it:
    /// the bump path hands out memory without zeroing it, and only reused
    /// free-list blocks are explicitly cleared.
    #[test]
    fn committed_memory_is_zero_and_writable() {
        let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, GRANULE));
        // SAFETY: the range was just committed.
        unsafe {
            let p = res.base;
            for i in [0usize, 1, 4095, GRANULE - 1] {
                assert_eq!(*p.add(i), 0, "byte {i} of a fresh granule is not zero");
            }
            *p.add(1234) = 0xAB;
            assert_eq!(*p.add(1234), 0xAB);
        }
    }

    /// **Decommitting rounds INWARD, so a partial granule is never dropped.**
    ///
    /// The bytes beside a partial granule are still in use, and a decommit that
    /// rounded outward would take them with it -- silently, because the next
    /// read of a dropped page returns zero rather than faulting.
    #[test]
    fn decommit_releases_only_whole_granules_inside_the_range() {
        let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 8 * GRANULE));
        assert_eq!(res.committed_granules(), 8);
        // A range that starts and ends mid-granule: only granules 2..5 are
        // wholly inside it.
        let released = res.decommit_range(GRANULE + 16, 4 * GRANULE);
        if released == 0 {
            return; // platform declined; nothing to assert about the split
        }
        assert_eq!(released, 3 * GRANULE);
        assert_eq!(res.committed_granules(), 5);
        assert!(res.is_committed(1), "the partial leading granule must survive");
        assert!(res.is_committed(5), "the partial trailing granule must survive");
        assert!(!res.is_committed(2));
        assert!(!res.is_committed(4));
    }

    /// **A decommitted granule comes back zeroed.**
    ///
    /// The contract `decommit_range` states, and the one that makes reuse safe:
    /// a re-committed granule must look like a fresh one, not like whatever the
    /// dead objects left.
    #[test]
    fn a_recommitted_granule_reads_as_zero_again() {
        let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
            return;
        };
        assert!(res.commit_range(0, 2 * GRANULE));
        // SAFETY: committed above.
        unsafe { *res.base.add(GRANULE + 99) = 0xCD };
        if res.decommit_range(GRANULE, GRANULE) == 0 {
            return; // platform declined
        }
        assert!(res.commit_range(GRANULE, GRANULE));
        // SAFETY: re-committed above.
        assert_eq!(unsafe { *res.base.add(GRANULE + 99) }, 0);
    }

    /// **Committing past the end of the reservation is refused, not clamped.**
    ///
    /// The caller is about to WRITE there. A clamp would return `true` for a
    /// range it did not make writable.
    #[test]
    fn committing_out_of_bounds_is_refused() {
        let Some(mut res) = Reservation::reserve(2 * GRANULE) else {
            return;
        };
        assert!(!res.commit_range(0, 4 * GRANULE));
        assert!(!res.commit_range(2 * GRANULE, 1));
    }

    /// **The owned fallback behaves as the block it replaces.**
    #[test]
    fn the_owned_store_is_wholly_committed_and_ignores_decommit() {
        let mut store = HeapStore::Owned(vec![0u8; 4096]);
        assert_eq!(store.len(), 4096);
        assert_eq!(store.committed_bytes(), 4096);
        assert!(store.commit_range(0, 4096));
        assert_eq!(store.decommit_range(0, 4096), 0);
        assert_eq!(store.committed_bytes(), 4096);
    }
}
