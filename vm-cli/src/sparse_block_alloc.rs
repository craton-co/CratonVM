// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z10/mt: big zero-initialised blocks bypass mimalloc's huge-page arena
//! (`CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP`, threshold
//! `CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB`).
//!
//! # Why
//!
//! With mimalloc's own `arena_eager_commit=2` (the launcher's default again
//! since z10, see `mimalloc_tuning`) the first 1 GiB arena is committed up
//! front and `madvise(MADV_HUGEPAGE)`d by mimalloc, so on a THP=`madvise` host
//! every 2 MiB of the arena that anything touches is faulted in whole. That is
//! the right trade for the dense small-object pages (z10 host: lazy commit's
//! extra ~35k 4 KiB first-touch faults cost +18 % CPU on class loading), and
//! the wrong one for a block that is large, zeroed and written sparsely: the
//! GC's object-start / mark / old-start bitmaps (`gc::heap_bitmap::HeapBitmap`,
//! 1/64 of `-Xmx` each, `alloc_zeroed`), `vec![0; n]` side tables, and the
//! like. Any such block that lands in the arena drags every 2 MiB it touches
//! into RSS, and a REUSED arena block is also `memset` in full by
//! `mi_zalloc_aligned`, which touches all of it.
//!
//! # What
//!
//! A `GlobalAlloc` around `mimalloc::MiMalloc`, installed by `main.rs` on Linux
//! only. An `alloc_zeroed` of at least the threshold (default 1 MiB, floor
//! 64 KiB, which is mimalloc's own singleton-page bound) with an alignment of
//! at most a page is served by its own anonymous private `mmap`, advised
//! `MADV_NOHUGEPAGE`: the kernel zero-fills it lazily, 4 KiB at a time, and
//! nothing is memset. `realloc` of such a block is `mremap(MREMAP_MAYMOVE)`
//! (no copy), a shrink below the threshold moves it back into mimalloc, and
//! `dealloc` is `munmap`. Mode `all` routes plain `alloc`s of that size too
//! (and moves a mimalloc block that a `realloc` grows past the threshold).
//!
//! **Ownership is decided by address, not by the mode or the size.** A block
//! is ours exactly when `mi_is_in_heap_region` says mimalloc does not own it;
//! mimalloc registers every page it hands out (arena or OS) in its page map,
//! because `mi_free` needs it. The size test in front of that lookup is only a
//! filter: every block of ours is at least the threshold, which is latched at
//! the first allocator call (before any block exists) and never changes.
//!
//! The switch is read with libc `getenv` at that first call (nothing here may
//! allocate), so it is environment-only, like `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT`.
//! It is a launcher-process allocator policy, not compatibility state:
//! `libcratonvm` and `cratonvm-embed` run on their host's allocator.
//!
//! # Default: off (counters only)
//!
//! Opt-in (`=1` / `zeroed`, or `all`). It changes the backing of every
//! collector's big zeroed blocks and of non-GC tables alike, and the round's
//! rule is that a shared change must leave GenGC and G1 alone; the ZGC
//! bitmaps, the GC-owned share, are taken by the ZGC-only
//! `CRATONVM_ZGC_SIDE_TABLE_NO_THP` (`gc::reservation::map_side_table`)
//! instead. What this module adds by default is the instrument.
//!
//! # Seeing it
//!
//! `CRATONVM_GC_STATS` prints `[cratonvm] mimalloc-sparse-blocks:` (see
//! [`summary_line`]). Its `big_*` counters are kept in EVERY mode, including
//! off: `big_live_peak_kb` bounds what a THP arena can have wasted on big
//! blocks (each wastes at most its own size plus two partial 2 MiB pages), so
//! the off arm's line already says how much of the huge-page surplus a
//! big-block lever can reach, and `big_zeroed_allocs` against the run's
//! length says whether such blocks churn (a mapping per churned block would
//! re-fault its pages every time).

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Default threshold, KiB.
pub(crate) const DEFAULT_MIN_KB: usize = 1024;
/// Lowest threshold accepted, KiB: below this mimalloc serves the request
/// from a shared small or medium page, and a mapping per block would cost a
/// VMA and two syscalls for a block that wastes little.
pub(crate) const FLOOR_MIN_KB: usize = 64;
/// The mapping granule. Linux x86_64 / aarch64-4K; a larger kernel page only
/// makes `mmap` round further, which `map_len` does not need to know.
pub(crate) const PAGE: usize = 4096;
/// Default mode when the switch is unset: off, counters only (see the
/// module docs).
pub(crate) const DEFAULT_MODE: SparseMode = SparseMode::Off;

/// The routing mode, latched at the first allocator call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SparseMode {
    /// Not latched yet (no allocator call so far), or the wrapper is not
    /// installed on this platform.
    Undecided = 0,
    /// Everything goes to mimalloc; only the `big_*` counters run.
    Off = 1,
    /// Big `alloc_zeroed` requests are mapped.
    Zeroed = 2,
    /// Big `alloc` and `alloc_zeroed` requests are mapped.
    All = 3,
    /// zs s9/rs2: like [`Self::Zeroed`], but only the first
    /// `CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT` (default 64) big zeroed
    /// blocks of the process are mapped: the boot's long-lived tables (shadow
    /// stacks, the class-id side tables, the jimage-sized maps). Later ones,
    /// which are the ones that churn (a collection's scratch), stay in
    /// mimalloc, so a GC-heavy row pays no mapping per cycle.
    Boot = 4,
}

impl SparseMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Off,
            2 => Self::Zeroed,
            3 => Self::All,
            4 => Self::Boot,
            _ => Self::Undecided,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Undecided => "not-installed",
            Self::Off => "off",
            Self::Zeroed => "zeroed",
            Self::All => "all",
            Self::Boot => "boot",
        }
    }
}

/// The switch's value to a mode: unset or empty → `default`; `0`, `off`,
/// `false`, `no` → off; `all` → all; `boot` → boot (zs s9/rs2); anything
/// else (`1`, `on`, `zeroed`) → zeroed. Any case, surrounding blanks ignored.
pub(crate) fn parse_mode(value: Option<&[u8]>, default: SparseMode) -> SparseMode {
    let Some(v) = value.map(<[u8]>::trim_ascii) else {
        return default;
    };
    if v.is_empty() {
        default
    } else if crate::mimalloc_tuning::switch_is_off(Some(v)) {
        SparseMode::Off
    } else if v.eq_ignore_ascii_case(b"all") {
        SparseMode::All
    } else if v.eq_ignore_ascii_case(b"boot") {
        SparseMode::Boot
    } else {
        SparseMode::Zeroed
    }
}

/// The threshold's value in KiB: a decimal number, clamped up to
/// [`FLOOR_MIN_KB`]; unset, empty or unparsable → [`DEFAULT_MIN_KB`]. Does not
/// allocate.
pub(crate) fn parse_min_kb(value: Option<&[u8]>) -> usize {
    let Some(v) = value.map(<[u8]>::trim_ascii).filter(|v| !v.is_empty()) else {
        return DEFAULT_MIN_KB;
    };
    let mut n: usize = 0;
    for &b in v {
        if !b.is_ascii_digit() {
            return DEFAULT_MIN_KB;
        }
        n = match n
            .checked_mul(10)
            .and_then(|n| n.checked_add((b - b'0') as usize))
        {
            Some(n) => n,
            None => return DEFAULT_MIN_KB,
        };
    }
    // An absurd threshold just means nothing is ever routed; keep it in range
    // of a byte count.
    n.clamp(FLOOR_MIN_KB, usize::MAX / 2048)
}

/// Is a NEW request of `size` / `align` (`zeroed` for `alloc_zeroed`) served
/// by a mapping under `mode` and a threshold of `min` bytes?
pub(crate) fn routes(
    mode: SparseMode,
    zeroed: bool,
    size: usize,
    align: usize,
    min: usize,
) -> bool {
    let wanted = match mode {
        SparseMode::Zeroed | SparseMode::Boot => zeroed,
        SparseMode::All => true,
        SparseMode::Off | SparseMode::Undecided => false,
    };
    wanted && size >= min && align <= PAGE
}

/// zr z11/rv9: may the mapping modes run on a mimalloc of `version`
/// (`mi_version()`, e.g. 332 for 3.3.2)? The ownership-by-address rule needs
/// a page map that covers the whole user address space: v3's does (sized
/// from the OS's virtual-address bits, and `mi_free` itself reads it). v2's
/// `mi_is_in_heap_region` reads the segment map, which stops at 48 TiB, so a
/// v2 segment the kernel places higher (Linux maps top-down from ~128 TiB)
/// would read as "not mimalloc's" and be `munmap`ped by `dealloc`. The
/// `mimalloc` crate's `v2` feature can be switched on by feature unification
/// anywhere in the graph, so the latch checks the linked library rather than
/// a cfg; on a v2 backend the switch reads as `off` (counters only).
pub(crate) fn backend_supports_mapping(version: i32) -> bool {
    version >= 300
}

/// The mapping length of a block of `size` bytes: whole pages.
pub(crate) fn map_len(size: usize) -> usize {
    size.div_ceil(PAGE).saturating_mul(PAGE)
}

static MODE: AtomicU8 = AtomicU8::new(SparseMode::Undecided as u8);
/// The threshold in bytes. 0 until latched; constant afterwards. The fast
/// paths gate on [`FAST_MIN`] (equal to it unless the debug census is on).
static MIN_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Allocations of at least the threshold, by layout (every mode).
static BIG_ALLOCS: AtomicUsize = AtomicUsize::new(0);
/// Of those, `alloc_zeroed` requests, and their bytes summed.
static BIG_ZEROED_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BIG_ZEROED_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Live bytes in blocks of at least the threshold, wherever they live, and
/// the peak of that.
static BIG_LIVE: AtomicUsize = AtomicUsize::new(0);
static BIG_PEAK: AtomicUsize = AtomicUsize::new(0);
/// Blocks served by a mapping, their live mapped bytes and the peak.
static MAPPED: AtomicUsize = AtomicUsize::new(0);
static MAPPED_LIVE: AtomicUsize = AtomicUsize::new(0);
static MAPPED_PEAK: AtomicUsize = AtomicUsize::new(0);
/// `mremap`s of a mapped block, and the requests a refused `mmap` / `mremap`
/// sent back to mimalloc.
static REMAPS: AtomicUsize = AtomicUsize::new(0);
static MAP_FAILED: AtomicUsize = AtomicUsize::new(0);

/// zs s9/rs2: default `CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT`.
pub(crate) const DEFAULT_BOOT_COUNT: usize = 64;
/// Mappings mode `boot` may still make (latched from the knob).
static BOOT_BUDGET: AtomicUsize = AtomicUsize::new(0);
/// Big zeroed requests mode `boot` sent to mimalloc after its budget ran out.
static BOOT_DECLINED: AtomicUsize = AtomicUsize::new(0);

/// Take one mapping from mode `boot`'s budget; `false` once it is spent.
pub(crate) fn take_boot_budget() -> bool {
    let took = BOOT_BUDGET
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok();
    if !took {
        BOOT_DECLINED.fetch_add(1, Ordering::Relaxed);
    }
    took
}

/// zt t2/fp: give back one mapping [`take_boot_budget`] took for a mapping
/// the kernel then refused.
pub(crate) fn refund_boot_budget() {
    BOOT_BUDGET.fetch_add(1, Ordering::AcqRel);
}

/// The budget knob: a decimal count; unset, empty or unparsable →
/// [`DEFAULT_BOOT_COUNT`]. Does not allocate.
pub(crate) fn parse_boot_count(value: Option<&[u8]>) -> usize {
    let Some(v) = value.map(<[u8]>::trim_ascii).filter(|v| !v.is_empty()) else {
        return DEFAULT_BOOT_COUNT;
    };
    let mut n: usize = 0;
    for &b in v {
        if !b.is_ascii_digit() {
            return DEFAULT_BOOT_COUNT;
        }
        n = match n
            .checked_mul(10)
            .and_then(|n| n.checked_add((b - b'0') as usize))
        {
            Some(n) => n,
            None => return DEFAULT_BOOT_COUNT,
        };
    }
    n
}

/// zs s6/rs: the fast-path gate. Every call whose size is below it goes
/// straight to mimalloc. It is 0 until the first call latches the
/// configuration, then the threshold, or 0 for good under the census
/// (`CRATONVM_DBG_ALLOC_CENSUS`), which needs to see every call.
static FAST_MIN: AtomicUsize = AtomicUsize::new(0);

/// zs s6/rs: `CRATONVM_DBG_ALLOC_CENSUS` latched on. Read only on the slow
/// paths, so the default arm pays nothing for it.
static CENSUS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The census's size buckets, by requested size: up to 64 B, 512 B, 8 KiB,
/// 64 KiB (mimalloc v3 serves up to 8 KiB from 64 KiB pages and up to
/// 64 KiB from 512 KiB pages), below 1 MiB, and 1 MiB or more.
pub(crate) const CENSUS_BUCKETS: [(&str, usize); 6] = [
    ("le64", 64),
    ("le512", 512),
    ("le8k", 8 << 10),
    ("le64k", 64 << 10),
    ("lt1m", (1 << 20) - 1),
    ("ge1m", usize::MAX),
];

/// The bucket of a request of `size` bytes.
pub(crate) fn census_bucket(size: usize) -> usize {
    CENSUS_BUCKETS
        .iter()
        .position(|&(_, hi)| size <= hi)
        .unwrap_or(CENSUS_BUCKETS.len() - 1)
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicUsize = AtomicUsize::new(0);
/// Live requested bytes per bucket, the live total and its peak, and the
/// allocation count (every `alloc` and `alloc_zeroed`).
static CENSUS_LIVE: [AtomicUsize; 6] = [ZERO; 6];
static CENSUS_LIVE_TOTAL: AtomicUsize = AtomicUsize::new(0);
static CENSUS_PEAK_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Each bucket's live bytes when the total last peaked.
static CENSUS_AT_PEAK: [AtomicUsize; 6] = [ZERO; 6];
static CENSUS_ALLOCS: AtomicUsize = AtomicUsize::new(0);

/// Account a block that went from `old` to `new` requested bytes (0 = no
/// block): an allocation is `(0, n)`, a free `(n, 0)`, a realloc `(a, b)`.
/// Only called while the census is on.
pub(crate) fn census_move(old: usize, new: usize) {
    if old > 0 {
        CENSUS_LIVE[census_bucket(old)].fetch_sub(old, Ordering::Relaxed);
        CENSUS_LIVE_TOTAL.fetch_sub(old, Ordering::Relaxed);
    }
    if new > 0 {
        if old == 0 {
            CENSUS_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        CENSUS_LIVE[census_bucket(new)].fetch_add(new, Ordering::Relaxed);
        let now = CENSUS_LIVE_TOTAL
            .fetch_add(new, Ordering::Relaxed)
            .wrapping_add(new);
        if now > CENSUS_PEAK_TOTAL.fetch_max(now, Ordering::Relaxed) {
            // A new peak: snapshot the buckets. Racy across threads by
            // design (a debug census); each bucket is a recent value.
            for (at, live) in CENSUS_AT_PEAK.iter().zip(CENSUS_LIVE.iter()) {
                at.store(live.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
    }
}

/// The `[cratonvm] alloc-census:` line, or `None` when the census is off.
/// KiB of REQUESTED bytes (what Rust asked for, before mimalloc's size-class
/// rounding): set beside the `alloc:mimalloc` class of the RSS report
/// (`rss_report`), it says how much of the arena's resident set is live data
/// and how much is page-level sparsity.
pub(crate) fn census_line() -> Option<String> {
    if !CENSUS.load(Ordering::Relaxed) {
        return None;
    }
    let kb = |a: &AtomicUsize| a.load(Ordering::Relaxed) / 1024;
    let mut line = format!(
        "[cratonvm] alloc-census: allocs={} live_kb={} peak_kb={}",
        CENSUS_ALLOCS.load(Ordering::Relaxed),
        kb(&CENSUS_LIVE_TOTAL),
        kb(&CENSUS_PEAK_TOTAL),
    );
    for (i, (name, _)) in CENSUS_BUCKETS.iter().enumerate() {
        line.push_str(&format!(
            " live_{name}_kb={} peak_{name}_kb={}",
            kb(&CENSUS_LIVE[i]),
            kb(&CENSUS_AT_PEAK[i])
        ));
    }
    Some(line)
}

fn add_live(live: &AtomicUsize, peak: &AtomicUsize, bytes: usize) {
    let now = live.fetch_add(bytes, Ordering::Relaxed).wrapping_add(bytes);
    peak.fetch_max(now, Ordering::Relaxed);
}

fn sub_live(live: &AtomicUsize, bytes: usize) {
    live.fetch_sub(bytes, Ordering::Relaxed);
}

/// The `[cratonvm] mimalloc-sparse-blocks:` line of the `CRATONVM_GC_STATS`
/// summary. Sizes in KiB.
pub(crate) fn summary_line() -> String {
    let kb = |a: &AtomicUsize| a.load(Ordering::Relaxed) / 1024;
    let n = |a: &AtomicUsize| a.load(Ordering::Relaxed);
    format!(
        "[cratonvm] mimalloc-sparse-blocks: mode={} min_kb={} big_allocs={} big_zeroed_allocs={} \
         big_zeroed_kb={} big_live_kb={} big_live_peak_kb={} mapped={} mapped_live_kb={} \
         mapped_peak_kb={} remaps={} map_failed={} boot_budget_left={} boot_declined={}",
        SparseMode::from_u8(MODE.load(Ordering::Relaxed)).name(),
        MIN_BYTES.load(Ordering::Relaxed) / 1024,
        n(&BIG_ALLOCS),
        n(&BIG_ZEROED_ALLOCS),
        kb(&BIG_ZEROED_BYTES),
        kb(&BIG_LIVE),
        kb(&BIG_PEAK),
        n(&MAPPED),
        kb(&MAPPED_LIVE),
        kb(&MAPPED_PEAK),
        n(&REMAPS),
        n(&MAP_FAILED),
        n(&BOOT_BUDGET),
        n(&BOOT_DECLINED),
    )
}

#[cfg(all(feature = "mimalloc", target_os = "linux"))]
pub(crate) use imp::SparseBlockAlloc;

#[cfg(all(feature = "mimalloc", target_os = "linux"))]
mod imp {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout};
    use std::os::raw::{c_char, c_int, c_void};

    // zt t3/fp (t2/rv FP-2): the MAP_* / MADV_* values below are the x86_64 /
    // aarch64 (asm-generic) ones; refuse a build for an arch whose values differ.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("MAP_*/MADV_* constants are the x86_64/aarch64 values");

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 0x02;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MREMAP_MAYMOVE: c_int = 1;
    const MADV_NOHUGEPAGE: c_int = 15;
    const MAP_FAILED_PTR: *mut c_void = !0usize as *mut c_void;

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
        fn mremap(
            old: *mut c_void,
            old_len: usize,
            new_len: usize,
            flags: c_int,
            ...
        ) -> *mut c_void;
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
        fn getenv(name: *const c_char) -> *mut c_char;
        fn mi_is_in_heap_region(p: *const c_void) -> bool;
        fn mi_version() -> c_int;
    }

    /// `getenv` as bytes, without allocating.
    fn env(name: &'static [u8]) -> Option<&'static [u8]> {
        // SAFETY: `name` is NUL-terminated (both callers pass a `concat!(..,
        // "\0")` literal). The first allocator call runs before `main` or at
        // its very start, before anything can mutate the environment.
        let p = unsafe { getenv(name.as_ptr() as *const c_char) };
        if p.is_null() {
            None
        } else {
            // SAFETY: `getenv` returned a NUL-terminated string that lives as
            // long as the environment is not modified.
            Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes())
        }
    }

    /// The latched `(mode, threshold)`; latches on first use.
    #[inline]
    fn config() -> (SparseMode, usize) {
        let m = MODE.load(Ordering::Acquire);
        if m != SparseMode::Undecided as u8 {
            return (SparseMode::from_u8(m), MIN_BYTES.load(Ordering::Relaxed));
        }
        latch()
    }

    #[cold]
    fn latch() -> (SparseMode, usize) {
        // `concat!` keeps each name a plain string literal, which is what the
        // flag-declaration guard's reader scan looks for.
        let mode = parse_mode(
            env(concat!("CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP", "\0").as_bytes()),
            DEFAULT_MODE,
        );
        // zr z11/rv9: see `backend_supports_mapping`. `mi_version` returns a
        // compile-time constant: no allocation, no initialisation.
        // SAFETY: a plain FFI call with no arguments.
        let mode = if mode != SparseMode::Off && !backend_supports_mapping(unsafe { mi_version() })
        {
            SparseMode::Off
        } else {
            mode
        };
        let min = parse_min_kb(env(
            concat!("CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB", "\0").as_bytes()
        )) * 1024;
        // zs s9/rs2: mode `boot`'s mapping budget.
        if mode == SparseMode::Boot {
            BOOT_BUDGET.store(
                parse_boot_count(env(concat!(
                    "CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT",
                    "\0"
                )
                .as_bytes())),
                Ordering::Relaxed,
            );
        }
        // zs s6/rs: the debug census sees every call, so it keeps the fast
        // gate at 0 (every call takes the slow path).
        let census = parse_mode(
            env(concat!("CRATONVM_DBG_ALLOC_CENSUS", "\0").as_bytes()),
            SparseMode::Off,
        ) != SparseMode::Off;
        if census {
            CENSUS.store(true, Ordering::Relaxed);
        }
        // Concurrent first calls read the same environment and store the same
        // values; the CAS only makes the mode's publication single.
        MIN_BYTES.store(min, Ordering::Relaxed);
        FAST_MIN.store(if census { 0 } else { min }, Ordering::Relaxed);
        let _ = MODE.compare_exchange(
            SparseMode::Undecided as u8,
            mode as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        (
            SparseMode::from_u8(MODE.load(Ordering::Acquire)),
            MIN_BYTES.load(Ordering::Relaxed),
        )
    }

    /// A fresh zero-filled mapping of `size` bytes, advised against huge
    /// pages; null when the kernel refuses.
    unsafe fn map_block(size: usize) -> *mut u8 {
        let len = map_len(size);
        // SAFETY: an anonymous private mapping at a kernel-chosen address
        // aliases nothing.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == MAP_FAILED_PTR || p.is_null() {
            MAP_FAILED.fetch_add(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        // Advice only: under THP `madvise` a fresh mapping gets no huge pages
        // anyway, and under `always` this is what keeps it 4 KiB. A refusal
        // leaves a correct, merely huge-page-eligible block.
        // SAFETY: `[p, p + len)` is the mapping just made.
        let _ = unsafe { madvise(p, len, MADV_NOHUGEPAGE) };
        MAPPED.fetch_add(1, Ordering::Relaxed);
        add_live(&MAPPED_LIVE, &MAPPED_PEAK, len);
        p as *mut u8
    }

    /// Unmap a block [`map_block`] made for `size` bytes.
    unsafe fn unmap_block(p: *mut u8, size: usize) {
        let len = map_len(size);
        // SAFETY: the caller passes a block of ours with its current size.
        let _ = unsafe { munmap(p as *mut c_void, len) };
        sub_live(&MAPPED_LIVE, len);
    }

    /// Is `p` (a live block of `size` bytes) one of ours? Only a block of at
    /// least the threshold can be; mimalloc's page map answers the rest.
    #[inline]
    unsafe fn is_mapped(mode: SparseMode, p: *mut u8, size: usize, min: usize) -> bool {
        // SAFETY: a page-map lookup; it reads no memory at `p`.
        mode != SparseMode::Off
            && size >= min
            && !unsafe { mi_is_in_heap_region(p as *const c_void) }
    }

    /// The launcher's global allocator on Linux: mimalloc, with big sparse
    /// blocks mapped on their own. See the module docs.
    pub(crate) struct SparseBlockAlloc;

    impl SparseBlockAlloc {
        #[cold]
        unsafe fn alloc_big(&self, layout: Layout, zeroed: bool) -> *mut u8 {
            let (mode, min) = config();
            let size = layout.size();
            if size >= min {
                BIG_ALLOCS.fetch_add(1, Ordering::Relaxed);
                if zeroed {
                    BIG_ZEROED_ALLOCS.fetch_add(1, Ordering::Relaxed);
                    BIG_ZEROED_BYTES.fetch_add(size, Ordering::Relaxed);
                }
                if routes(mode, zeroed, size, layout.align(), min)
                    && (mode != SparseMode::Boot || take_boot_budget())
                {
                    // SAFETY: plain mapping.
                    let p = unsafe { map_block(size) };
                    if !p.is_null() {
                        add_live(&BIG_LIVE, &BIG_PEAK, size);
                        if CENSUS.load(Ordering::Relaxed) {
                            census_move(0, size);
                        }
                        return p;
                    }
                    // zt t2/fp: a refused mapping made nothing, so mode
                    // `boot` gets its budget back (it counted mappings that
                    // were never made, and `boot_budget_left` under-read).
                    if mode == SparseMode::Boot {
                        refund_boot_budget();
                    }
                }
            }
            // SAFETY: forwarding the caller's layout.
            let p = unsafe {
                if zeroed {
                    mimalloc::MiMalloc.alloc_zeroed(layout)
                } else {
                    mimalloc::MiMalloc.alloc(layout)
                }
            };
            if size >= min && !p.is_null() {
                add_live(&BIG_LIVE, &BIG_PEAK, size);
            }
            if !p.is_null() && CENSUS.load(Ordering::Relaxed) {
                census_move(0, size);
            }
            p
        }
    }

    // SAFETY: every block is either mimalloc's (forwarded unchanged) or a
    // private mapping of at least `layout.size()` bytes aligned to a page,
    // which satisfies any `layout.align() <= PAGE` (the only ones mapped).
    // `dealloc` / `realloc` tell the two apart by mimalloc's own page map.
    unsafe impl GlobalAlloc for SparseBlockAlloc {
        #[inline]
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if layout.size() < FAST_MIN.load(Ordering::Relaxed) {
                // SAFETY: forwarding.
                return unsafe { mimalloc::MiMalloc.alloc(layout) };
            }
            // SAFETY: as above.
            unsafe { self.alloc_big(layout, false) }
        }

        #[inline]
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if layout.size() < FAST_MIN.load(Ordering::Relaxed) {
                // SAFETY: forwarding.
                return unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) };
            }
            // SAFETY: as above.
            unsafe { self.alloc_big(layout, true) }
        }

        #[inline]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if layout.size() >= FAST_MIN.load(Ordering::Relaxed) {
                let (mode, min) = config();
                // SAFETY: the caller's contract, forwarded.
                unsafe { self.dealloc_with(mode, min, ptr, layout) };
                return;
            }
            // SAFETY: forwarding a mimalloc block.
            unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let fast_min = FAST_MIN.load(Ordering::Relaxed);
            if layout.size() < fast_min && new_size < fast_min {
                // SAFETY: forwarding.
                return unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) };
            }
            let (mode, min) = config();
            // SAFETY: the caller's contract, forwarded.
            unsafe { self.realloc_with(mode, min, ptr, layout, new_size) }
        }
    }

    impl SparseBlockAlloc {
        /// `dealloc` under an explicit `(mode, min)`: the latched pair in the
        /// allocator, any pair in a test. Ownership is by address, so a
        /// mapped block is unmapped under any mode but `Off`.
        unsafe fn dealloc_with(&self, mode: SparseMode, min: usize, ptr: *mut u8, layout: Layout) {
            let size = layout.size();
            if size >= min {
                sub_live(&BIG_LIVE, size);
            }
            if CENSUS.load(Ordering::Relaxed) {
                census_move(size, 0);
            }
            // SAFETY: `ptr` is a live block of `size` bytes.
            if unsafe { is_mapped(mode, ptr, size, min) } {
                // SAFETY: ours, with its current size.
                unsafe { unmap_block(ptr, size) };
                return;
            }
            // SAFETY: forwarding a mimalloc block.
            unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) }
        }

        /// `realloc` under an explicit `(mode, min)`; see [`Self::dealloc_with`].
        unsafe fn realloc_with(
            &self,
            mode: SparseMode,
            min: usize,
            ptr: *mut u8,
            layout: Layout,
            new_size: usize,
        ) -> *mut u8 {
            let old = layout.size();
            if old < min && new_size < min {
                // SAFETY: forwarding.
                let q = unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) };
                if !q.is_null() && CENSUS.load(Ordering::Relaxed) {
                    census_move(old, new_size);
                }
                return q;
            }
            // SAFETY: `ptr` is a live block of `old` bytes.
            let mapped = unsafe { is_mapped(mode, ptr, old, min) };
            let new_layout =
                // SAFETY: the `realloc` contract: `new_size`, rounded up to
                // `layout.align()`, does not overflow `isize`.
                unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            let p = if mapped && new_size >= min {
                let (old_len, new_len) = (map_len(old), map_len(new_size));
                if old_len == new_len {
                    ptr
                } else {
                    // SAFETY: `[ptr, ptr + old_len)` is our mapping; MAYMOVE
                    // keeps its contents and its NOHUGEPAGE advice.
                    let q = unsafe { mremap(ptr as *mut c_void, old_len, new_len, MREMAP_MAYMOVE) };
                    if q != MAP_FAILED_PTR && !q.is_null() {
                        REMAPS.fetch_add(1, Ordering::Relaxed);
                        if new_len > old_len {
                            add_live(&MAPPED_LIVE, &MAPPED_PEAK, new_len - old_len);
                        } else {
                            sub_live(&MAPPED_LIVE, old_len - new_len);
                        }
                        q as *mut u8
                    } else {
                        MAP_FAILED.fetch_add(1, Ordering::Relaxed);
                        // SAFETY: a fresh mimalloc block, filled from ours,
                        // which is then unmapped.
                        unsafe {
                            let q = mimalloc::MiMalloc.alloc(new_layout);
                            if q.is_null() {
                                return q;
                            }
                            std::ptr::copy_nonoverlapping(ptr, q, old.min(new_size));
                            unmap_block(ptr, old);
                            q
                        }
                    }
                }
            } else if mapped {
                // Shrunk below the threshold: back into mimalloc, so every
                // mapped block stays at least the threshold.
                // SAFETY: as in the fallback above.
                unsafe {
                    let q = mimalloc::MiMalloc.alloc(new_layout);
                    if q.is_null() {
                        return q;
                    }
                    std::ptr::copy_nonoverlapping(ptr, q, new_size);
                    unmap_block(ptr, old);
                    q
                }
            } else if routes(mode, false, new_size, layout.align(), min) {
                // Mode `all`: a mimalloc block grown past the threshold moves
                // into a mapping.
                // SAFETY: a fresh mapping filled from the mimalloc block,
                // which is then freed; on a refused mapping, plain realloc.
                unsafe {
                    let q = map_block(new_size);
                    if q.is_null() {
                        let q = mimalloc::MiMalloc.realloc(ptr, layout, new_size);
                        if q.is_null() {
                            return q;
                        }
                        q
                    } else {
                        std::ptr::copy_nonoverlapping(ptr, q, old.min(new_size));
                        mimalloc::MiMalloc.dealloc(ptr, layout);
                        q
                    }
                }
            } else {
                // SAFETY: forwarding a mimalloc block.
                let q = unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) };
                if q.is_null() {
                    return q;
                }
                q
            };
            // The block moved from `old` to `new_size` bytes (a null return
            // above left the old block, and the counters, as they were).
            if old >= min {
                sub_live(&BIG_LIVE, old);
            }
            if new_size >= min {
                add_live(&BIG_LIVE, &BIG_PEAK, new_size);
            }
            if CENSUS.load(Ordering::Relaxed) {
                census_move(old, new_size);
            }
            p
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The real mapping path, driven directly (the test binary's global
        /// allocator is this wrapper too, in whatever mode its environment
        /// latched): a mapped block reads zero, is not mimalloc's, keeps its
        /// bytes across a grow and a shrink-back, and is freed.
        #[test]
        fn zr10mt_a_mapped_block_is_zero_survives_realloc_and_is_not_mimallocs() {
            let a = SparseBlockAlloc;
            let size = 3 << 20;
            let layout = Layout::from_size_align(size, 8).unwrap();
            // SAFETY: test-local block, used within its layout.
            unsafe {
                let p = map_block(size);
                assert!(!p.is_null());
                assert!(
                    !mi_is_in_heap_region(p as *const c_void),
                    "a mapping is not mimalloc's"
                );
                assert_eq!(p as usize % PAGE, 0);
                assert!(std::slice::from_raw_parts(p, size).iter().all(|&b| b == 0));
                *p = 7;
                *p.add(size - 1) = 9;
                BIG_LIVE.fetch_add(size, Ordering::Relaxed);
                // An explicit mode, whatever this binary's environment
                // latched: ownership is by address, so it is sound.
                let (mode, min) = (SparseMode::Zeroed, 1 << 20);
                let q = a.realloc_with(mode, min, p, layout, 2 * size);
                assert!(!q.is_null());
                assert!(!mi_is_in_heap_region(q as *const c_void));
                assert_eq!(*q, 7);
                assert_eq!(*q.add(size - 1), 9);
                let grown = Layout::from_size_align(2 * size, 8).unwrap();
                let r = a.realloc_with(mode, min, q, grown, 16);
                assert!(!r.is_null());
                assert!(
                    mi_is_in_heap_region(r as *const c_void),
                    "a shrink below the threshold returns to mimalloc"
                );
                assert_eq!(*r, 7);
                a.dealloc_with(mode, min, r, Layout::from_size_align(16, 8).unwrap());
                // And a mapped block's dealloc is an unmap.
                let m = map_block(size);
                assert!(!m.is_null());
                let live = MAPPED_LIVE.load(Ordering::Relaxed);
                assert!(live >= map_len(size));
                a.dealloc_with(mode, min, m, layout);
            }
        }

        /// zr z11/rv9: the linked mimalloc is one whose page map backs the
        /// ownership rule (v3). If this fails, a dependency switched the
        /// `mimalloc` crate to its v2 backend and the latch forces `off`.
        #[test]
        fn zr11rv9_the_linked_mimalloc_backs_the_ownership_rule() {
            // SAFETY: a plain FFI call.
            let v = unsafe { mi_version() };
            assert!(backend_supports_mapping(v), "mimalloc {v} is not v3");
        }

        /// The kill arm never unmaps: under `Off` the page-map test is not
        /// even asked, so a block's owner cannot be misread.
        #[test]
        fn zr10mt_off_never_claims_a_block() {
            // SAFETY: a page-map lookup is not made for `Off`; the pointer is
            // never dereferenced.
            unsafe {
                let fake = 0x1000 as *mut u8;
                assert!(!is_mapped(SparseMode::Off, fake, 1 << 30, 1 << 20));
                assert!(
                    !is_mapped(SparseMode::Zeroed, fake, 4096, 1 << 20),
                    "under min"
                );
            }
        }

        /// A small block is mimalloc's and the page-map test says so: the
        /// address test cannot mistake one for a mapping.
        #[test]
        fn zr10mt_a_mimalloc_block_is_in_its_heap_region() {
            let a = SparseBlockAlloc;
            let layout = Layout::from_size_align(64, 8).unwrap();
            // SAFETY: test-local block.
            unsafe {
                let p = a.alloc(layout);
                assert!(mi_is_in_heap_region(p as *const c_void));
                // Claim a size over the threshold so the page-map test runs.
                assert!(!is_mapped(SparseMode::Zeroed, p, 1 << 30, 1 << 20));
                a.dealloc(p, layout);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zr10mt_the_mode_switch_reads_off_all_and_defaults() {
        assert_eq!(parse_mode(None, SparseMode::Zeroed), SparseMode::Zeroed);
        assert_eq!(parse_mode(None, SparseMode::Off), SparseMode::Off);
        assert_eq!(
            parse_mode(Some(b"  "), SparseMode::Zeroed),
            SparseMode::Zeroed
        );
        for v in [&b"0"[..], b"off", b" FALSE ", b"no"] {
            assert_eq!(
                parse_mode(Some(v), SparseMode::Zeroed),
                SparseMode::Off,
                "{v:?}"
            );
        }
        for v in [&b"1"[..], b"on", b"zeroed", b"yes"] {
            assert_eq!(
                parse_mode(Some(v), SparseMode::Off),
                SparseMode::Zeroed,
                "{v:?}"
            );
        }
        assert_eq!(parse_mode(Some(b"ALL"), SparseMode::Off), SparseMode::All);
    }

    #[test]
    fn zr10mt_the_threshold_parses_decimal_kib_with_a_floor() {
        assert_eq!(parse_min_kb(None), DEFAULT_MIN_KB);
        assert_eq!(parse_min_kb(Some(b"")), DEFAULT_MIN_KB);
        assert_eq!(parse_min_kb(Some(b"2048")), 2048);
        assert_eq!(parse_min_kb(Some(b" 512 ")), 512);
        assert_eq!(
            parse_min_kb(Some(b"4")),
            FLOOR_MIN_KB,
            "clamped up to the floor"
        );
        assert_eq!(
            parse_min_kb(Some(b"1m")),
            DEFAULT_MIN_KB,
            "no suffixes: unparsable"
        );
        assert_eq!(parse_min_kb(Some(b"-1")), DEFAULT_MIN_KB);
        assert_eq!(
            parse_min_kb(Some(b"99999999999999999999999999")),
            DEFAULT_MIN_KB
        );
    }

    #[test]
    fn zr10mt_only_big_page_aligned_requests_of_the_mode_are_routed() {
        let min = 1 << 20;
        assert!(routes(SparseMode::Zeroed, true, min, 8, min));
        assert!(
            !routes(SparseMode::Zeroed, false, min, 8, min),
            "zeroed mode leaves plain alloc"
        );
        assert!(
            !routes(SparseMode::Zeroed, true, min - 1, 8, min),
            "under the threshold"
        );
        assert!(
            !routes(SparseMode::Zeroed, true, min, 2 * PAGE, min),
            "over-page alignment"
        );
        assert!(routes(SparseMode::All, false, min, PAGE, min));
        assert!(!routes(SparseMode::Off, true, 1 << 30, 8, min));
        assert!(!routes(SparseMode::Undecided, true, 1 << 30, 8, min));
    }

    #[test]
    fn zr11rv9_only_a_v3_backend_may_map() {
        assert!(backend_supports_mapping(332));
        assert!(backend_supports_mapping(300));
        assert!(!backend_supports_mapping(219));
        assert!(!backend_supports_mapping(0));
    }

    #[test]
    fn zr10mt_map_len_is_whole_pages() {
        assert_eq!(map_len(1), PAGE);
        assert_eq!(map_len(PAGE), PAGE);
        assert_eq!(map_len(PAGE + 1), 2 * PAGE);
        assert_eq!(map_len(1 << 20), 1 << 20);
    }

    #[test]
    fn zs9rs2_boot_mode_parses_routes_zeroed_and_counts_its_budget() {
        assert_eq!(parse_mode(Some(b"boot"), SparseMode::Off), SparseMode::Boot);
        assert_eq!(
            parse_mode(Some(b" BOOT "), SparseMode::Off),
            SparseMode::Boot
        );
        assert_eq!(
            SparseMode::from_u8(SparseMode::Boot as u8),
            SparseMode::Boot
        );
        assert_eq!(SparseMode::Boot.name(), "boot");
        let min = 1 << 20;
        assert!(routes(SparseMode::Boot, true, min, 8, min));
        assert!(
            !routes(SparseMode::Boot, false, min, 8, min),
            "only zeroed blocks"
        );
        assert_eq!(parse_boot_count(None), DEFAULT_BOOT_COUNT);
        assert_eq!(parse_boot_count(Some(b"")), DEFAULT_BOOT_COUNT);
        assert_eq!(parse_boot_count(Some(b"x1")), DEFAULT_BOOT_COUNT);
        assert_eq!(parse_boot_count(Some(b" 0 ")), 0);
        assert_eq!(parse_boot_count(Some(b"128")), 128);
    }

    /// zt t2/fp: a refund puts back exactly the mapping a take removed. Skipped
    /// when this test binary's own allocator latched mode `boot` (it would
    /// race the budget).
    #[test]
    fn zt2fp_a_refused_boot_mapping_refunds_its_budget() {
        if SparseMode::from_u8(MODE.load(Ordering::Acquire)) == SparseMode::Boot {
            return;
        }
        let before = BOOT_BUDGET.load(Ordering::Acquire);
        refund_boot_budget();
        assert_eq!(BOOT_BUDGET.load(Ordering::Acquire), before + 1);
        assert!(take_boot_budget(), "the refunded mapping can be taken");
        assert_eq!(BOOT_BUDGET.load(Ordering::Acquire), before);
    }

    #[test]
    fn zs6rs_census_buckets_partition_the_sizes() {
        assert_eq!(census_bucket(0), 0);
        assert_eq!(census_bucket(64), 0);
        assert_eq!(census_bucket(65), 1);
        assert_eq!(census_bucket(512), 1);
        assert_eq!(census_bucket(8 << 10), 2);
        assert_eq!(census_bucket((8 << 10) + 1), 3);
        assert_eq!(census_bucket(64 << 10), 3);
        assert_eq!(census_bucket((1 << 20) - 1), 4);
        assert_eq!(census_bucket(1 << 20), 5);
        assert_eq!(census_bucket(usize::MAX), 5);
    }

    #[test]
    fn zs6rs_census_line_is_off_unless_latched() {
        // The test binary's allocator latched from its own environment; the
        // line exists exactly when the census is on, and then names every
        // bucket.
        match census_line() {
            None => assert!(!CENSUS.load(Ordering::Relaxed)),
            Some(line) => {
                for (name, _) in CENSUS_BUCKETS {
                    assert!(line.contains(&format!(" live_{name}_kb=")), "{line}");
                    assert!(line.contains(&format!(" peak_{name}_kb=")), "{line}");
                }
            }
        }
    }

    #[test]
    fn zr10mt_the_summary_line_names_every_counter() {
        let line = summary_line();
        for key in [
            "[cratonvm] mimalloc-sparse-blocks: mode=",
            " min_kb=",
            " big_allocs=",
            " big_zeroed_allocs=",
            " big_zeroed_kb=",
            " big_live_kb=",
            " big_live_peak_kb=",
            " mapped=",
            " mapped_live_kb=",
            " mapped_peak_kb=",
            " remaps=",
            " map_failed=",
            " boot_budget_left=",
            " boot_declined=",
        ] {
            assert!(line.contains(key), "{key} missing from {line}");
        }
    }
}
