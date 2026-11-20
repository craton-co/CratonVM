// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
const HEADER_PAGE: usize = PAGE;
/// An allocation aligned below this cannot hold a Rust pointer (every type with
/// a pointer field is 8-aligned), so its block is *pointer-free*: the boot
/// image's dangling-pointer scan skips it. shortcut: a `repr(packed)` type or a
/// byte buffer used as raw pointer storage would hide a pointer from the scan,
/// upgrade when one shows up in a census.
const PF_ALIGN: usize = 8;
/// Bytes a pointer-free small-block run is carved from the page allocator in.
const PF_CHUNK: usize = 16 * PAGE;
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
// Owner tracer (diagnosis only)
// ---------------------------------------------------------------------------
//
// `CRATONVM_BOOT_IMAGE_TRACE_ADDRS=0x..,0x..` (read at `arm`) names arena
// addresses (taken from a previous `census` run: the capture is deterministic
// without ASLR). The last arena allocation to cover each one keeps its call
// stack, which the pointer gate prints as raw return addresses (resolve them
// with `addr2line -f -C -e <binary>`: the launcher is `-no-pie`). Statics
// here change only when the variable is set; they are excluded from the data
// diff through `own_statics`.

const TRACE_MAX: usize = 64;
const TRACE_DEPTH: usize = 24;
static TRACE_N: AtomicUsize = AtomicUsize::new(0);
static TRACE_IN: AtomicU8 = AtomicU8::new(0);
static TRACE_ADDRS: [AtomicUsize; TRACE_MAX] = [const { AtomicUsize::new(0) }; TRACE_MAX];
static TRACE_FRAMES: [[AtomicUsize; TRACE_DEPTH]; TRACE_MAX] =
    [const { [const { AtomicUsize::new(0) }; TRACE_DEPTH] }; TRACE_MAX];

/// Parse the address list (no allocation: runs with the arena about to arm).
#[cfg(unix)]
fn trace_init() {
    // SAFETY: getenv with a NUL-terminated name; the result is read once.
    let v = unsafe { libc::getenv(c"CRATONVM_BOOT_IMAGE_TRACE_ADDRS".as_ptr()) };
    if v.is_null() {
        return;
    }
    let mut n = 0;
    let mut cur: Option<usize> = None;
    let mut i = 0;
    loop {
        // SAFETY: NUL-terminated C string.
        let c = unsafe { *v.add(i) } as u8;
        match c {
            b'0'..=b'9' | b'a'..=b'f' => {
                let d = (c as char).to_digit(16).unwrap_or(0) as usize;
                cur = Some(cur.unwrap_or(0) * 16 + d);
            }
            b'x' => {}
            _ => {
                if let Some(a) = cur.take() {
                    if n < TRACE_MAX {
                        TRACE_ADDRS[n].store(a, Ordering::Relaxed);
                        n += 1;
                    }
                }
                if c == 0 {
                    break;
                }
            }
        }
        i += 1;
    }
    // Load libgcc's unwinder now: its first call may allocate.
    let mut warm = [std::ptr::null_mut::<libc::c_void>(); 2];
    // SAFETY: a valid buffer of 2 entries.
    unsafe { libc::backtrace(warm.as_mut_ptr(), 2) };
    TRACE_N.store(n, Ordering::Release);
}

#[cfg(not(unix))]
fn trace_init() {}

#[cold]
#[cfg(unix)]
fn trace_hit(p: usize, len: usize) {
    for k in 0..TRACE_N.load(Ordering::Relaxed) {
        let a = TRACE_ADDRS[k].load(Ordering::Relaxed);
        if a < p || a >= p + len {
            continue;
        }
        // The unwinder may allocate (re-entering here): one tracer at a time.
        if TRACE_IN.swap(1, Ordering::Acquire) != 0 {
            return;
        }
        let mut buf = [std::ptr::null_mut::<libc::c_void>(); TRACE_DEPTH];
        // SAFETY: a valid buffer of TRACE_DEPTH entries.
        let n = unsafe { libc::backtrace(buf.as_mut_ptr(), TRACE_DEPTH as i32) }.max(0) as usize;
        for (d, slot) in TRACE_FRAMES[k].iter().enumerate() {
            slot.store(if d < n { buf[d] as usize } else { 0 }, Ordering::Relaxed);
        }
        TRACE_IN.store(0, Ordering::Release);
    }
}

#[cfg(not(unix))]
fn trace_hit(_: usize, _: usize) {}

/// Pointer-free pages of the process arena (see
/// [`ArenaHeap::pointer_free_ranges`]); empty when no arena exists.
pub fn pointer_free_ranges() -> Vec<std::ops::Range<usize>> {
    let st = STATE.load(Ordering::Acquire);
    if st == OFF {
        return Vec::new();
    }
    process_heap(st).pointer_free_ranges()
}

/// Free blocks and spans of the process arena (see [`ArenaHeap::free_ranges`]);
/// empty when no arena exists.
pub fn free_ranges() -> Vec<std::ops::Range<usize>> {
    let st = STATE.load(Ordering::Acquire);
    if st == OFF {
        return Vec::new();
    }
    process_heap(st).free_ranges()
}

/// `(address, return addresses)` of every traced address that got an owner.
pub fn trace_report() -> Vec<(usize, Vec<usize>)> {
    (0..TRACE_N.load(Ordering::Relaxed))
        .map(|k| {
            let f = TRACE_FRAMES[k]
                .iter()
                .map(|s| s.load(Ordering::Relaxed))
                .take_while(|&x| x != 0)
                .collect();
            (TRACE_ADDRS[k].load(Ordering::Relaxed), f)
        })
        .collect()
}

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
    /// Free lists and bump run of the pointer-free small blocks.
    pf_free: [usize; NUM_SMALL],
    pf_cur: usize,
    pf_end: usize,
    stats: Stats,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_PAGE);

/// Bytes at the arena base before the first block: the header page and the
/// pointer-free page bitmap (one bit per page of `capacity`).
const fn header_len(capacity: usize) -> usize {
    HEADER_PAGE + align_up(capacity / PAGE / 8 + 1, PAGE)
}

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
            capacity >= header_len(capacity) + PAGE && capacity % PAGE == 0,
            "arena too small"
        );
        let h = base as *mut Header;
        // SAFETY: the caller hands us the range; the header fits in its first
        // page (static assertion above).
        unsafe {
            std::ptr::write_bytes(base, 0, header_len(capacity));
            (*h).magic = MAGIC;
            (*h).version = VERSION;
            (*h).base = base as usize;
            (*h).capacity = capacity;
            (*h).bump = header_len(capacity);
            (*h).dirty = header_len(capacity);
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
            if (*h).bump < header_len(capacity) || (*h).bump > (*h).dirty || (*h).dirty > capacity {
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

    /// Address ranges of every free block and free page span, sorted. Their
    /// bytes are stale (a freed block keeps its old contents past the first
    /// word), so a scan for live pointers must skip them. Allocates the
    /// result: call it only when the arena is not armed.
    pub fn free_ranges(&self) -> Vec<std::ops::Range<usize>> {
        let h = self.hdr();
        let mut out = Vec::new();
        self.lock();
        // SAFETY: under the lock; a free block's first word is its list link,
        // a free span's first two words are `[next, len]`.
        unsafe {
            for list in [&(*h).small_free, &(*h).pf_free] {
                for (c, &head) in list.iter().enumerate() {
                    let mut p = head;
                    while p != 0 {
                        out.push(p..p + class_size(c));
                        p = *(p as *const usize);
                    }
                }
            }
            let mut s = (*h).large_free;
            while s != 0 {
                out.push(s..s + *((s + 8) as *const usize));
                s = *(s as *const usize);
            }
        }
        self.unlock();
        out.sort_by_key(|r| r.start);
        out
    }

    /// Address ranges of the pointer-free pages (blocks allocated with an
    /// alignment below [`PF_ALIGN`]), sorted and coalesced. Allocates.
    pub fn pointer_free_ranges(&self) -> Vec<std::ops::Range<usize>> {
        let mut out: Vec<std::ops::Range<usize>> = Vec::new();
        self.lock();
        let bm = (self.base + PAGE) as *const u8;
        for p in 0..self.capacity / PAGE {
            // SAFETY: under the lock; the bitmap covers every page of `capacity`.
            if unsafe { *bm.add(p / 8) } >> (p % 8) & 1 == 1 {
                let a = self.base + p * PAGE;
                match out.last_mut() {
                    Some(r) if r.end == a => r.end += PAGE,
                    _ => out.push(a..a + PAGE),
                }
            }
        }
        self.unlock();
        out
    }

    /// Under the lock: set or clear the pointer-free bit of every page of
    /// `[start, start + len)`.
    unsafe fn mark_pf(&self, start: usize, len: usize, on: bool) {
        let bm = (self.base + PAGE) as *mut u8;
        for p in (start - self.base) / PAGE..(start + len - self.base).div_ceil(PAGE) {
            // SAFETY: under the lock (caller); inside the bitmap.
            unsafe {
                if on {
                    *bm.add(p / 8) |= 1 << (p % 8);
                } else {
                    *bm.add(p / 8) &= !(1 << (p % 8));
                }
            }
        }
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
            self.mark_pf(addr, len, false);
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
    unsafe fn alloc_bucket(&self, b: Bucket, pf: bool) -> (*mut u8, bool) {
        let h = self.hdr();
        self.lock();
        // SAFETY: header fields under the lock; free-list words are inside
        // free blocks, which nobody else references.
        let out = unsafe {
            match b {
                Bucket::Small(c) if pf => self.alloc_pf_locked(c),
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
                Bucket::Large { len, align } => {
                    let r = self.alloc_large_locked(len, align);
                    if pf && !r.0.is_null() {
                        self.mark_pf(r.0 as usize, len, true);
                    }
                    r
                }
            }
        };
        if !out.0.is_null() {
            // SAFETY: under the lock.
            unsafe { self.add_live(b.bytes()) };
            self.stats_ref().allocs.fetch_add(1, Ordering::Relaxed);
        }
        self.unlock();
        if TRACE_N.load(Ordering::Relaxed) != 0 && self.base == ARENA_BASE && !out.0.is_null() {
            trace_hit(out.0 as usize, b.bytes());
        }
        // A reused block is cleared whole -- the link word, the bytes past the
        // request, and what the last owner left -- so the image holds no stale
        // word the pointer scan could mistake for a live pointer. Capture only:
        // the arena is armed in no other mode.
        if !out.0.is_null() && !out.1 {
            // SAFETY: the block is ours and `bytes()` long.
            unsafe { std::ptr::write_bytes(out.0, 0, b.bytes()) };
            return (out.0, true);
        }
        out
    }

    /// A pointer-free small block: its class's free list, else the next bytes
    /// of the pointer-free run (a [`PF_CHUNK`] taken from the page allocator).
    unsafe fn alloc_pf_locked(&self, c: usize) -> (*mut u8, bool) {
        let h = self.hdr();
        // SAFETY: under the lock (caller).
        unsafe {
            let head = (*h).pf_free[c];
            if head != 0 {
                (*h).pf_free[c] = *(head as *const usize);
                return (head as *mut u8, false);
            }
            let cs = class_size(c);
            let mut start = align_up((*h).pf_cur, natural_align(cs));
            if (*h).pf_cur == 0 || start + cs > (*h).pf_end {
                let (chunk, _) = self.alloc_large_locked(PF_CHUNK, PAGE);
                if chunk.is_null() {
                    return (std::ptr::null_mut(), false);
                }
                self.mark_pf(chunk as usize, PF_CHUNK, true);
                (*h).pf_end = chunk as usize + PF_CHUNK;
                start = chunk as usize;
            }
            (*h).pf_cur = start + cs;
            // A run's bytes are not tracked as fresh: the caller zeroes them.
            (start as *mut u8, false)
        }
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
            self.alloc_bucket(Bucket::of(layout.size(), layout.align()), layout.align() < PF_ALIGN)
                .0
        }
    }

    /// # Safety
    /// As [`GlobalAlloc::alloc_zeroed`].
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding; `alloc_bucket` hands out cleared memory.
        unsafe { self.alloc(layout) }
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
                    let list = if layout.align() < PF_ALIGN {
                        &mut (*h).pf_free
                    } else {
                        &mut (*h).small_free
                    };
                    *(p as *mut usize) = list[c];
                    list[c] = p as usize;
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
                    if align < PF_ALIGN {
                        self.mark_pf(addr + old, new - old, true);
                    }
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
    trace_init();
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
    if base != ARENA_BASE || len < HEADER_PAGE || len > ARENA_RESERVE || len % PAGE != 0 {
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
pub fn own_statics() -> [std::ops::Range<usize>; 7] {
    fn r<T>(s: &T) -> std::ops::Range<usize> {
        let a = s as *const T as usize;
        a..a + std::mem::size_of::<T>()
    }
    [
        r(&STATE),
        r(&RESTORED_LEN),
        r(&IGNORED_FREES),
        r(&TRACE_N),
        r(&TRACE_IN),
        r(&TRACE_ADDRS),
        r(&TRACE_FRAMES),
    ]
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
mod tests;
