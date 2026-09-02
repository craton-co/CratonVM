// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! F-16 — a heap that is RESERVED at `-Xmx` and COMMITTED on demand.
//!
//! # The problem this exists for
//!
//! `G1Collector::new` allocated its whole arena with `alloc_zeroed`, sized to
//! `-Xmx`, in the constructor. There was no `-Xms` (the flag was parsed and
//! discarded), no expansion, and no uncommit — so `-Xmx16g` charged 16 GiB
//! against the process at startup whether or not a byte of it was ever used. On
//! Windows that is 16 GiB of commit charge against the page file immediately.
//! It also forecloses G1's own footprint story, in which regions are committed
//! and uncommitted at region granularity.
//!
//! # The shape
//!
//! One contiguous RESERVATION of the full `-Xmx`, and a COMMITTED PREFIX inside
//! it that only ever grows on demand (and shrinks only under an explicit,
//! stop-the-world request).
//!
//! A prefix, rather than an arbitrary set of committed regions, is not a
//! simplification for its own sake — it is what keeps two existing contracts
//! true:
//!
//! * `gen_heap::publish_jit_read_bounds` tells the JIT "a raw load anywhere in
//!   `[base, end)` cannot fault", and compiled `getfield` fast paths test
//!   against it. That claim is only expressible as a RANGE, so the set of
//!   mapped bytes has to be one.
//! * every region base is `arena_base + i * region_size` and the address→region
//!   lookup is a shift (F-09). A reservation is contiguous address space, so
//!   that arithmetic is unchanged; only the mapping state of the pages differs.
//!
//! Committing "through" a region index therefore commits every region below it
//! too. In practice that costs almost nothing: the free-region search starts at
//! zero and advances a rotating hint, so claims run roughly in index order.
//!
//! # Falling back is a feature
//!
//! On any platform this module does not have a reservation implementation for —
//! and on any platform where the reservation call fails — it falls back to
//! exactly the previous behaviour: one fully-committed zeroed allocation. The
//! rest of the collector cannot tell the difference, because the fallback
//! reports its whole length as committed and refuses to shrink. That is what
//! makes this change safe to land without a platform matrix: the new path is an
//! optimisation on two targets and a no-op everywhere else.
//!
//! # Zeroing
//!
//! Freshly committed pages are zero-filled by the OS on both implemented
//! platforms, and a decommitted-then-recommitted page is zero again. So the
//! collector's zeroing contract is unaffected — and note it does not depend on
//! this anyway: `G1Region::bump_alloc` zeroes exactly the range it hands out,
//! which is the single establishment of that contract since `reset` stopped
//! scrubbing freed regions.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How the reservation is backed.
enum Backing {
    /// A real reservation: address space taken, pages committed on demand.
    /// The `usize` is the base address, kept for the platform release call.
    Reserved(usize),
    /// The pre-F-16 behaviour: one fully-committed allocation. Used when the
    /// platform has no implementation here, or when its reservation call fails.
    Committed(Vec<u8>),
}

/// A contiguous heap reservation with a committed prefix.
pub struct ReservedHeap {
    backing: Backing,
    base: usize,
    /// Total reserved bytes — the address range the collector's region table
    /// spans. Fixed for the life of the heap.
    reserved: usize,
    /// Bytes committed from `base`. Only this prefix may be touched.
    committed: AtomicUsize,
    /// Commit granularity: every commit and decommit is rounded out to a
    /// multiple of this.
    granularity: usize,
}

// SAFETY: the reservation is a plain address range with no interior mutability
// beyond the atomic prefix length; the collector serialises grow/shrink under
// its own regions lock, and reads of `committed` are atomic.
unsafe impl Send for ReservedHeap {}
unsafe impl Sync for ReservedHeap {}

impl ReservedHeap {
    /// Reserve `reserved` bytes and commit the first `initial_commit`.
    ///
    /// `reserved` is `-Xmx` rounded to the region grid by the caller;
    /// `initial_commit` is `-Xms`, clamped into `[granularity, reserved]`.
    ///
    /// Never fails: a refused reservation falls back to the fully-committed
    /// allocation, whose own failure is reported by `alloc_zeroed_heap` as a
    /// heap-reservation error exactly as before.
    pub fn new(reserved: usize, initial_commit: usize, region: &str) -> Self {
        if reserved == 0 {
            return Self {
                backing: Backing::Committed(Vec::new()),
                base: 0,
                reserved: 0,
                committed: AtomicUsize::new(0),
                granularity: 1,
            };
        }
        let granularity = commit_granularity();
        let initial = round_up(initial_commit.clamp(granularity, reserved), granularity).min(reserved);

        if let Some(base) = platform::reserve(reserved) {
            if platform::commit(base, initial) {
                return Self {
                    backing: Backing::Reserved(base),
                    base,
                    reserved,
                    committed: AtomicUsize::new(initial),
                    granularity,
                };
            }
            // Reserved but could not commit even the initial prefix: release
            // and take the fallback rather than hand back a heap whose first
            // page faults.
            platform::release(base, reserved);
        }

        let mut buf = crate::arena::alloc_zeroed_heap(reserved, region);
        let base = buf.as_mut_ptr() as usize;
        Self {
            backing: Backing::Committed(buf),
            base,
            reserved,
            committed: AtomicUsize::new(reserved),
            granularity,
        }
    }

    /// Base address of the reservation.
    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    /// Total reserved bytes. This is the span the region table covers, NOT the
    /// memory charged to the process.
    #[inline]
    pub fn reserved_len(&self) -> usize {
        self.reserved
    }

    /// Bytes currently committed, i.e. actually charged to the process and safe
    /// to touch.
    #[inline]
    pub fn committed_len(&self) -> usize {
        self.committed.load(Ordering::Acquire)
    }

    /// Is this heap one real reservation, or the fully-committed fallback?
    /// Diagnostics and tests only.
    #[inline]
    pub fn is_reserved(&self) -> bool {
        matches!(self.backing, Backing::Reserved(_))
    }

    /// Grow the committed prefix to at least `want` bytes.
    ///
    /// Returns `true` if `[base, want)` is committed afterwards — including
    /// when it already was, and always for the fully-committed fallback.
    /// Returns `false` only when the OS refused, which the caller must treat as
    /// "no more heap": the reservation still holds the address space, but the
    /// pages are not there.
    pub fn commit_to(&self, want: usize) -> bool {
        if want > self.reserved {
            return false;
        }
        let current = self.committed.load(Ordering::Acquire);
        if want <= current {
            return true;
        }
        match self.backing {
            // The fallback is committed in full at construction; there is
            // nothing to grow and `committed` already reports `reserved`.
            Backing::Committed(_) => true,
            Backing::Reserved(base) => {
                let target = round_up(want, self.granularity).min(self.reserved);
                if !platform::commit(base + current, target - current) {
                    return false;
                }
                // Release: the pages must be visible to any thread that
                // observes the new length, which is the whole point of
                // publishing it.
                self.committed.store(target, Ordering::Release);
                true
            }
        }
    }

    /// Shrink the committed prefix to `want` bytes, returning the pages above
    /// it to the OS.
    ///
    /// # Caller obligation
    ///
    /// Every byte in `[want, committed)` must be dead and unreachable: no live
    /// object, no region the collector will allocate into, no address any
    /// compiled frame or conservative root holds. This is only callable from a
    /// stop-the-world phase, and the caller must additionally have narrowed the
    /// published JIT read bounds FIRST — a compiled `getfield` fast path tests
    /// against those bounds and will happily issue a raw load into a range they
    /// still describe as mapped.
    ///
    /// Returns `true` if the prefix is now at most `want`. The fully-committed
    /// fallback returns `false` and changes nothing: it has no way to give
    /// pages back.
    pub fn decommit_to(&self, want: usize) -> bool {
        let current = self.committed.load(Ordering::Acquire);
        if want >= current {
            return true;
        }
        match self.backing {
            Backing::Committed(_) => false,
            Backing::Reserved(base) => {
                let target = round_up(want, self.granularity).min(current);
                if target >= current {
                    return true;
                }
                // Publish the shrink BEFORE unmapping, so no reader can observe
                // a length that covers pages already gone.
                self.committed.store(target, Ordering::Release);
                platform::decommit(base + target, current - target)
            }
        }
    }
}

impl Drop for ReservedHeap {
    fn drop(&mut self) {
        if let Backing::Reserved(base) = self.backing {
            platform::release(base, self.reserved);
        }
    }
}

#[inline]
fn round_up(value: usize, granularity: usize) -> usize {
    if granularity <= 1 {
        return value;
    }
    value.div_ceil(granularity) * granularity
}

/// Commit granularity for this platform, cached.
fn commit_granularity() -> usize {
    use std::sync::OnceLock;
    static G: OnceLock<usize> = OnceLock::new();
    *G.get_or_init(platform::granularity)
}

// ---------------------------------------------------------------------------
// Platform layer
// ---------------------------------------------------------------------------
//
// Declared as raw `extern` blocks rather than pulled in with `libc` /
// `windows-sys`. The `gc` crate's dependency list is deliberately four crates
// long (`types`, `jfr`, `parking_lot`, `tracing`, `rustc-hash`), the workspace
// carries an explicit note about keeping `windows-sys` multi-versioned rather
// than unifying it, and these six symbols are stable ABI on both platforms.
// Adding a dependency to buy them would be the larger change.

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;

    const MEM_COMMIT: u32 = 0x0000_1000;
    const MEM_RESERVE: u32 = 0x0000_2000;
    const MEM_DECOMMIT: u32 = 0x0000_4000;
    const MEM_RELEASE: u32 = 0x0000_8000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_NOACCESS: u32 = 0x01;

    extern "system" {
        fn VirtualAlloc(
            address: *mut c_void,
            size: usize,
            allocation_type: u32,
            protect: u32,
        ) -> *mut c_void;
        fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    }

    /// Windows commits at page granularity (4 KiB on every supported
    /// architecture) even though it RESERVES at 64 KiB. Rounding commits to
    /// 64 KiB would over-commit by up to 60 KiB per grow for no benefit.
    pub(super) fn granularity() -> usize {
        4096
    }

    pub(super) fn reserve(len: usize) -> Option<usize> {
        // SAFETY: a null base asks the OS to choose the address; `MEM_RESERVE`
        // with `PAGE_NOACCESS` takes address space without committing memory.
        let p = unsafe {
            VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_NOACCESS)
        };
        (!p.is_null()).then_some(p as usize)
    }

    pub(super) fn commit(addr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        // SAFETY: `[addr, addr+len)` lies inside a reservation this module
        // owns; `MEM_COMMIT` on reserved address space is the documented way to
        // back it, and the pages arrive zeroed.
        let p = unsafe { VirtualAlloc(addr as *mut c_void, len, MEM_COMMIT, PAGE_READWRITE) };
        !p.is_null()
    }

    pub(super) fn decommit(addr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        // SAFETY: same range ownership as `commit`. `MEM_DECOMMIT` returns the
        // physical pages while KEEPING the reservation, which is exactly the
        // property the committed-prefix design needs.
        unsafe { VirtualFree(addr as *mut c_void, len, MEM_DECOMMIT) != 0 }
    }

    pub(super) fn release(addr: usize, _len: usize) {
        // SAFETY: `MEM_RELEASE` requires a size of 0 and the exact base of the
        // original reservation, which is what is stored.
        unsafe {
            VirtualFree(addr as *mut c_void, 0, MEM_RELEASE);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::ffi::c_void;

    const PROT_NONE: i32 = 0x0;
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_NORESERVE: i32 = 0x4000;
    const MADV_DONTNEED: i32 = 4;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
        fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
        fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    }

    pub(super) fn granularity() -> usize {
        4096
    }

    pub(super) fn reserve(len: usize) -> Option<usize> {
        // SAFETY: an anonymous `PROT_NONE` mapping takes address space without
        // backing it. `MAP_NORESERVE` additionally asks the kernel not to
        // account it against the overcommit limit, which is the whole point:
        // reserving `-Xmx` must not charge `-Xmx`.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p as isize == -1 || p.is_null() {
            None
        } else {
            Some(p as usize)
        }
    }

    pub(super) fn commit(addr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        // SAFETY: the range lies inside a reservation this module owns.
        // Granting read/write to a `PROT_NONE` anonymous mapping is what backs
        // it with (zero-filled) pages on first touch.
        unsafe { mprotect(addr as *mut c_void, len, PROT_READ | PROT_WRITE) == 0 }
    }

    pub(super) fn decommit(addr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        // SAFETY: same range ownership. `MADV_DONTNEED` on an anonymous mapping
        // frees the pages and guarantees the next read sees zeroes;
        // `PROT_NONE` then makes an accidental touch a fault rather than a
        // silent re-fault into fresh zeroes, which is the behaviour a
        // use-after-decommit bug should have.
        unsafe {
            let dropped = madvise(addr as *mut c_void, len, MADV_DONTNEED) == 0;
            let protected = mprotect(addr as *mut c_void, len, PROT_NONE) == 0;
            dropped && protected
        }
    }

    pub(super) fn release(addr: usize, len: usize) {
        // SAFETY: the exact base and length of a mapping this module made.
        unsafe {
            munmap(addr as *mut c_void, len);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    //! No reservation implementation for this target: every entry point
    //! declines, and `ReservedHeap::new` takes the fully-committed fallback —
    //! i.e. exactly the pre-F-16 behaviour.

    pub(super) fn granularity() -> usize {
        4096
    }
    pub(super) fn reserve(_len: usize) -> Option<usize> {
        None
    }
    pub(super) fn commit(_addr: usize, _len: usize) -> bool {
        false
    }
    pub(super) fn decommit(_addr: usize, _len: usize) -> bool {
        false
    }
    pub(super) fn release(_addr: usize, _len: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    #[test]
    fn a_reservation_commits_only_what_was_asked_for() {
        let heap = ReservedHeap::new(64 * MIB, 4 * MIB, "test");
        assert_eq!(heap.reserved_len(), 64 * MIB);
        assert!(heap.base() != 0);
        if heap.is_reserved() {
            assert_eq!(
                heap.committed_len(),
                4 * MIB,
                "the whole point: -Xmx is address space, -Xms is memory"
            );
        } else {
            assert_eq!(
                heap.committed_len(),
                64 * MIB,
                "the fallback is the pre-F-16 behaviour and says so"
            );
        }
    }

    #[test]
    fn the_committed_prefix_is_writable_and_zeroed() {
        let heap = ReservedHeap::new(4 * MIB, 64 * 1024, "test");
        let n = heap.committed_len().min(64 * 1024);
        // SAFETY: `[base, committed)` is committed by construction.
        let slice = unsafe { std::slice::from_raw_parts_mut(heap.base() as *mut u8, n) };
        assert!(slice.iter().all(|&b| b == 0), "committed pages arrive zeroed");
        slice[0] = 0xAB;
        slice[n - 1] = 0xCD;
        assert_eq!((slice[0], slice[n - 1]), (0xAB, 0xCD));
    }

    #[test]
    fn growing_the_prefix_makes_the_new_bytes_usable() {
        let heap = ReservedHeap::new(4 * MIB, 64 * 1024, "test");
        let before = heap.committed_len();
        assert!(heap.commit_to(2 * MIB));
        assert!(heap.committed_len() >= 2 * MIB);
        assert!(heap.committed_len() >= before);

        // SAFETY: just committed.
        let slice = unsafe {
            std::slice::from_raw_parts_mut((heap.base() + before) as *mut u8, 2 * MIB - before)
        };
        assert!(slice.iter().all(|&b| b == 0));
        slice[0] = 0x5A;
        assert_eq!(slice[0], 0x5A);
    }

    #[test]
    fn growing_is_monotone_and_idempotent() {
        let heap = ReservedHeap::new(4 * MIB, MIB, "test");
        assert!(heap.commit_to(2 * MIB));
        let high = heap.committed_len();
        // A request BELOW the current prefix must not shrink it — the callers
        // are region claims arriving out of order, and a claim of a low index
        // must not un-map a higher one already in use.
        assert!(heap.commit_to(MIB));
        assert_eq!(heap.committed_len(), high);
    }

    #[test]
    fn a_request_past_the_reservation_is_refused_rather_than_clamped() {
        let heap = ReservedHeap::new(2 * MIB, MIB, "test");
        assert!(
            !heap.commit_to(4 * MIB),
            "the reservation is the hard limit -Xmx means; silently clamping \
             would report success for memory the caller does not have"
        );
    }

    #[test]
    fn decommitting_gives_pages_back_and_they_read_as_zero_again() {
        let heap = ReservedHeap::new(4 * MIB, 2 * MIB, "test");
        if !heap.is_reserved() {
            // The fallback cannot give pages back and must say so rather than
            // pretend.
            assert!(!heap.decommit_to(MIB));
            assert_eq!(heap.committed_len(), 4 * MIB);
            return;
        }
        // SAFETY: committed by construction.
        unsafe {
            std::ptr::write_bytes((heap.base() + 3 * MIB / 2) as *mut u8, 0xFF, 4096);
        }
        assert!(heap.decommit_to(MIB));
        assert_eq!(heap.committed_len(), MIB);

        // Re-commit and confirm the dirty bytes did not survive: a recycled
        // region must not hand an allocator someone else's data.
        assert!(heap.commit_to(2 * MIB));
        let slice = unsafe {
            std::slice::from_raw_parts((heap.base() + 3 * MIB / 2) as *const u8, 4096)
        };
        assert!(
            slice.iter().all(|&b| b == 0),
            "recommitted pages must be zero, not the bytes that were there"
        );
    }

    /// The reservation path must actually be TAKEN on a platform that has one.
    /// Every test above has a fallback arm, so a silent regression to the
    /// pre-F-16 behaviour would leave them all green.
    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn this_platform_really_reserves() {
        let heap = ReservedHeap::new(64 * MIB, MIB, "test");
        assert!(
            heap.is_reserved(),
            "windows and linux both have a reservation implementation; falling              back here means it stopped working and every other test in this              module quietly started covering the fallback instead"
        );
        assert!(heap.committed_len() < heap.reserved_len());
    }

    #[test]
    fn a_zero_sized_heap_is_inert_rather_than_a_panic() {
        let heap = ReservedHeap::new(0, 0, "test");
        assert_eq!(heap.reserved_len(), 0);
        assert_eq!(heap.committed_len(), 0);
        assert!(!heap.commit_to(1));
    }

    #[test]
    fn the_initial_commit_is_clamped_into_the_reservation() {
        // -Xms above -Xmx: take the reservation, not a refusal — the two are
        // separately specified and a user who oversizes one should get a heap.
        let heap = ReservedHeap::new(MIB, 64 * MIB, "test");
        assert_eq!(heap.committed_len(), MIB);
    }
}
