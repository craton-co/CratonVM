// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 fast boot, WP1: the capture allocator.
//!
//! # Why
//!
//! A boot image (`cratonvm_types::image_layout`) snapshots the process at P1
//! and a restored run maps it back at the same addresses. Every heap block
//! allocated between the launcher prologue (P0) and P1 therefore has to live
//! at an address the restore run can map back, and the allocator that handed
//! it out must not keep any of its state anywhere else: mimalloc keeps its
//! thread heaps, segment maps and options in `.data`/`.bss` and TLS, so a
//! block it served P0..P1 would come back without the metadata that owns it,
//! and its statics would land in the image's `.data` diff.
//!
//! # What
//!
//! [`ImageArenaAlloc`] wraps the process allocator (on Linux the
//! `SparseBlockAlloc` -> mimalloc stack). The wrapper's whole state is one
//! byte, [`STATE`]:
//!
//! - `OFF` (every non-image run, and every non-Linux build): passthrough. The
//!   cost is one load and one predictable branch per call; `dealloc` does not
//!   even range-check.
//! - `ARMED` (capture, P0..P1, entered by [`arm`]): every allocation of every
//!   thread is served by an [`ArenaHeap`] at the fixed address
//!   [`ARENA_BASE`]. A free of an arena block goes back to the arena's free
//!   lists. A free of a block the inner allocator served before P0 is
//!   **leaked** (counted), and a `realloc` of one moves it into the arena and
//!   leaks the old copy, so the inner allocator is never entered.
//! - `DISARMED` (capture, after P1, entered by [`disarm`]): new allocations go
//!   to the inner allocator (the image writer's own buffers must not grow the
//!   arena it is dumping), and a free of an arena block is a no-op, so the
//!   arena bytes cannot change under the dump.
//! - `RESTORED` (entered by [`adopt_restored`] once the image loader mapped
//!   the arena): as `DISARMED`. A free of an arena block is a **no-op**, and a
//!   `realloc` of one allocates from the inner allocator and copies. Returning
//!   restored blocks to the in-arena free list would work (the allocator state
//!   is in the arena; the Linux child-process test proves it) but would dirty
//!   MAP_PRIVATE image pages for little gain; the no-op is the safe default.
//!
//! # The in-arena allocator
//!
//! [`ArenaHeap`] keeps **everything** -- the lock word, the bump offset, 40
//! size-class free lists, a first-fit list of free page spans, the counters --
//! in a [`Header`] in the arena's first page. A handle is just `(base,
//! capacity)`. Restoring the arena bytes at the same address therefore
//! restores the allocator; nothing in `.data`, `.bss` or TLS refers into it.
//!
//! - Small (<= 32 KiB after alignment): 40 classes (16..128 by 16, then four
//!   per power of two up to 32 KiB). A class's blocks are bump-allocated at
//!   the largest power of two dividing the class size (capped at a page), so a
//!   request with alignment `a` <= 4 KiB is served from a class whose blocks
//!   are all `a`-aligned (a power-of-two class when the size alone would pick
//!   a less aligned one). The class is a pure function of the `Layout`, so
//!   `dealloc` needs no block header.
//! - Large: whole pages, alignment up to anything. Address-ordered first fit
//!   on the span list (an unaligned prefix and the tail are freed back), else
//!   the bump pointer; the page-aligned gap an alignment skips becomes a free
//!   span. Freed spans coalesce with their neighbours, and a span that ends
//!   at the bump pointer lowers it, so `used_len` (the image length) shrinks
//!   back when the topmost blocks go. (Without coalescing, the 8-thread mixed
//!   test used 498 MB for an 11 MB peak.) A `realloc` shrinks in place, and
//!   grows in place when the block is the last one before the bump pointer
//!   (a growing `Vec`).
//! - Memory at or above the dirty mark (the bump pointer's high-water mark)
//!   was never written, hence zero (anonymous mapping), so `alloc_zeroed`
//!   there writes nothing: a big zeroed table costs image bytes only where it
//!   was written.
//! - The lock is a spin lock (`AtomicU32` in the header) that yields after a
//!   short spin. `realloc` copies outside it.
//!
//! When the arena is full the wrapper falls back to the inner allocator,
//! counts it in `inner_allocs_while_armed` and calls `image_layout::spoil`.
//! **The image writer must refuse to write an image when `spoiled()` answers
//! or that counter is not 0** -- such a block is not imaged.
//!
//! # Launcher protocol (Linux)
//!
//! 1. `image_layout::set_mode(Capture)`, then [`arm`] at P0 (reserves
//!    `[ARENA_BASE, +ARENA_RESERVE)` with `MAP_FIXED_NOREPLACE |
//!    MAP_NORESERVE`, initialises the header, flips to `ARMED`).
//! 2. At P1, [`disarm`], then [`captured_extent`] -> `(base, used_len)`
//!    (page-aligned) for the image writer, and [`census`] for the gate.
//! 3. A restored run: map the image's arena bytes MAP_PRIVATE at `base`,
//!    `image_layout::set_mode(Restored)`, then [`adopt_restored`]`(base,
//!    len)` **before** anything frees an imaged block (a free that reached
//!    mimalloc would corrupt it).

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// The arena's fixed virtual address: the image layout's
/// `CAPTURE_ARENA_BASE` (32 TiB: far below the mmap top-down area, far above
/// the `-no-pie` binary and brk heap, 8 TiB below the broker's regions).
pub const ARENA_BASE: usize = cratonvm_types::image_layout::CAPTURE_ARENA_BASE;
/// Address space reserved for the arena (`MAP_NORESERVE`: committed by first
/// touch, so only the used part costs anything).
pub const ARENA_RESERVE: usize = 4 << 30;

const PAGE: usize = 4096;
/// Bytes at the arena base that hold the [`Header`]; blocks start after it.
const HEADER_LEN: usize = PAGE;
const MAX_SMALL: usize = 32 * 1024;
const NUM_SMALL: usize = 40;
const MAGIC: u64 = u64::from_le_bytes(*b"CVMARENA");
const VERSION: u32 = 1;

const OFF: u8 = 0;
const ARMED: u8 = 1;
const DISARMED: u8 = 2;
const RESTORED: u8 = 3;

/// The wrapper's routing state; see the module docs. Changed only by
/// [`arm`], [`disarm`] and [`adopt_restored`].
static STATE: AtomicU8 = AtomicU8::new(OFF);
/// Arena length a restored run adopted: in `RESTORED`, only
/// `[ARENA_BASE, +RESTORED_LEN)` is the image's; the rest of the reserve may
/// be anyone's.
static RESTORED_LEN: AtomicUsize = AtomicUsize::new(0);
/// Frees (and the old half of reallocs) of arena blocks after the capture
/// window, which are no-ops. A static rather than a header field so that it
/// never writes the arena after P1 (it is a `.bss` word touched only after P1,
/// so it is not in the P0..P1 `.data` diff either).
static IGNORED_FREES: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The in-arena allocator
// ---------------------------------------------------------------------------

/// Counters, in the arena header (so a capture's census is imaged with it).
#[repr(C)]
struct Stats {
    allocs: AtomicU64,
    frees: AtomicU64,
    reallocs: AtomicU64,
    in_place_reallocs: AtomicU64,
    bytes_live: AtomicU64,
    bytes_peak: AtomicU64,
    inner_allocs_while_armed: AtomicU64,
    foreign_frees_while_armed: AtomicU64,
    foreign_bytes_leaked: AtomicU64,
}

/// The allocator's entire state, at the arena base. Mutable plain fields are
/// read and written only under `lock`, through raw-pointer places (never a
/// `&mut Header`, which would alias the atomics other threads read).
#[repr(C)]
struct Header {
    magic: u64,
    version: u32,
    lock: AtomicU32,
    /// The base this arena was initialised at; a mapping elsewhere is refused.
    base: usize,
    capacity: usize,
    /// Offset of the end of the used part. Lowered when the topmost block
    /// is freed, so it is the image's length.
    bump: usize,
    /// High-water mark of `bump`: bytes at or above it were never written,
    /// hence zero.
    dirty: usize,
    /// Address of the lowest free page span (`[next, len]` in its first two
    /// words), or 0. Address-ordered and coalesced.
    large_free: usize,
    /// One word for the owner (tests; an image writer's directory).
    root: usize,
    small_free: [usize; NUM_SMALL],
    stats: Stats,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_LEN);

/// Counters of an [`ArenaHeap`], as read by [`ArenaHeap::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    pub used_len: usize,
    pub allocs: u64,
    pub frees: u64,
    pub reallocs: u64,
    pub in_place_reallocs: u64,
    pub bytes_live: u64,
    pub bytes_peak: u64,
    pub inner_allocs_while_armed: u64,
    pub foreign_frees_while_armed: u64,
    pub foreign_bytes_leaked: u64,
}

#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// Largest power of two dividing `s`, capped at a page: the alignment every
/// block of a class of size `s` gets.
#[inline]
const fn natural_align(s: usize) -> usize {
    let a = s & s.wrapping_neg();
    if a > PAGE {
        PAGE
    } else {
        a
    }
}

/// Size class of `size` (1..=MAX_SMALL).
#[inline]
const fn class_of(size: usize) -> usize {
    if size <= 128 {
        return (size + 15) / 16 - if size == 0 { 0 } else { 1 };
    }
    let lg = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize; // >= 7
    8 + (lg - 7) * 4 + ((size - 1) >> (lg - 2)) - 4
}

/// Block size of class `c`.
#[inline]
const fn class_size(c: usize) -> usize {
    if c < 8 {
        return 16 * (c + 1);
    }
    let j = c - 8;
    let lg = 7 + j / 4;
    (1 << lg) + (j % 4 + 1) * (1 << (lg - 2))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bucket {
    Small(usize),
    Large { len: usize, align: usize },
}

impl Bucket {
    #[inline]
    fn of(size: usize, align: usize) -> Bucket {
        let size = size.max(1);
        if align <= PAGE {
            let eff = size.max(align);
            if eff <= MAX_SMALL {
                let mut c = class_of(eff);
                if natural_align(class_size(c)) < align {
                    c = class_of(eff.next_power_of_two());
                }
                return Bucket::Small(c);
            }
        }
        Bucket::Large {
            len: align_up(size, PAGE),
            align: align.max(PAGE),
        }
    }

    #[inline]
    fn bytes(self) -> usize {
        match self {
            Bucket::Small(c) => class_size(c),
            Bucket::Large { len, .. } => len,
        }
    }
}

/// A handle on an in-arena allocator: `(base, capacity)`, nothing else. All
/// state is in the [`Header`] at `base`.
#[derive(Clone, Copy, Debug)]
pub struct ArenaHeap {
    base: usize,
    capacity: usize,
}

impl ArenaHeap {
    /// A handle on the arena at `base` without touching it (the wrapper's
    /// routing handle; `owns` needs no header read).
    pub const fn at(base: usize, capacity: usize) -> Self {
        ArenaHeap { base, capacity }
    }

    /// Initialise a new arena over `[base, base + capacity)`.
    ///
    /// # Safety
    /// The range is readable, writable, page-aligned, owned by the caller for
    /// the arena's life, and reads as zero beyond the header page (an
    /// anonymous mapping, or zeroed memory).
    pub unsafe fn init(base: *mut u8, capacity: usize) -> Self {
        assert!(base as usize % PAGE == 0, "arena base must be page-aligned");
        assert!(
            capacity >= 2 * PAGE && capacity % PAGE == 0,
            "arena too small"
        );
        let h = base as *mut Header;
        // SAFETY: the caller hands us the range; the header fits in its first
        // page (static assertion above).
        unsafe {
            std::ptr::write_bytes(base, 0, HEADER_LEN);
            (*h).magic = MAGIC;
            (*h).version = VERSION;
            (*h).base = base as usize;
            (*h).capacity = capacity;
            (*h).bump = HEADER_LEN;
            (*h).dirty = HEADER_LEN;
        }
        ArenaHeap {
            base: base as usize,
            capacity,
        }
    }

    /// Attach to an arena someone initialised at `base` (in this process or,
    /// mapped back from its bytes, in an earlier one). `None` when the header
    /// is not an arena initialised at this very address.
    ///
    /// # Safety
    /// The header page at `base` is mapped and readable, and the rest of the
    /// arena up to its recorded capacity is mapped before it is allocated
    /// from.
    pub unsafe fn attach(base: *mut u8) -> Option<Self> {
        let h = base as *const Header;
        // SAFETY: the caller guarantees the header page is mapped.
        unsafe {
            if (*h).magic != MAGIC || (*h).version != VERSION || (*h).base != base as usize {
                return None;
            }
            let capacity = (*h).capacity;
            if (*h).bump < HEADER_LEN || (*h).bump > (*h).dirty || (*h).dirty > capacity {
                return None;
            }
            Some(ArenaHeap {
                base: base as usize,
                capacity,
            })
        }
    }

    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    /// `true` when `p` is inside this arena's range.
    #[inline]
    pub fn owns(&self, p: *const u8) -> bool {
        (p as usize).wrapping_sub(self.base) < self.capacity
    }

    #[inline]
    fn hdr(&self) -> *mut Header {
        self.base as *mut Header
    }

    #[inline]
    fn stats_ref(&self) -> &Stats {
        // SAFETY: an initialised or attached arena's header is mapped; the
        // counters are atomics, shared references to them are sound.
        unsafe { &(*self.hdr()).stats }
    }

    fn lock(&self) {
        // SAFETY: as `stats_ref`.
        let l = unsafe { &(*self.hdr()).lock };
        let mut spins = 0u32;
        while l
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while l.load(Ordering::Relaxed) != 0 {
                spins += 1;
                if spins < 128 {
                    std::hint::spin_loop();
                } else {
                    // sched_yield / SwitchToThread: no allocation.
                    std::thread::yield_now();
                    spins = 0;
                }
            }
        }
    }

    #[inline]
    fn unlock(&self) {
        // SAFETY: as `stats_ref`.
        unsafe { (*self.hdr()).lock.store(0, Ordering::Release) };
    }

    /// Page-rounded length of the used part, header included: the bytes an
    /// image has to carry.
    pub fn used_len(&self) -> usize {
        self.lock();
        // SAFETY: under the lock.
        let bump = unsafe { (*self.hdr()).bump };
        self.unlock();
        align_up(bump, PAGE)
    }

    /// The owner's word (see [`Header::root`]).
    pub fn root(&self) -> usize {
        self.lock();
        // SAFETY: under the lock.
        let r = unsafe { (*self.hdr()).root };
        self.unlock();
        r
    }

    pub fn set_root(&self, r: usize) {
        self.lock();
        // SAFETY: under the lock.
        unsafe { (*self.hdr()).root = r };
        self.unlock();
    }

    pub fn stats(&self) -> HeapStats {
        let s = self.stats_ref();
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        HeapStats {
            used_len: self.used_len(),
            allocs: r(&s.allocs),
            frees: r(&s.frees),
            reallocs: r(&s.reallocs),
            in_place_reallocs: r(&s.in_place_reallocs),
            bytes_live: r(&s.bytes_live),
            bytes_peak: r(&s.bytes_peak),
            inner_allocs_while_armed: r(&s.inner_allocs_while_armed),
            foreign_frees_while_armed: r(&s.foreign_frees_while_armed),
            foreign_bytes_leaked: r(&s.foreign_bytes_leaked),
        }
    }

    /// Under the lock: account `n` bytes going live.
    #[inline]
    unsafe fn add_live(&self, n: usize) {
        let s = self.stats_ref();
        let live = s.bytes_live.load(Ordering::Relaxed) + n as u64;
        s.bytes_live.store(live, Ordering::Relaxed);
        if live > s.bytes_peak.load(Ordering::Relaxed) {
            s.bytes_peak.store(live, Ordering::Relaxed);
        }
    }

    #[inline]
    unsafe fn sub_live(&self, n: usize) {
        let s = self.stats_ref();
        let live = s
            .bytes_live
            .load(Ordering::Relaxed)
            .saturating_sub(n as u64);
        s.bytes_live.store(live, Ordering::Relaxed);
    }

    /// Under the lock: free the page span `[addr, addr + len)`. Inserted in
    /// address order and merged with free neighbours; a span that ends at the
    /// bump pointer lowers it instead.
    unsafe fn free_span(&self, addr: usize, len: usize) {
        debug_assert!(addr % PAGE == 0 && len % PAGE == 0 && len > 0);
        let h = self.hdr();
        // SAFETY: the span is ours and at least a page long; list words are
        // in free spans.
        unsafe {
            let mut prev_link: *mut usize = std::ptr::null_mut();
            let mut link: *mut usize = std::ptr::addr_of_mut!((*h).large_free);
            while *link != 0 && *link < addr {
                prev_link = link;
                link = *link as *mut usize;
            }
            let mut len = len;
            let mut next = *link;
            if next != 0 && addr + len == next {
                len += *(next as *const usize).add(1);
                next = *(next as *const usize);
            }
            let (mut start, mut start_link) = (addr, link);
            if !prev_link.is_null() {
                let prev = *prev_link;
                let plen = *(prev as *const usize).add(1);
                if prev + plen == addr {
                    start = prev;
                    start_link = prev_link;
                    len += plen;
                }
            }
            if start + len == self.base + (*h).bump {
                (*h).bump = start - self.base;
                *start_link = next;
            } else {
                let w = start as *mut usize;
                *w = next;
                *w.add(1) = len;
                *start_link = start;
            }
        }
    }

    /// Under the lock: move the bump pointer to `end`; a block from `start`
    /// is fresh (zero) when it lies wholly above the dirty mark.
    #[inline]
    unsafe fn bump_to(&self, start: usize, end: usize) -> bool {
        let h = self.hdr();
        // SAFETY: under the lock (caller).
        unsafe {
            let fresh = start >= self.base + (*h).dirty;
            (*h).bump = end - self.base;
            if (*h).bump > (*h).dirty {
                (*h).dirty = (*h).bump;
            }
            fresh
        }
    }

    /// Allocate a block of bucket `b`. Returns the block and whether it is
    /// fresh (never used, hence zero). Null when the arena is full.
    unsafe fn alloc_bucket(&self, b: Bucket) -> (*mut u8, bool) {
        let h = self.hdr();
        self.lock();
        // SAFETY: header fields under the lock; free-list words are inside
        // free blocks, which nobody else references.
        let out = unsafe {
            match b {
                Bucket::Small(c) => {
                    let head = (*h).small_free[c];
                    if head != 0 {
                        (*h).small_free[c] = *(head as *const usize);
                        (head as *mut u8, false)
                    } else {
                        let cs = class_size(c);
                        let start = align_up(self.base + (*h).bump, natural_align(cs));
                        if start + cs > self.base + self.capacity {
                            (std::ptr::null_mut(), false)
                        } else {
                            let fresh = self.bump_to(start, start + cs);
                            (start as *mut u8, fresh)
                        }
                    }
                }
                Bucket::Large { len, align } => self.alloc_large_locked(len, align),
            }
        };
        if !out.0.is_null() {
            // SAFETY: under the lock.
            unsafe { self.add_live(b.bytes()) };
            self.stats_ref().allocs.fetch_add(1, Ordering::Relaxed);
        }
        self.unlock();
        out
    }

    unsafe fn alloc_large_locked(&self, len: usize, align: usize) -> (*mut u8, bool) {
        let h = self.hdr();
        // SAFETY: under the lock (caller); span words are in free spans.
        unsafe {
            // First fit on the span list.
            let mut link: *mut usize = std::ptr::addr_of_mut!((*h).large_free);
            while *link != 0 {
                let addr = *link;
                let w = addr as *mut usize;
                let next = *w;
                let span = *w.add(1);
                let start = align_up(addr, align);
                if start + len <= addr + span {
                    *link = next;
                    if start > addr {
                        self.free_span(addr, start - addr);
                    }
                    let tail = addr + span - (start + len);
                    if tail > 0 {
                        self.free_span(start + len, tail);
                    }
                    return (start as *mut u8, false);
                }
                link = w;
            }
            // The bump pointer.
            let top = align_up(self.base + (*h).bump, PAGE);
            let start = align_up(top, align);
            if start + len > self.base + self.capacity || start < top {
                return (std::ptr::null_mut(), false);
            }
            let fresh = self.bump_to(start, start + len);
            if start > top {
                self.free_span(top, start - top);
            }
            (start as *mut u8, fresh)
        }
    }

    /// # Safety
    /// As [`GlobalAlloc::alloc`].
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding.
        unsafe {
            self.alloc_bucket(Bucket::of(layout.size(), layout.align()))
                .0
        }
    }

    /// # Safety
    /// As [`GlobalAlloc::alloc_zeroed`].
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding; a reused block is ours to clear.
        unsafe {
            let (p, fresh) = self.alloc_bucket(Bucket::of(layout.size(), layout.align()));
            if !p.is_null() && !fresh {
                std::ptr::write_bytes(p, 0, layout.size());
            }
            p
        }
    }

    /// # Safety
    /// `p` was allocated from this arena with `layout`, and is not used again.
    pub unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let b = Bucket::of(layout.size(), layout.align());
        let h = self.hdr();
        self.lock();
        // SAFETY: under the lock; the block is ours again.
        unsafe {
            match b {
                Bucket::Small(c) => {
                    *(p as *mut usize) = (*h).small_free[c];
                    (*h).small_free[c] = p as usize;
                }
                Bucket::Large { len, .. } => self.free_span(p as usize, len),
            }
            self.sub_live(b.bytes());
        }
        self.stats_ref().frees.fetch_add(1, Ordering::Relaxed);
        self.unlock();
    }

    /// Null when the arena cannot hold `new_size`; the old block is intact.
    ///
    /// # Safety
    /// As [`GlobalAlloc::realloc`], for a block of this arena.
    pub unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let align = layout.align();
        let old_b = Bucket::of(layout.size(), align);
        let new_b = Bucket::of(new_size, align);
        let s = self.stats_ref();
        s.reallocs.fetch_add(1, Ordering::Relaxed);
        if old_b == new_b {
            s.in_place_reallocs.fetch_add(1, Ordering::Relaxed);
            return p;
        }
        if let (Bucket::Large { len: old, .. }, Bucket::Large { len: new, .. }) = (old_b, new_b) {
            let h = self.hdr();
            let addr = p as usize;
            self.lock();
            // SAFETY: under the lock; the tail is ours.
            let done = unsafe {
                if new < old {
                    self.free_span(addr + new, old - new);
                    self.sub_live(old - new);
                    true
                } else if addr + old == self.base + (*h).bump
                    && addr + new <= self.base + self.capacity
                {
                    self.bump_to(addr + old, addr + new);
                    self.add_live(new - old);
                    true
                } else {
                    false
                }
            };
            self.unlock();
            if done {
                s.in_place_reallocs.fetch_add(1, Ordering::Relaxed);
                return p;
            }
        }
        // SAFETY: the caller's contract; `new_size` with `align` is a valid
        // layout (GlobalAlloc::realloc's precondition).
        unsafe {
            let q = self.alloc(Layout::from_size_align_unchecked(new_size, align));
            if q.is_null() {
                return q;
            }
            std::ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
            self.dealloc(p, layout);
            q
        }
    }
}

// ---------------------------------------------------------------------------
// The global-allocator wrapper
// ---------------------------------------------------------------------------

/// The process allocator: `A` outside the capture window, the arena inside.
pub struct ImageArenaAlloc<A>(pub A);

/// The routing handle on the process arena.
#[inline]
fn process_heap(st: u8) -> ArenaHeap {
    let cap = if st == RESTORED {
        RESTORED_LEN.load(Ordering::Relaxed)
    } else {
        ARENA_RESERVE
    };
    ArenaHeap::at(ARENA_BASE, cap)
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for ImageArenaAlloc<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if STATE.load(Ordering::Acquire) != ARMED {
            // SAFETY: forwarding.
            return unsafe { self.0.alloc(layout) };
        }
        // SAFETY: armed: the arena is reserved and initialised.
        unsafe { armed_alloc(process_heap(ARMED), &self.0, layout, false) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if STATE.load(Ordering::Acquire) != ARMED {
            // SAFETY: forwarding.
            return unsafe { self.0.alloc_zeroed(layout) };
        }
        // SAFETY: as `alloc`.
        unsafe { armed_alloc(process_heap(ARMED), &self.0, layout, true) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let st = STATE.load(Ordering::Acquire);
        if st == OFF {
            // SAFETY: forwarding.
            return unsafe { self.0.dealloc(ptr, layout) };
        }
        // SAFETY: the caller's contract, routed by owner.
        unsafe { routed_dealloc(st, process_heap(st), &self.0, ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let st = STATE.load(Ordering::Acquire);
        if st == OFF {
            // SAFETY: forwarding.
            return unsafe { self.0.realloc(ptr, layout, new_size) };
        }
        // SAFETY: the caller's contract, routed by owner.
        unsafe { routed_realloc(st, process_heap(st), &self.0, ptr, layout, new_size) }
    }
}

/// An allocation while armed: the arena, or (arena full) the inner allocator,
/// counted so the image writer can refuse the image.
#[cold]
#[inline(never)]
unsafe fn armed_alloc<A: GlobalAlloc>(
    heap: ArenaHeap,
    inner: &A,
    layout: Layout,
    zeroed: bool,
) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe {
        let p = if zeroed {
            heap.alloc_zeroed(layout)
        } else {
            heap.alloc(layout)
        };
        if !p.is_null() {
            return p;
        }
        arena_overflow(heap);
        if zeroed {
            inner.alloc_zeroed(layout)
        } else {
            inner.alloc(layout)
        }
    }
}

/// The arena could not serve an armed allocation, which therefore goes to
/// the inner allocator and will not be imaged: count it and, for the process
/// arena, spoil the capture (`image_layout::spoil` is a `OnceLock::set`, which
/// does not allocate).
#[cold]
fn arena_overflow(heap: ArenaHeap) {
    heap.stats_ref()
        .inner_allocs_while_armed
        .fetch_add(1, Ordering::Relaxed);
    if heap.base() == ARENA_BASE {
        cratonvm_types::image_layout::spoil(
            "image_arena: the capture arena is full; an allocation went to the inner allocator",
        );
    }
}

#[cold]
#[inline(never)]
unsafe fn routed_dealloc<A: GlobalAlloc>(
    st: u8,
    heap: ArenaHeap,
    inner: &A,
    ptr: *mut u8,
    layout: Layout,
) {
    // SAFETY: the caller's contract.
    unsafe {
        if heap.owns(ptr) {
            if st == ARMED {
                heap.dealloc(ptr, layout);
            } else {
                IGNORED_FREES.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        if st == ARMED {
            // A pre-P0 block: freeing it would enter the inner allocator.
            let s = heap.stats_ref();
            s.foreign_frees_while_armed.fetch_add(1, Ordering::Relaxed);
            s.foreign_bytes_leaked
                .fetch_add(layout.size() as u64, Ordering::Relaxed);
            return;
        }
        inner.dealloc(ptr, layout)
    }
}

#[cold]
#[inline(never)]
unsafe fn routed_realloc<A: GlobalAlloc>(
    st: u8,
    heap: ArenaHeap,
    inner: &A,
    ptr: *mut u8,
    layout: Layout,
    new_size: usize,
) -> *mut u8 {
    // SAFETY: the caller's contract (GlobalAlloc::realloc: `new_size` with
    // `layout.align()` is a valid layout).
    unsafe {
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        let keep = layout.size().min(new_size);
        if heap.owns(ptr) {
            if st == ARMED {
                let q = heap.realloc(ptr, layout, new_size);
                if !q.is_null() {
                    return q;
                }
                arena_overflow(heap);
                let q = inner.alloc(new_layout);
                if !q.is_null() {
                    std::ptr::copy_nonoverlapping(ptr, q, keep);
                    heap.dealloc(ptr, layout);
                }
                return q;
            }
            // After the window: copy out, leave the imaged block alone.
            let q = inner.alloc(new_layout);
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(ptr, q, keep);
                IGNORED_FREES.fetch_add(1, Ordering::Relaxed);
            }
            return q;
        }
        if st == ARMED {
            // A pre-P0 block: move it into the arena, leak the old copy.
            let q = heap.alloc(new_layout);
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(ptr, q, keep);
                let s = heap.stats_ref();
                s.foreign_frees_while_armed.fetch_add(1, Ordering::Relaxed);
                s.foreign_bytes_leaked
                    .fetch_add(layout.size() as u64, Ordering::Relaxed);
                return q;
            }
            arena_overflow(heap);
        }
        inner.realloc(ptr, layout, new_size)
    }
}

// ---------------------------------------------------------------------------
// Launcher API
// ---------------------------------------------------------------------------

/// Why an arena transition was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArenaError {
    /// Not Linux (or not a 64-bit target): images are unsupported.
    Unsupported,
    /// `image_layout::mode()` is not the mode this call belongs to.
    WrongImageMode,
    /// The wrapper is not in a state this transition leaves from.
    WrongState,
    /// `mmap` of the reserve failed (errno), e.g. `EEXIST`: something already
    /// lives at [`ARENA_BASE`].
    Reserve(i32),
    /// The kernel placed the reserve elsewhere (pre-4.17 kernels ignore
    /// `MAP_FIXED_NOREPLACE`).
    Misplaced,
    /// The adopted range is not an arena captured at [`ARENA_BASE`].
    BadImage,
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArenaError::Unsupported => {
                f.write_str("boot image arena: unsupported on this platform")
            }
            ArenaError::WrongImageMode => f.write_str("boot image arena: wrong image_layout mode"),
            ArenaError::WrongState => f.write_str("boot image arena: wrong arena state"),
            ArenaError::Reserve(e) => write!(
                f,
                "boot image arena: reserve at {ARENA_BASE:#x} failed (errno {e})"
            ),
            ArenaError::Misplaced => {
                write!(f, "boot image arena: kernel did not honour {ARENA_BASE:#x}")
            }
            ArenaError::BadImage => {
                f.write_str("boot image arena: adopted range is not a captured arena")
            }
        }
    }
}

/// P0: reserve the arena and route every allocation of every thread into it.
/// Capture mode only; call once, from the launcher, before any thread that
/// allocates is started (threads already running are fine: they are routed
/// from their next call on).
pub fn arm() -> Result<(), ArenaError> {
    if cratonvm_types::image_layout::mode() != cratonvm_types::image_layout::ImageMode::Capture {
        return Err(ArenaError::WrongImageMode);
    }
    if STATE.load(Ordering::Acquire) != OFF {
        return Err(ArenaError::WrongState);
    }
    let base = sys::reserve_fixed(ARENA_BASE, ARENA_RESERVE)?;
    // SAFETY: a fresh anonymous read-write reserve of ARENA_RESERVE bytes.
    unsafe { ArenaHeap::init(base, ARENA_RESERVE) };
    STATE.store(ARMED, Ordering::Release);
    Ok(())
}

/// P1: stop routing into the arena. Later allocations go to the inner
/// allocator; frees of arena blocks become no-ops, so the arena bytes are
/// stable while the image writer reads them.
pub fn disarm() -> Result<(), ArenaError> {
    STATE
        .compare_exchange(ARMED, DISARMED, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| ArenaError::WrongState)
}

/// `(base, used_len)` of the captured arena, `used_len` page-aligned and
/// including the header page. `None` unless armed or disarmed (call it after
/// [`disarm`] for a stable answer).
pub fn captured_extent() -> Option<(usize, usize)> {
    let st = STATE.load(Ordering::Acquire);
    if st != ARMED && st != DISARMED {
        return None;
    }
    Some((ARENA_BASE, process_heap(st).used_len()))
}

/// A restored run: the image loader has mapped `[base, base + len)` from the
/// image. Validates the header and makes frees of blocks in that range
/// no-ops. Requires `image_layout::mode() == Restored`.
pub fn adopt_restored(base: usize, len: usize) -> Result<(), ArenaError> {
    if cratonvm_types::image_layout::mode() != cratonvm_types::image_layout::ImageMode::Restored {
        return Err(ArenaError::WrongImageMode);
    }
    if !sys::SUPPORTED {
        return Err(ArenaError::Unsupported);
    }
    if base != ARENA_BASE || len < HEADER_LEN || len > ARENA_RESERVE || len % PAGE != 0 {
        return Err(ArenaError::BadImage);
    }
    // SAFETY: the loader mapped at least the header page (len >= a page).
    let heap = unsafe { ArenaHeap::attach(base as *mut u8) }.ok_or(ArenaError::BadImage)?;
    if heap.used_len() > len {
        return Err(ArenaError::BadImage);
    }
    let st = STATE.load(Ordering::Acquire);
    if st != OFF && st != DISARMED {
        return Err(ArenaError::WrongState);
    }
    RESTORED_LEN.store(len, Ordering::Relaxed);
    STATE.store(RESTORED, Ordering::Release);
    Ok(())
}

/// Address ranges of this module's own statics (the routing state and its
/// companions). Phase 5 WP7 class `exclude`: they describe *this* process's
/// relation to the arena (`ARMED` in the capture at P1, `RESTORED` once a
/// restored run adopts it), so the image's data diff must never carry them.
pub fn own_statics() -> [std::ops::Range<usize>; 3] {
    fn r<T>(s: &T) -> std::ops::Range<usize> {
        let a = s as *const T as usize;
        a..a + std::mem::size_of::<T>()
    }
    [r(&STATE), r(&RESTORED_LEN), r(&IGNORED_FREES)]
}

/// The census of the process arena.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaCensus {
    /// `"off"`, `"armed"`, `"disarmed"` or `"restored"`.
    pub state: &'static str,
    /// The arena's counters (in a restored run: the capture run's, as imaged).
    pub heap: HeapStats,
    /// Frees of arena blocks after the capture window (no-ops).
    pub ignored_frees: u64,
}

/// Counters for the census: allocations served by the arena vs by the inner
/// allocator while armed (the second must be 0 for a valid image). `None`
/// when no arena exists (`OFF`).
pub fn census() -> Option<ArenaCensus> {
    let st = STATE.load(Ordering::Acquire);
    let state = match st {
        ARMED => "armed",
        DISARMED => "disarmed",
        RESTORED => "restored",
        _ => return None,
    };
    Some(ArenaCensus {
        state,
        heap: process_heap(st).stats(),
        ignored_frees: IGNORED_FREES.load(Ordering::Relaxed),
    })
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod sys {
    use super::ArenaError;
    use std::os::raw::{c_int, c_void};

    pub const SUPPORTED: bool = true;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 0x02;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MAP_NORESERVE: c_int = 0x4000;
    const MAP_FIXED_NOREPLACE: c_int = 0x10_0000;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    fn place(base: usize, len: usize, flags: c_int, fd: c_int) -> Result<*mut u8, ArenaError> {
        // SAFETY: MAP_FIXED_NOREPLACE never replaces an existing mapping.
        let p = unsafe {
            mmap(
                base as *mut c_void,
                len,
                PROT_READ | PROT_WRITE,
                flags | MAP_FIXED_NOREPLACE,
                fd,
                0,
            )
        };
        if p == MAP_FAILED {
            return Err(ArenaError::Reserve(
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            ));
        }
        if p as usize != base {
            // SAFETY: the mapping we just made, elsewhere.
            unsafe { munmap(p, len) };
            return Err(ArenaError::Misplaced);
        }
        Ok(p as *mut u8)
    }

    /// Reserve `[base, base + len)` read-write, lazily committed.
    pub fn reserve_fixed(base: usize, len: usize) -> Result<*mut u8, ArenaError> {
        place(base, len, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1)
    }

    /// Map `len` bytes of `fd` MAP_PRIVATE at `base` (what an image loader
    /// does with the arena section). Tests only for now.
    #[cfg(test)]
    pub fn map_file_fixed(base: usize, len: usize, fd: c_int) -> Result<*mut u8, ArenaError> {
        place(base, len, MAP_PRIVATE, fd)
    }

    /// # Safety
    /// The range is a mapping nothing references any more.
    #[cfg(test)]
    pub unsafe fn unmap(base: usize, len: usize) {
        // SAFETY: the caller's contract.
        unsafe { munmap(base as *mut c_void, len) };
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod sys {
    use super::ArenaError;

    pub const SUPPORTED: bool = false;

    pub fn reserve_fixed(_base: usize, _len: usize) -> Result<*mut u8, ArenaError> {
        Err(ArenaError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::System;

    /// A zeroed, page-aligned region from the system allocator to run an
    /// arena over.
    struct Region {
        ptr: *mut u8,
        layout: Layout,
    }

    impl Region {
        fn new(len: usize) -> Region {
            let layout = Layout::from_size_align(len, PAGE).unwrap();
            // SAFETY: non-zero size.
            let ptr = unsafe { System.alloc_zeroed(layout) };
            assert!(!ptr.is_null());
            Region { ptr, layout }
        }

        fn heap(&self) -> ArenaHeap {
            // SAFETY: a fresh zeroed region we own.
            unsafe { ArenaHeap::init(self.ptr, self.layout.size()) }
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { System.dealloc(self.ptr, self.layout) };
        }
    }

    unsafe impl Send for Region {}
    unsafe impl Sync for Region {}

    fn lay(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// Fill a block with a pattern derived from `seed`.
    unsafe fn fill(p: *mut u8, n: usize, seed: u64) {
        for i in 0..n {
            unsafe { *p.add(i) = (seed as usize).wrapping_mul(31).wrapping_add(i) as u8 };
        }
    }

    unsafe fn check(p: *const u8, n: usize, seed: u64) -> bool {
        (0..n)
            .all(|i| unsafe { *p.add(i) } == (seed as usize).wrapping_mul(31).wrapping_add(i) as u8)
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn size_classes_cover_every_small_size() {
        assert_eq!(class_size(NUM_SMALL - 1), MAX_SMALL);
        let mut prev = 0;
        for c in 0..NUM_SMALL {
            let s = class_size(c);
            assert!(s > prev && s % 16 == 0, "class {c} = {s}");
            assert_eq!(class_of(s), c, "class_of(class_size({c}))");
            prev = s;
        }
        for s in 1..=MAX_SMALL {
            let c = class_of(s);
            assert!(
                class_size(c) >= s && (c == 0 || class_size(c - 1) < s),
                "size {s} -> class {c}"
            );
        }
    }

    #[test]
    fn alloc_free_reuses_blocks() {
        let r = Region::new(16 << 20);
        let h = r.heap();
        unsafe {
            let mut v = Vec::new();
            for (i, s) in [1usize, 8, 16, 17, 100, 129, 1000, 4096, 20000, 32768]
                .iter()
                .enumerate()
            {
                let p = h.alloc(lay(*s, 8));
                assert!(h.owns(p) && p as usize >= r.ptr as usize + HEADER_LEN);
                fill(p, *s, i as u64);
                v.push((p, *s, i as u64));
            }
            for &(p, s, seed) in &v {
                assert!(check(p, s, seed), "block of {s} clobbered");
            }
            for &(p, s, _) in &v {
                h.dealloc(p, lay(s, 8));
            }
            // LIFO free lists: the same size comes back at the same address.
            for &(p, s, _) in v.iter().rev() {
                assert_eq!(h.alloc(lay(s, 8)), p, "size {s} not reused");
            }
            let st = h.stats();
            assert_eq!(st.allocs, 20);
            assert_eq!(st.frees, 10);
        }
    }

    #[test]
    fn alignment_is_honoured_small_and_large() {
        let r = Region::new(64 << 20);
        let h = r.heap();
        unsafe {
            for shift in 0..=21 {
                let align = 1usize << shift;
                for size in [1usize, 3, 16, 48, 100, 4000, 5000, 40000] {
                    let l = lay(size, align);
                    let p = h.alloc(l);
                    assert!(!p.is_null(), "size {size} align {align}");
                    assert_eq!(p as usize % align, 0, "size {size} align {align} -> {p:p}");
                    fill(p, size, 7);
                    assert!(check(p, size, 7));
                    if size % 2 == 0 {
                        h.dealloc(p, l);
                        // Freed then re-requested: still aligned.
                        let q = h.alloc(l);
                        assert_eq!(q as usize % align, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn realloc_preserves_contents_across_buckets() {
        let r = Region::new(64 << 20);
        let h = r.heap();
        unsafe {
            let mut l = lay(10, 8);
            let mut p = h.alloc(l);
            fill(p, 10, 3);
            // Same class: in place.
            assert_eq!(h.realloc(p, l, 16), p);
            l = lay(16, 8);
            let (mut size, mut seed) = (10, 3);
            for next in [100usize, 4000, 33000, 200_000, 1 << 20, 3 << 20] {
                let q = h.realloc(p, l, next);
                assert!(!q.is_null());
                assert!(check(q, size, seed), "{size} -> {next}");
                seed = 4;
                fill(q, next, seed);
                p = q;
                l = lay(next, 8);
                size = next;
            }
            // The last large block is at the bump top: grows in place.
            let q = h.realloc(p, l, 5 << 20);
            assert_eq!(q, p, "top-of-bump grow should be in place");
            assert!(check(q, 3 << 20, 4));
            l = lay(5 << 20, 8);
            // Shrink in place, then the freed tail is reusable.
            let q = h.realloc(p, l, 1 << 20);
            assert_eq!(q, p);
            l = lay(1 << 20, 8);
            assert!(check(q, 1 << 20, 4));
            let tail = h.alloc(lay(2 << 20, 8));
            assert!(
                tail as usize >= p as usize + (1 << 20) && (tail as usize) < p as usize + (5 << 20)
            );
            // Large -> small.
            let q = h.realloc(p, l, 50);
            assert!(check(q, 50, 4));
            assert!(h.stats().in_place_reallocs >= 3);
        }
    }

    #[test]
    fn large_blocks_reuse_spans_and_split() {
        let r = Region::new(64 << 20);
        let h = r.heap();
        unsafe {
            let a = h.alloc(lay(4 << 20, 8));
            let _guard = h.alloc(lay(64, 8));
            h.dealloc(a, lay(4 << 20, 8));
            // Fits in the freed 4 MiB span: first fit, at its start.
            let b = h.alloc(lay(1 << 20, 8));
            assert_eq!(b, a);
            // An aligned request carves from the same span past an unaligned prefix.
            let c = h.alloc(lay(1 << 20, 1 << 20));
            assert_eq!(c as usize % (1 << 20), 0);
            assert!(c as usize >= a as usize && (c as usize) < a as usize + (4 << 20));
            fill(b, 1 << 20, 1);
            fill(c, 1 << 20, 2);
            assert!(check(b, 1 << 20, 1) && check(c, 1 << 20, 2));
        }
    }

    #[test]
    fn alloc_zeroed_clears_reused_blocks_only() {
        let r = Region::new(16 << 20);
        let h = r.heap();
        unsafe {
            for size in [24usize, 3000, 100_000] {
                let l = lay(size, 8);
                let p = h.alloc(l);
                std::ptr::write_bytes(p, 0xAA, size);
                h.dealloc(p, l);
                let q = h.alloc_zeroed(l);
                assert_eq!(q, p);
                assert!((0..size).all(|i| *q.add(i) == 0), "size {size} not zeroed");
                let f = h.alloc_zeroed(l);
                assert!(
                    (0..size).all(|i| *f.add(i) == 0),
                    "fresh size {size} not zero"
                );
            }
        }
    }

    #[test]
    fn full_arena_returns_null() {
        let r = Region::new(64 * 1024);
        let h = r.heap();
        unsafe {
            let mut n = 0;
            while !h.alloc(lay(1000, 8)).is_null() {
                n += 1;
                assert!(n < 100);
            }
            assert!(n >= 50, "only {n} blocks of 1000 in 60 KiB");
            assert!(h.alloc(lay(1 << 20, 8)).is_null());
            assert!(h.used_len() <= 64 * 1024);
        }
    }

    #[test]
    fn concurrent_threads_never_share_a_block() {
        let r = Region::new(512 << 20);
        let h = r.heap();
        std::thread::scope(|s| {
            for t in 0..8u64 {
                s.spawn(move || {
                    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (t + 1));
                    let mut live: Vec<(usize, usize, usize, u64)> = Vec::new();
                    for i in 0..20_000u64 {
                        let x = rng.next();
                        if live.len() < 64 && (x % 3 != 0 || live.is_empty()) {
                            let size = match x % 10 {
                                0 => 33_000 + (x >> 8) as usize % 200_000,
                                1..=3 => 1 + (x >> 8) as usize % 4000,
                                _ => 1 + (x >> 8) as usize % 200,
                            };
                            let align = 1usize << ((x >> 40) % 8);
                            let seed = t << 32 | i;
                            unsafe {
                                let p = h.alloc(lay(size, align));
                                assert!(!p.is_null());
                                assert_eq!(p as usize % align, 0);
                                fill(p, size, seed);
                                live.push((p as usize, size, align, seed));
                            }
                        } else {
                            let k = (x >> 20) as usize % live.len();
                            let (p, size, align, seed) = live.swap_remove(k);
                            unsafe {
                                assert!(
                                    check(p as *const u8, size, seed),
                                    "thread {t}: block clobbered"
                                );
                                if x % 7 == 0 {
                                    let ns = size * 2 + 1;
                                    let q = h.realloc(p as *mut u8, lay(size, align), ns);
                                    assert!(check(q, size, seed), "thread {t}: realloc lost data");
                                    h.dealloc(q, lay(ns, align));
                                } else {
                                    h.dealloc(p as *mut u8, lay(size, align));
                                }
                            }
                        }
                    }
                    for (p, size, align, seed) in live {
                        unsafe {
                            assert!(check(p as *const u8, size, seed));
                            h.dealloc(p as *mut u8, lay(size, align));
                        }
                    }
                });
            }
        });
        let st = h.stats();
        assert!(st.allocs > 50_000, "{st:?}");
        assert_eq!(st.bytes_live, 0, "{st:?}");
        assert_eq!(st.allocs, st.frees, "{st:?}");
        // Coalescing and bump retraction: with everything freed, the large
        // spans have folded back; the used part stays a small multiple of
        // the peak (small blocks stay on their class lists).
        assert!(
            (st.used_len as u64) < 4 * st.bytes_peak,
            "fragmentation: {st:?}"
        );
    }

    #[test]
    fn attach_sees_the_same_state_and_refuses_garbage() {
        let r = Region::new(1 << 20);
        let h = r.heap();
        unsafe {
            let p = h.alloc(lay(100, 8));
            h.set_root(p as usize);
            let a = ArenaHeap::attach(r.ptr).expect("attach");
            assert_eq!(a.root(), p as usize);
            assert_eq!(a.stats(), h.stats());
            let q = a.alloc(lay(100, 8));
            assert_ne!(p, q);
            *(r.ptr as *mut u64) = 0;
            assert!(ArenaHeap::attach(r.ptr).is_none());
        }
    }

    /// The wrapper's routing, over a local arena and the system allocator.
    #[test]
    fn routing_by_state_and_owner() {
        let r = Region::new(1 << 20);
        let h = r.heap();
        let inner = System;
        unsafe {
            // Armed: allocations come from the arena.
            let a = armed_alloc(h, &inner, lay(64, 8), false);
            assert!(h.owns(a));
            // Armed: a pre-P0 block is leaked, never handed to the inner allocator.
            let foreign = inner.alloc(lay(32, 8));
            routed_dealloc(ARMED, h, &inner, foreign, lay(32, 8));
            assert_eq!(h.stats().foreign_frees_while_armed, 1);
            // Armed realloc of a pre-P0 block moves it into the arena.
            let foreign2 = inner.alloc(lay(16, 8));
            fill(foreign2, 16, 9);
            let moved = routed_realloc(ARMED, h, &inner, foreign2, lay(16, 8), 48);
            assert!(h.owns(moved) && check(moved, 16, 9));
            assert_eq!(h.stats().foreign_frees_while_armed, 2);
            // Armed, arena full: inner allocator, counted.
            assert_eq!(h.stats().inner_allocs_while_armed, 0);
            let big = armed_alloc(h, &inner, lay(4 << 20, 8), false);
            assert!(!big.is_null() && !h.owns(big));
            assert_eq!(h.stats().inner_allocs_while_armed, 1);
            inner.dealloc(big, lay(4 << 20, 8));
            // After the window: arena frees are no-ops, the arena is unchanged.
            fill(a, 64, 5);
            let before = h.stats();
            let ign = IGNORED_FREES.load(Ordering::Relaxed);
            routed_dealloc(RESTORED, h, &inner, a, lay(64, 8));
            assert_eq!(h.stats(), before);
            assert!(IGNORED_FREES.load(Ordering::Relaxed) > ign);
            assert!(check(a, 64, 5));
            // ... and a realloc copies out to the inner allocator.
            let out = routed_realloc(DISARMED, h, &inner, a, lay(64, 8), 200);
            assert!(!h.owns(out) && check(out, 64, 5));
            assert_eq!(h.stats(), before);
            // A non-arena block after the window goes to the inner allocator.
            routed_dealloc(RESTORED, h, &inner, out, lay(200, 8));
        }
    }

    #[test]
    fn transitions_refuse_outside_their_modes() {
        // image_layout is Off in this test process unless another test set it;
        // in Off, arm and adopt refuse without touching the state.
        if cratonvm_types::image_layout::mode() == cratonvm_types::image_layout::ImageMode::Off {
            assert_eq!(arm(), Err(ArenaError::WrongImageMode));
            assert_eq!(
                adopt_restored(ARENA_BASE, PAGE),
                Err(ArenaError::WrongImageMode)
            );
        }
        assert_eq!(disarm(), Err(ArenaError::WrongState));
        assert!(census().is_none());
        assert!(captured_extent().is_none());
    }

    /// Off-mode cost of the wrapper: an alloc/free loop through
    /// `ImageArenaAlloc<System>` against `System` directly. Run with
    /// `--ignored --nocapture` on an optimised build.
    #[test]
    #[ignore]
    fn off_mode_overhead_bench() {
        fn run<A: GlobalAlloc>(a: &A) -> std::time::Duration {
            let t = std::time::Instant::now();
            let mut keep: [*mut u8; 64] = [std::ptr::null_mut(); 64];
            for i in 0..4_000_000usize {
                let l = lay(16 + (i % 7) * 24, 8);
                let k = i % 64;
                unsafe {
                    if !keep[k].is_null() {
                        a.dealloc(keep[k], lay(16 + ((i - 64) % 7) * 24, 8));
                    }
                    keep[k] = std::hint::black_box(a.alloc(l));
                }
            }
            for (k, p) in keep.iter().enumerate() {
                let i = 4_000_000 - 64 + k;
                unsafe { a.dealloc(*p, lay(16 + (i % 7) * 24, 8)) };
            }
            t.elapsed()
        }
        let w = ImageArenaAlloc(System);
        let (mut best_s, mut best_w) = (u128::MAX, u128::MAX);
        for _ in 0..5 {
            best_s = best_s.min(run(&System).as_nanos());
            best_w = best_w.min(run(&w).as_nanos());
        }
        println!(
            "off-mode bench: System {} ms, ImageArenaAlloc<System> {} ms",
            best_s / 1_000_000,
            best_w / 1_000_000
        );
    }

    /// The "state lives in the arena" property: a parent builds an arena at a
    /// fixed address, records its live blocks in the arena itself, and writes
    /// the used bytes to a file; a child process maps that file MAP_PRIVATE at
    /// the same address (plus a fresh reserve for the rest), attaches, checks
    /// every block and keeps allocating, reallocating and freeing.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    mod linux_restore {
        use super::*;

        /// Away from ARENA_BASE so a test never collides with a real arena.
        const TEST_BASE: usize = 0x2400_0000_0000;
        const TEST_RESERVE: usize = 256 << 20;
        const ENV_FILE: &str = "CRATONVM_IMAGE_ARENA_TEST_CHILD_FILE";
        const OK_MARK: &str = "IMAGE_ARENA_CHILD_OK";

        /// One live block, as recorded in the arena.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Rec {
            ptr: usize,
            size: usize,
            align: usize,
            seed: u64,
        }
        const N: usize = 2000;

        #[test]
        fn restored_bytes_at_the_same_address_keep_allocating() {
            let base = sys::reserve_fixed(TEST_BASE, TEST_RESERVE).expect("reserve test arena");
            let h = unsafe { ArenaHeap::init(base, TEST_RESERVE) };
            let mut rng = Rng(0xC0FF_EE00_1234_5678);
            unsafe {
                let recs = h.alloc(lay(N * std::mem::size_of::<Rec>(), 8)) as *mut Rec;
                for i in 0..N {
                    let x = rng.next();
                    let size = if i % 50 == 0 {
                        40_000 + (x as usize % 300_000)
                    } else {
                        1 + (x as usize % 3000)
                    };
                    let align = 1usize << (x >> 50) % 7;
                    let p = h.alloc(lay(size, align));
                    fill(p, size, i as u64);
                    *recs.add(i) = Rec {
                        ptr: p as usize,
                        size,
                        align,
                        seed: i as u64,
                    };
                }
                // Free every third one so the free lists are populated at capture.
                for i in (0..N).step_by(3) {
                    let r = *recs.add(i);
                    h.dealloc(r.ptr as *mut u8, lay(r.size, r.align));
                    (*recs.add(i)).ptr = 0;
                }
                h.set_root(recs as usize);
            }
            let used = h.used_len();
            let dir = std::env::temp_dir().join(format!("cvm-image-arena-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("arena.bin");
            let bytes = unsafe { std::slice::from_raw_parts(base, used) };
            std::fs::write(&file, bytes).unwrap();
            unsafe { sys::unmap(TEST_BASE, TEST_RESERVE) };

            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "image_arena::tests::linux_restore::child_entry",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(ENV_FILE, &file)
                .output()
                .expect("spawn child");
            let _ = std::fs::remove_dir_all(&dir);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "child failed: {}\n{stdout}\n{stderr}",
                out.status
            );
            assert!(
                stdout.contains(OK_MARK),
                "child did not run its body:\n{stdout}\n{stderr}"
            );
        }

        /// The child half; a no-op unless the parent set the env variable.
        #[test]
        fn child_entry() {
            let Some(file) = std::env::var_os(ENV_FILE) else {
                return;
            };
            use std::os::unix::io::AsRawFd;
            let f = std::fs::File::open(&file).expect("open image");
            let len = f.metadata().unwrap().len() as usize;
            assert!(len % PAGE == 0 && len >= HEADER_LEN);
            sys::map_file_fixed(TEST_BASE, len, f.as_raw_fd()).expect("map image");
            drop(f);
            sys::reserve_fixed(TEST_BASE + len, TEST_RESERVE - len).expect("reserve tail");
            let h =
                unsafe { ArenaHeap::attach(TEST_BASE as *mut u8) }.expect("attach restored arena");
            assert_eq!(h.used_len(), len);
            let before = h.stats();
            let recs = h.root() as *mut Rec;
            assert!(h.owns(recs as *const u8));
            let mut rng = Rng(0x5EED);
            let mut verified = 0;
            unsafe {
                // Every live block survived.
                for i in 0..N {
                    let r = *recs.add(i);
                    if r.ptr != 0 {
                        assert!(
                            check(r.ptr as *const u8, r.size, r.seed),
                            "block {i} lost in restore"
                        );
                        verified += 1;
                    }
                }
                // Keep going: new blocks (from the restored free lists and the
                // restored bump pointer), reallocs and frees, then recheck all.
                for i in 0..N {
                    let r = *recs.add(i);
                    let x = rng.next();
                    let seed = (N + i) as u64;
                    if r.ptr == 0 {
                        let size = 1 + (x as usize % 5000);
                        let p = h.alloc(lay(size, 8));
                        assert!(h.owns(p));
                        fill(p, size, seed);
                        *recs.add(i) = Rec {
                            ptr: p as usize,
                            size,
                            align: 8,
                            seed,
                        };
                    } else if x % 4 == 0 {
                        let ns = r.size * 3 + 7;
                        let q = h.realloc(r.ptr as *mut u8, lay(r.size, r.align), ns);
                        assert!(check(q, r.size, r.seed));
                        fill(q, ns, seed);
                        *recs.add(i) = Rec {
                            ptr: q as usize,
                            size: ns,
                            align: r.align,
                            seed,
                        };
                    } else if x % 4 == 1 {
                        h.dealloc(r.ptr as *mut u8, lay(r.size, r.align));
                        (*recs.add(i)).ptr = 0;
                    }
                }
                for i in 0..N {
                    let r = *recs.add(i);
                    if r.ptr != 0 {
                        assert!(
                            check(r.ptr as *const u8, r.size, r.seed),
                            "block {i} clobbered after restore"
                        );
                    }
                }
            }
            let after = h.stats();
            assert!(after.allocs > before.allocs && after.frees > before.frees);
            assert!(after.used_len >= before.used_len);
            println!(
                "{OK_MARK} verified={verified} allocs={} frees={}",
                after.allocs, after.frees
            );
        }
    }
}
