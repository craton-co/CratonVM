// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `CRATONVM_DBG_ROOT_WRITE_AUDIT`: an opt-in, release-mode audit of every
//! RAW root write-back.
//!
//! A raw root write-back is a collector-side store of a relocated address into
//! a word that is NOT a Java heap field and that the VM names only by its
//! address: a blocked peer's native-stack word (`apply_native_slot_fixups`), an
//! oop-map slot or an unmapped duplicate of a live compiled frame
//! (`remap_one_jit_frame`, `remap_unmapped_frame_dupes`), a callee-saved
//! register image (`remap_one_frame_register_images`) and the frame word an
//! indirect shadow-stack entry names (`ShadowStack::remap_in`). Typed slots (`Vec<ObjectRef>`, `Option<ObjectRef>`
//! fields, the JNI global-ref boxes) cannot land anywhere but their own
//! storage, and are not audited.
//!
//! gen r5w1/crash5 (2026-09-26), for
//! `docs/internal/gc/generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928.md`.
//! That page records native words (an `Arc` pointer, hashbrown control bytes,
//! a cached `OnceLock<bool>`, mimalloc's thread-local heap pointer) overwritten
//! at an unpredictable moment from an unpredictable thread, and asks for the
//! write-back that did it. Every writer above runs on the thread whose stack
//! the word should belong to, so the one question this audit asks of each
//! store is: does the target word lie in the WRITING thread's own live stack?
//!
//! * `own-live`: in `[sp, stack top)` of the writer, i.e. in a frame that is
//!   still running. The expected answer for every kind.
//! * `own-below-sp`: in the writer's stack but below its stack pointer, i.e. a
//!   dead frame. Harmless to the program, and a sign the slot was recorded for
//!   a frame that has since returned.
//! * `foreign`: not in the writer's stack at all -- another thread's stack,
//!   thread-local storage, the heap of the allocator. A store there is the
//!   corruption the page describes.
//! * `unknown`: the platform cannot say where the stack is.
//!
//! Each audited store prints one line on stderr, rate-limited (the first
//! [`ORDINARY_LINE_BUDGET`] `own-live` stores, and separately the first
//! [`ANOMALY_LINE_BUDGET`] of every other verdict, so a flood of ordinary
//! stores cannot hide the one that matters):
//!
//! ```text
//! [root-write-audit] os_tid=<tid> kind=<kind> slot=<addr> old=<value> new=<value> place=<verdict> written=<bool> stack=[<lo>,<hi>) sp=<sp>
//! ```
//!
//! `written=false` is a store the caller DECLINED (for the native-stack
//! write-back: a word outside the own stack, which the gen r5w1/crash5 guard
//! refuses, or a word that no longer reads its captured value). To match a
//! crash, compare the crash report's faulting value and address against the
//! `slot=` / `new=` pairs.
//!
//! The CAPTURE side is audited too, on Linux: a blocked peer's helper-window
//! band that runs past the stack the peer published
//! ([`note_band_overrun`]), and each object-looking word captured from beyond
//! it ([`note_foreign_capture`]) -- the words that, adopted by the peer, were
//! stored into another mapping's memory at its wake before the gen r5w1/crash5
//! own-stack bound.
//!
//! Off unless the variable is set (`CRATONVM_DBG=root-write-audit`). When off,
//! every hook is one latched load.
//!
//! # The heap-write half (gen r5w2/roots6, 2026-09-26)
//!
//! A root write-back stores into a word the VM names by address; the
//! collector's BULK writes store into ranges it computed itself -- the copy of
//! a survivor, the zeroing of a dead span, a gap filler, a forwarding-pointer
//! install, the conservative rewrite of an unparseable stretch. Each of those
//! is supposed to land inside a Java heap region (a young semi-space or the
//! old generation), and the netty crash this audit exists for stored a 16-byte
//! Java `Value` cell (`Value::Int(1)`, first word `0x1_0000_0000`) over a Rust
//! heap allocation: exactly what a bulk write with a bad destination produces.
//! [`heap_write_in_bounds`] checks each such range against the regions the
//! writer believes it is writing and, on a miss, prints the range, the
//! regions and a backtrace, then ABORTS -- the point is to stop at the writer,
//! not three phases later in whoever reads the damage. [`MapWatch`] is the
//! reader-side twin: it fingerprints a collector-owned `PointerMap` (its entry
//! count, its capacity and the bytes of its handle) and aborts at the first
//! checkpoint where the fingerprint moved without the collector touching the
//! map, naming the two checkpoints the corrupting store happened between.
//!
//! # Ring mode (gen r5w4/pin8, 2026-09-26)
//!
//! `CRATONVM_DBG_ROOT_WRITE_AUDIT=ring` (token `root-write-audit=ring`). The
//! printing audit above MASKED the crash it was built for: with it armed the
//! netty buffer classes passed 6 of 6 under `CRATONVM_GEN_PINNED_YOUNG_COPY=1`,
//! because twenty thousand `eprintln!`s change the timing the crash needs. In
//! ring mode every raw root write-back is classified exactly as before but
//! printed NOWHERE: it goes into a lock-free in-memory ring (the last
//! [`RING_LEN`] stores, plus the first [`ANOMALY_RING_LEN`] stores whose place
//! is not `own-live`, which are never overwritten), at the cost of a handful of
//! relaxed atomic stores per write-back. The rings are printed
//!
//! * at process exit (a C `atexit` hook, registered the first time ring mode is
//!   read),
//! * before every abort this module makes (a heap write outside the heap, a
//!   collector map or a scanner local that changed under its owner), and
//! * from a crash handler, through [`dump_ring_for_crash`], which formats into a
//!   stack buffer and calls `write(2)` directly (no allocation, no lock).
//!
//! The dump's lines are `[root-write-audit ring] #<seq> os_tid=.. kind=..
//! slot=.. old=.. new=.. place=.. written=.. sp=..`: to match a crash, grep the
//! crashing thread's `os_tid` and the corrupted word's address (`slot=`) or
//! value (`new=`).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::shadow_stack::SlotBand;

/// Which raw write-back a line describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootWriteKind {
    /// A blocked peer's native-stack word, captured by the cross-thread
    /// helper-window scan and written back at the peer's wake
    /// (`vm_exec::apply_native_slot_fixups`).
    BlockedPeerNativeSlot,
    /// An oop-map slot of a live compiled frame (`remap_one_jit_frame`).
    JitFrameSlot,
    /// A word of a live compiled frame no map named that still held a moved
    /// base (`remap_unmapped_frame_dupes`).
    JitFrameDuplicate,
    /// A callee-saved GPR image word of a live compiled frame
    /// (`remap_one_frame_register_images`).
    JitRegisterImage,
    /// The frame word an indirect shadow-stack entry names
    /// (`ShadowStack::remap_in`). A value entry's home is the shadow stack's
    /// own buffer and is not audited.
    ShadowStackHome,
}

impl RootWriteKind {
    /// The `kind=` word of the audit line.
    pub fn label(self) -> &'static str {
        match self {
            RootWriteKind::BlockedPeerNativeSlot => "blocked-peer-native-slot",
            RootWriteKind::JitFrameSlot => "jit-frame-slot",
            RootWriteKind::JitFrameDuplicate => "jit-frame-duplicate",
            RootWriteKind::JitRegisterImage => "jit-register-image",
            RootWriteKind::ShadowStackHome => "shadow-stack-home",
        }
    }
}

/// Where a raw slot lies relative to the CALLING thread's stack. See the
/// module docs for what each answer means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotPlace {
    /// In `[sp, stack top)` of the calling thread.
    OwnLive,
    /// In the calling thread's stack, below its stack pointer.
    OwnBelowSp,
    /// Outside the calling thread's stack.
    Foreign,
    /// The platform reports no stack bounds for this thread.
    Unknown,
}

impl SlotPlace {
    /// The `place=` word of the audit line.
    pub fn label(self) -> &'static str {
        match self {
            SlotPlace::OwnLive => "own-live",
            SlotPlace::OwnBelowSp => "own-below-sp",
            SlotPlace::Foreign => "foreign",
            SlotPlace::Unknown => "unknown",
        }
    }
}

/// Lines printed for `own-live` stores before the audit goes quiet for them.
pub const ORDINARY_LINE_BUDGET: u64 = 20_000;

/// Lines printed for every other verdict, counted separately.
pub const ANOMALY_LINE_BUDGET: u64 = 4_096;

static ORDINARY_LINES: AtomicU64 = AtomicU64::new(0);
static ANOMALY_LINES: AtomicU64 = AtomicU64::new(0);

/// Stores audited, by [`SlotPlace`] (`own-live`, `own-below-sp`, `foreign`,
/// `unknown`). The denominators of the lines: a budget that ran out is visible
/// here, and a zero everywhere means no raw write-back ran at all.
static AUDITED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Whether the audit is armed. Latched: it decides whether every raw
/// write-back pays a stack-bounds lookup, which must not change mid-run.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOT_WRITE_AUDIT").is_some()
    })
}

/// `(own-live, own-below-sp, foreign, unknown)` stores audited so far.
pub fn census() -> [u64; 4] {
    [
        AUDITED[0].load(Ordering::Relaxed),
        AUDITED[1].load(Ordering::Relaxed),
        AUDITED[2].load(Ordering::Relaxed),
        AUDITED[3].load(Ordering::Relaxed),
    ]
}

/// Classify `slot` against the calling thread's stack.
///
/// `#[inline(never)]` for the reason `SlotBand::current_thread` gives: the
/// stack-pointer probe must sit in a frame BELOW every caller's, so a caller's
/// own locals classify as `own-live`.
#[inline(never)]
pub fn classify_slot(slot: usize) -> SlotPlace {
    classify_slot_with_sp(slot).0
}

/// [`classify_slot`] plus the band and stack pointer it used, for the line.
#[inline(never)]
fn classify_slot_with_sp(slot: usize) -> (SlotPlace, usize, usize, usize) {
    let probe = 0u8;
    let sp = (std::hint::black_box(&probe) as *const u8 as usize) & !0x7;
    match crate::shadow_stack::current_thread_stack() {
        None => (SlotPlace::Unknown, 0, 0, sp),
        Some(stack) => {
            let place = place_in(slot, stack.lo, stack.hi, sp);
            (place, stack.lo, stack.hi, sp)
        }
    }
}

/// The verdict for `slot` in a thread stack `[lo, hi)` whose stack pointer is
/// `sp`. Split out so the rule is testable without a real stack.
#[inline]
fn place_in(slot: usize, lo: usize, hi: usize, sp: usize) -> SlotPlace {
    if slot < lo || slot >= hi || hi - slot < std::mem::size_of::<usize>() {
        SlotPlace::Foreign
    } else if sp >= lo && sp < hi && slot < sp {
        SlotPlace::OwnBelowSp
    } else {
        SlotPlace::OwnLive
    }
}

/// Audit one raw write-back of `new` over `old` at `slot`. `written` says
/// whether the caller made (or is about to make) the store; `false` is a
/// store it declined. Call only when [`enabled`]; the hook sites test it
/// first so the disarmed cost stays one load.
#[cold]
#[inline(never)]
pub fn note_write(kind: RootWriteKind, slot: usize, old: usize, new: usize, written: bool) {
    let (place, lo, hi, sp) = classify_slot_with_sp(slot);
    let idx = match place {
        SlotPlace::OwnLive => 0,
        SlotPlace::OwnBelowSp => 1,
        SlotPlace::Foreign => 2,
        SlotPlace::Unknown => 3,
    };
    AUDITED[idx].fetch_add(1, Ordering::Relaxed);
    // gen r5w4/pin8: ring mode records the store and prints nothing.
    if ring_mode() {
        ring_record(kind, place, slot, old, new, written, sp);
        return;
    }
    let (budget, cap) = if place == SlotPlace::OwnLive {
        (&ORDINARY_LINES, ORDINARY_LINE_BUDGET)
    } else {
        (&ANOMALY_LINES, ANOMALY_LINE_BUDGET)
    };
    let n = budget.fetch_add(1, Ordering::Relaxed);
    if n < cap {
        eprintln!(
            "[root-write-audit] os_tid={} kind={} slot={slot:#x} old={old:#x} new={new:#x} \
             place={} written={written} stack=[{lo:#x},{hi:#x}) sp={sp:#x}",
            os_tid(),
            kind.label(),
            place.label(),
        );
    } else if n == cap {
        eprintln!(
            "[root-write-audit] line budget for place={} exhausted after {cap} lines; \
             later stores of that verdict are counted, not printed",
            if place == SlotPlace::OwnLive {
                "own-live"
            } else {
                "own-below-sp/foreign/unknown"
            }
        );
    }
}

static CAPTURE_LINES: AtomicU64 = AtomicU64::new(0);
static FOREIGN_CAPTURES: AtomicU64 = AtomicU64::new(0);
static BAND_OVERRUNS: AtomicU64 = AtomicU64::new(0);

/// `(helper-window bands that ran past the peer's own stack, captures taken
/// from beyond it)` so far. Counted only while the audit is armed.
pub fn capture_census() -> (u64, u64) {
    (
        BAND_OVERRUNS.load(Ordering::Relaxed),
        FOREIGN_CAPTURES.load(Ordering::Relaxed),
    )
}

/// The CAPTURE side of the audit: a blocked peer's helper-window band
/// (`xt_root_scan::snapshot_parked_slot`, `[rsp, end)` with `end` taken from a
/// `/proc/self/maps` line) ran past the stack the peer itself published
/// (`shadow_stack::thread_stack_band`). Everything beyond `own.hi` is some
/// other mapping's memory -- a neighbouring or cached thread stack, an
/// allocator segment -- read as this peer's roots.
#[cold]
#[inline(never)]
pub fn note_band_overrun(peer_os_tid: u32, rsp: usize, band_end: usize, own: SlotBand) {
    BAND_OVERRUNS.fetch_add(1, Ordering::Relaxed);
    if CAPTURE_LINES.fetch_add(1, Ordering::Relaxed) < ANOMALY_LINE_BUDGET {
        eprintln!(
            "[root-write-audit] helper-window band of peer os_tid={peer_os_tid} ran past its own \
             stack: band=[{rsp:#x},{band_end:#x}) own=[{:#x},{:#x}) overrun={} bytes",
            own.lo,
            own.hi,
            band_end - own.hi,
        );
    }
}

/// One object-looking word captured from beyond the peer's own stack (see
/// [`note_band_overrun`]). The capture is attributed to the peer, adopted by
/// it, and written back at its wake -- refused since gen r5w1/crash5 by the
/// write-back's own-stack bound, which logs `written=false place=foreign`.
#[cold]
#[inline(never)]
pub fn note_foreign_capture(peer_os_tid: u32, addr: usize, value: usize, own: SlotBand) {
    FOREIGN_CAPTURES.fetch_add(1, Ordering::Relaxed);
    if CAPTURE_LINES.fetch_add(1, Ordering::Relaxed) < ANOMALY_LINE_BUDGET {
        eprintln!(
            "[root-write-audit] capture for peer os_tid={peer_os_tid} at {addr:#x} (value \
             {value:#x}) lies outside its own stack [{:#x},{:#x})",
            own.lo, own.hi,
        );
    }
}

/// Which bulk collector write a heap-write audit line describes (gen
/// r5w2/roots6). See the module docs' "heap-write half".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapWriteKind {
    /// A young survivor's bytes copied to its destination by the serial
    /// evacuator (`forward_object_impl`): to-space, old gen, or -- on a pinned
    /// in-place cycle -- a from-space span that was free when the cycle began.
    EvacuationCopy,
    /// A dead span of from-space zeroed by the pinned in-place cycle
    /// (`finish_in_place_young_cycle`).
    InPlaceZero,
    /// A sub-header sliver stamped with the GAP sentinel by the pinned
    /// in-place cycle (`finish_in_place_young_cycle`).
    InPlaceFiller,
    /// A survivor copied into old gen by the non-moving sweep's selective
    /// promotion.
    PromotionCopy,
    /// A forwarding pointer or an age byte written into a young source header
    /// by the non-moving sweep's deferred installs.
    SweepHeaderWrite,
    /// An unparseable from-space stretch handed to the non-moving sweep's
    /// conservative rewrite (`rewrite_stretch`): the whole stretch, once.
    SweepStretchRewrite,
    /// gen r5w4/pin8: a span a pinned in-place cycle VACATED (a survivor's old
    /// copy), stamped as a poisoned filler instead of being zeroed and
    /// free-listed, under `CRATONVM_DBG_STALE_OBJREF`
    /// (`finish_in_place_young_cycle`).
    InPlaceQuarantine,
}

impl HeapWriteKind {
    /// The `kind=` word of the heap-write line.
    pub fn label(self) -> &'static str {
        match self {
            HeapWriteKind::EvacuationCopy => "evacuation-copy",
            HeapWriteKind::InPlaceZero => "in-place-zero",
            HeapWriteKind::InPlaceFiller => "in-place-filler",
            HeapWriteKind::PromotionCopy => "promotion-copy",
            HeapWriteKind::SweepHeaderWrite => "sweep-header-write",
            HeapWriteKind::SweepStretchRewrite => "sweep-stretch-rewrite",
            HeapWriteKind::InPlaceQuarantine => "in-place-quarantine",
        }
    }
}

/// Bulk writes checked, and those found outside every region their writer
/// named. Counted only while the audit is armed.
static HEAP_WRITES_CHECKED: AtomicU64 = AtomicU64::new(0);
static HEAP_WRITES_OUTSIDE: AtomicU64 = AtomicU64::new(0);

/// `(bulk writes checked, bulk writes found outside their regions)` so far.
pub fn heap_write_census() -> (u64, u64) {
    (
        HEAP_WRITES_CHECKED.load(Ordering::Relaxed),
        HEAP_WRITES_OUTSIDE.load(Ordering::Relaxed),
    )
}

/// Does `[addr, addr + len)` lie entirely inside one of `regions` (each an
/// absolute `[lo, hi)`)? Pure, so the rule is testable.
#[inline]
fn span_inside_one(addr: usize, len: usize, regions: &[(usize, usize)]) -> bool {
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    regions
        .iter()
        .any(|&(lo, hi)| lo < hi && addr >= lo && end <= hi)
}

/// The heap-write audit: `true` when the bulk write `[addr, addr + len)` lies
/// inside one of `regions`, the Java heap regions its writer may write. On a
/// miss it prints one line naming the write, the regions and the calling
/// stack, and ABORTS the process: a bulk write outside the heap is the
/// corruption this audit is for, and letting it proceed only moves the crash
/// to whoever next reads the damaged allocation. Call only when [`enabled`].
#[cold]
#[inline(never)]
pub fn heap_write_in_bounds(
    kind: HeapWriteKind,
    addr: usize,
    len: usize,
    regions: &[(usize, usize)],
) -> bool {
    HEAP_WRITES_CHECKED.fetch_add(1, Ordering::Relaxed);
    if span_inside_one(addr, len, regions) {
        return true;
    }
    HEAP_WRITES_OUTSIDE.fetch_add(1, Ordering::Relaxed);
    let regions_text: Vec<String> = regions
        .iter()
        .map(|&(lo, hi)| format!("[{lo:#x},{hi:#x})"))
        .collect();
    eprintln!(
        "[root-write-audit] HEAP WRITE OUTSIDE THE HEAP: os_tid={} kind={} range=[{addr:#x},{:#x}) \
         len={len} regions={} -- aborting at the writer\n{}",
        os_tid(),
        kind.label(),
        addr.wrapping_add(len),
        regions_text.join(","),
        std::backtrace::Backtrace::force_capture(),
    );
    if ring_mode() {
        dump_ring("heap write outside the heap");
    }
    std::process::abort();
}

/// A collector-owned `PointerMap`'s fingerprint, checked at named points of a
/// phase that must not change the map (gen r5w2/roots6; the reader-side twin
/// of [`heap_write_in_bounds`]). Arm with [`MapWatch::arm`] after the last
/// write the collector makes to the map; every [`MapWatch::check`] compares
/// the entry count, the capacity and the raw bytes of the map's handle (the
/// `Vec` header on the collector's own stack frame) with the armed values.
///
/// The three answers split the corruption the netty crash showed
/// (`PointerMap::get` on `evac_map` reading a table pointer of `0x1_0000_0000`):
/// a changed handle is a store into the collector's STACK frame; an unchanged
/// handle with a changed count or capacity is a store into the map's HEAP
/// tables. Either way the line names the checkpoint pair it happened between,
/// and the process aborts there.
pub struct MapWatch {
    site: &'static str,
    len: usize,
    capacity: usize,
    handle: [u8; MAP_HANDLE_BYTES],
    handle_addr: usize,
}

/// Bytes of a `PointerMap` handle (its `Vec` of shards).
const MAP_HANDLE_BYTES: usize = std::mem::size_of::<cratonvm_types::PointerMap>();

/// The raw bytes of `map`'s handle. A `PointerMap` is one `Vec`, three words
/// with no padding, so every byte is initialised.
fn map_handle_bytes(map: &cratonvm_types::PointerMap) -> [u8; MAP_HANDLE_BYTES] {
    let mut out = [0u8; MAP_HANDLE_BYTES];
    // SAFETY: `map` is a live, aligned `PointerMap` of exactly
    // `MAP_HANDLE_BYTES` bytes; they are copied, not interpreted.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (map as *const cratonvm_types::PointerMap).cast::<u8>(),
            out.as_mut_ptr(),
            MAP_HANDLE_BYTES,
        );
    }
    out
}

impl MapWatch {
    /// Fingerprint `map` now, at `site`.
    pub fn arm(map: &cratonvm_types::PointerMap, site: &'static str) -> Self {
        Self {
            site,
            len: map.len(),
            capacity: map.capacity(),
            handle: map_handle_bytes(map),
            handle_addr: map as *const cratonvm_types::PointerMap as usize,
        }
    }

    /// Compare `map` with the armed fingerprint at `site`. Aborts, after one
    /// line, when anything moved; otherwise records `site` as the last clean
    /// checkpoint.
    pub fn check(&mut self, map: &cratonvm_types::PointerMap, site: &'static str) {
        let handle = map_handle_bytes(map);
        let handle_moved = handle != self.handle;
        // Read the count and capacity only through an unchanged handle: a
        // corrupted `Vec` header would send them through a wild pointer.
        let (len, capacity) = if handle_moved {
            (usize::MAX, usize::MAX)
        } else {
            (map.len(), map.capacity())
        };
        if !handle_moved && len == self.len && capacity == self.capacity {
            self.site = site;
            return;
        }
        eprintln!(
            "[root-write-audit] COLLECTOR MAP CHANGED UNDER THE COLLECTOR: os_tid={} map@{:#x} \
             between `{}` and `{site}`: handle_changed={handle_moved} (was {:02x?}, now {:02x?}) \
             len {} -> {} capacity {} -> {} -- aborting\n{}",
            os_tid(),
            self.handle_addr,
            self.site,
            self.handle,
            handle,
            self.len,
            if handle_moved {
                "?".to_string()
            } else {
                len.to_string()
            },
            self.capacity,
            if handle_moved {
                "?".to_string()
            } else {
                capacity.to_string()
            },
            std::backtrace::Backtrace::force_capture(),
        );
        if ring_mode() {
            dump_ring("collector map changed");
        }
        std::process::abort();
    }
}

/// gen r5w4/pin8: a LOCAL of a thread's own root-scanning frame changed while
/// that thread was running the scan -- the scanner's own code writes nothing
/// there between arming and checking (see the caller,
/// `verify_precise_covers_conservative`'s set watch in
/// `vm/src/jit/conservative_roots.rs`). Prints one line naming the local, its
/// bytes before and after and the calling stack, dumps the rings in ring mode,
/// and aborts at the first point the damage is visible.
///
/// `site` names the local and `note` says which half changed: the handle on
/// the stack (a stack store into the scanner's frame) or the table it owns
/// (a store into that heap allocation).
#[cold]
#[inline(never)]
pub fn scanner_local_changed(
    site: &str,
    local_addr: usize,
    before: &[u8],
    after: &[u8],
    note: std::fmt::Arguments<'_>,
) -> ! {
    eprintln!(
        "[root-write-audit] SCANNER LOCAL CHANGED UNDER THE SCANNER: os_tid={} local={site} \
         @{local_addr:#x} (was {before:02x?}, now {after:02x?}) {note} -- aborting\n{}",
        os_tid(),
        std::backtrace::Backtrace::force_capture(),
    );
    if ring_mode() {
        dump_ring("scanner local changed");
    }
    std::process::abort();
}

// ---------------------------------------------------------------------------
// Ring mode (gen r5w4/pin8). See the module docs.
// ---------------------------------------------------------------------------

/// Records the ordinary ring keeps: the LAST this many audited write-backs.
pub const RING_LEN: usize = 1 << 14;

/// Records the anomaly ring keeps: the FIRST this many write-backs whose place
/// is not `own-live`. Never overwritten, so a flood of ordinary stores after
/// them cannot push them out.
pub const ANOMALY_RING_LEN: usize = 1 << 10;

/// One ring record. `seq` is `0` while the record is unwritten or being
/// rewritten and `n + 1` once record `n` is complete, so a reader that sees the
/// same non-zero `seq` before and after reading the fields read one whole
/// record.
struct RingRec {
    seq: AtomicU64,
    tid: AtomicU64,
    /// Bits 0..8: the kind code ([`kind_code`]); 8..16: the place code
    /// ([`place_code`]); bit 16: `written`.
    tag: AtomicU64,
    slot: AtomicU64,
    old: AtomicU64,
    new: AtomicU64,
    sp: AtomicU64,
}

impl RingRec {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            tid: AtomicU64::new(0),
            tag: AtomicU64::new(0),
            slot: AtomicU64::new(0),
            old: AtomicU64::new(0),
            new: AtomicU64::new(0),
            sp: AtomicU64::new(0),
        }
    }
}

static RING: [RingRec; RING_LEN] = [const { RingRec::new() }; RING_LEN];
static RING_NEXT: AtomicU64 = AtomicU64::new(0);
static ANOMALY_RING: [RingRec; ANOMALY_RING_LEN] =
    [const { RingRec::new() }; ANOMALY_RING_LEN];
static ANOMALY_NEXT: AtomicU64 = AtomicU64::new(0);

/// Is `v` the ring-mode value of `CRATONVM_DBG_ROOT_WRITE_AUDIT`? Pure.
#[inline]
fn is_ring_value(v: &str) -> bool {
    v.trim().eq_ignore_ascii_case("ring")
}

/// Whether the audit runs in RING mode: armed ([`enabled`]) with the value
/// `ring`. Latched, like [`enabled`]; the first read also registers the
/// at-exit dump.
pub fn ring_mode() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOT_WRITE_AUDIT")
            .is_some_and(|v| is_ring_value(&v.to_string_lossy()));
        if on {
            register_exit_dump();
        }
        on
    })
}

/// Register [`dump_ring`] with the C runtime's `atexit`, so a run that exits
/// normally (or through `std::process::exit`) prints the rings last.
fn register_exit_dump() {
    extern "C" {
        fn atexit(cb: extern "C" fn()) -> i32;
    }
    extern "C" fn dump_at_exit() {
        dump_ring("process exit");
    }
    // SAFETY: `atexit` stores a plain `extern "C" fn()` with no captured state,
    // valid for the life of the process; its return value only reports whether
    // the table had room, and a missed dump is not an error.
    let _ = unsafe { atexit(dump_at_exit) };
}

/// The `kind=` code of a ring record.
#[inline]
fn kind_code(kind: RootWriteKind) -> u64 {
    match kind {
        RootWriteKind::BlockedPeerNativeSlot => 1,
        RootWriteKind::JitFrameSlot => 2,
        RootWriteKind::JitFrameDuplicate => 3,
        RootWriteKind::JitRegisterImage => 4,
        RootWriteKind::ShadowStackHome => 5,
    }
}

/// [`kind_code`]'s inverse, as the line's `kind=` word.
fn kind_code_label(code: u64) -> &'static [u8] {
    let label: &'static str = match code {
        1 => RootWriteKind::BlockedPeerNativeSlot.label(),
        2 => RootWriteKind::JitFrameSlot.label(),
        3 => RootWriteKind::JitFrameDuplicate.label(),
        4 => RootWriteKind::JitRegisterImage.label(),
        5 => RootWriteKind::ShadowStackHome.label(),
        _ => "?",
    };
    label.as_bytes()
}

/// The `place=` code of a ring record.
#[inline]
fn place_code(place: SlotPlace) -> u64 {
    match place {
        SlotPlace::OwnLive => 1,
        SlotPlace::OwnBelowSp => 2,
        SlotPlace::Foreign => 3,
        SlotPlace::Unknown => 4,
    }
}

/// [`place_code`]'s inverse, as the line's `place=` word.
fn place_code_label(code: u64) -> &'static [u8] {
    let label: &'static str = match code {
        1 => SlotPlace::OwnLive.label(),
        2 => SlotPlace::OwnBelowSp.label(),
        3 => SlotPlace::Foreign.label(),
        4 => SlotPlace::Unknown.label(),
        _ => "?",
    };
    label.as_bytes()
}

/// The calling thread's OS tid, asked of the kernel once per thread.
fn cached_os_tid() -> u64 {
    thread_local! {
        static TID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    TID.try_with(|c| {
        let t = c.get();
        if t != 0 {
            t
        } else {
            let t = os_tid();
            c.set(t);
            t
        }
    })
    .unwrap_or_else(|_| os_tid())
}

/// Append one record to `ring`. `keep_first` makes the ring stop at its
/// capacity instead of wrapping.
#[inline]
fn ring_push(ring: &[RingRec], next: &AtomicU64, keep_first: bool, fields: [u64; 6]) {
    let seq = next.fetch_add(1, Ordering::Relaxed);
    let Ok(n) = usize::try_from(seq) else {
        return;
    };
    if keep_first && n >= ring.len() {
        return;
    }
    let r = &ring[n % ring.len()];
    r.seq.store(0, Ordering::Relaxed);
    r.tid.store(fields[0], Ordering::Relaxed);
    r.tag.store(fields[1], Ordering::Relaxed);
    r.slot.store(fields[2], Ordering::Relaxed);
    r.old.store(fields[3], Ordering::Relaxed);
    r.new.store(fields[4], Ordering::Relaxed);
    r.sp.store(fields[5], Ordering::Relaxed);
    r.seq.store(seq + 1, Ordering::Release);
}

/// Record one audited write-back (ring mode's replacement for the line).
fn ring_record(
    kind: RootWriteKind,
    place: SlotPlace,
    slot: usize,
    old: usize,
    new: usize,
    written: bool,
    sp: usize,
) {
    let tag = kind_code(kind) | (place_code(place) << 8) | (u64::from(written) << 16);
    let fields = [
        cached_os_tid(),
        tag,
        slot as u64,
        old as u64,
        new as u64,
        sp as u64,
    ];
    ring_push(&RING, &RING_NEXT, false, fields);
    if place != SlotPlace::OwnLive {
        ring_push(&ANOMALY_RING, &ANOMALY_NEXT, true, fields);
    }
}

/// A fixed-size line buffer: formatting without allocating, so the crash dump
/// can use it from a signal handler.
struct LineBuf {
    buf: [u8; 256],
    len: usize,
}

impl LineBuf {
    fn new() -> Self {
        Self {
            buf: [0u8; 256],
            len: 0,
        }
    }

    fn push(&mut self, s: &[u8]) {
        for &b in s {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
    }

    fn hex(&mut self, v: u64) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        self.push(b"0x");
        let mut started = false;
        for shift in (0..16).rev() {
            let d = ((v >> (shift * 4)) & 0xf) as usize;
            if d != 0 || started || shift == 0 {
                started = true;
                self.push(&[DIGITS[d]]);
            }
        }
    }

    fn dec(&mut self, mut v: u64) {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push(&tmp[i..]);
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// Read record `n` of `ring` whole, or `None` if it is not (or no longer)
/// record `n`.
fn ring_read(ring: &[RingRec], n: u64) -> Option<[u64; 6]> {
    let r = &ring[usize::try_from(n).ok()? % ring.len()];
    if r.seq.load(Ordering::Acquire) != n + 1 {
        return None;
    }
    let fields = [
        r.tid.load(Ordering::Relaxed),
        r.tag.load(Ordering::Relaxed),
        r.slot.load(Ordering::Relaxed),
        r.old.load(Ordering::Relaxed),
        r.new.load(Ordering::Relaxed),
        r.sp.load(Ordering::Relaxed),
    ];
    (r.seq.load(Ordering::Acquire) == n + 1).then_some(fields)
}

/// Format one record as a dump line.
fn format_record(line: &mut LineBuf, which: &[u8], n: u64, f: [u64; 6]) {
    line.push(b"[root-write-audit ring] ");
    line.push(which);
    line.push(b" #");
    line.dec(n);
    line.push(b" os_tid=");
    line.dec(f[0]);
    line.push(b" kind=");
    line.push(kind_code_label(f[1] & 0xff));
    line.push(b" slot=");
    line.hex(f[2]);
    line.push(b" old=");
    line.hex(f[3]);
    line.push(b" new=");
    line.hex(f[4]);
    line.push(b" place=");
    line.push(place_code_label((f[1] >> 8) & 0xff));
    let written: &[u8] = if (f[1] >> 16) & 1 != 0 {
        b" written=true"
    } else {
        b" written=false"
    };
    line.push(written);
    line.push(b" sp=");
    line.hex(f[5]);
    line.push(b"\n");
}

/// Emit both rings through `sink`, one line per call: a header, the anomaly
/// ring (first records, oldest first), then the ordinary ring (the retained
/// window, oldest first).
fn dump_rings_into(reason: &[u8], sink: &mut dyn FnMut(&[u8])) {
    let total = RING_NEXT.load(Ordering::Acquire);
    let anomalies = ANOMALY_NEXT.load(Ordering::Acquire);
    let mut head = LineBuf::new();
    head.push(b"[root-write-audit ring] dump (");
    head.push(reason);
    head.push(b"): ");
    head.dec(total);
    head.push(b" write-backs audited, last ");
    head.dec(total.min(RING_LEN as u64));
    head.push(b" kept; ");
    head.dec(anomalies);
    head.push(b" not own-live, first ");
    head.dec(anomalies.min(ANOMALY_RING_LEN as u64));
    head.push(b" kept\n");
    sink(head.bytes());
    for n in 0..anomalies.min(ANOMALY_RING_LEN as u64) {
        if let Some(f) = ring_read(&ANOMALY_RING, n) {
            let mut line = LineBuf::new();
            format_record(&mut line, b"anomaly", n, f);
            sink(line.bytes());
        }
    }
    for n in total.saturating_sub(RING_LEN as u64)..total {
        if let Some(f) = ring_read(&RING, n) {
            let mut line = LineBuf::new();
            format_record(&mut line, b"write", n, f);
            sink(line.bytes());
        }
    }
    sink(b"[root-write-audit ring] end of dump\n");
}

/// Print both rings on stderr. Ring mode's at-exit and pre-abort dump; a
/// no-op outside ring mode.
pub fn dump_ring(reason: &str) {
    if !ring_mode() {
        return;
    }
    use std::io::Write as _;
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    dump_rings_into(reason.as_bytes(), &mut |b| {
        let _ = out.write_all(b);
    });
    let _ = out.flush();
}

/// Print both rings on file descriptor 2 with `write(2)`: no allocation, no
/// lock, no `std::io` -- for a crash handler, which may run while the crashing
/// thread holds the stderr lock or the allocator's. A no-op unless the rings
/// hold something (it never reads the environment: that is not
/// async-signal-safe).
///
/// Cross-lane hook: `vm/src/runtime/crash_handler.rs`'s fatal-signal report
/// calls this after its own header lines.
pub fn dump_ring_for_crash() {
    if !rings_hold_records() {
        return;
    }
    dump_rings_into(b"fatal signal", &mut |b| raw_stderr_write(b));
}

/// Whether either ring recorded anything. Only ring mode records, so this is
/// `false` on every run without it; it reads two atomics and nothing else.
fn rings_hold_records() -> bool {
    RING_NEXT.load(Ordering::Relaxed) != 0 || ANOMALY_NEXT.load(Ordering::Relaxed) != 0
}

#[cfg(unix)]
fn raw_stderr_write(mut b: &[u8]) {
    extern "C" {
        fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    }
    while !b.is_empty() {
        // SAFETY: `b` is a live byte slice; `write` reads at most `b.len()`
        // bytes from it and touches nothing else. Async-signal-safe (POSIX).
        let n = unsafe { write(2, b.as_ptr(), b.len()) };
        if n <= 0 {
            return;
        }
        b = &b[(n as usize).min(b.len())..];
    }
}

#[cfg(not(unix))]
fn raw_stderr_write(b: &[u8]) {
    use std::io::Write as _;
    let _ = std::io::stderr().write_all(b);
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn os_tid() -> u64 {
    extern "C" {
        fn syscall(num: std::ffi::c_long, ...) -> std::ffi::c_long;
    }
    #[cfg(target_arch = "x86_64")]
    const SYS_GETTID: std::ffi::c_long = 186;
    #[cfg(target_arch = "aarch64")]
    const SYS_GETTID: std::ffi::c_long = 178;
    // SAFETY: `gettid` takes no arguments, cannot fail, and has no effect.
    (unsafe { syscall(SYS_GETTID) }) as u64
}

#[cfg(windows)]
fn os_tid() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    // SAFETY: a Win32 API with no arguments and no effect.
    u64::from(unsafe { GetCurrentThreadId() })
}

#[cfg(not(any(
    windows,
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
)))]
fn os_tid() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_place_rule_names_live_dead_and_foreign_words() {
        let (lo, hi, sp) = (0x1000usize, 0x9000usize, 0x4000usize);
        assert_eq!(place_in(0x4000, lo, hi, sp), SlotPlace::OwnLive);
        assert_eq!(place_in(0x8ff8, lo, hi, sp), SlotPlace::OwnLive);
        assert_eq!(place_in(0x3ff8, lo, hi, sp), SlotPlace::OwnBelowSp);
        assert_eq!(place_in(0x1000, lo, hi, sp), SlotPlace::OwnBelowSp);
        assert_eq!(place_in(0x9000, lo, hi, sp), SlotPlace::Foreign);
        assert_eq!(place_in(0x0ff8, lo, hi, sp), SlotPlace::Foreign);
        // A word straddling the top is not inside the stack.
        assert_eq!(place_in(0x8ffc, lo, hi, sp), SlotPlace::Foreign);
        // A stack pointer outside the band (an alternate signal stack): no
        // word can be called dead.
        assert_eq!(place_in(0x2000, lo, hi, 0xf000), SlotPlace::OwnLive);
    }

    #[test]
    fn a_local_of_the_caller_is_own_live_and_a_heap_word_is_not() {
        let local = 0usize;
        let addr = std::hint::black_box(&local) as *const usize as usize;
        let place = classify_slot(addr);
        assert!(
            matches!(place, SlotPlace::OwnLive | SlotPlace::Unknown),
            "a local of the calling frame classified {place:?}"
        );
        let boxed = Box::new(0usize);
        let heap = &*boxed as *const usize as usize;
        let place = classify_slot(heap);
        assert!(
            matches!(place, SlotPlace::Foreign | SlotPlace::Unknown),
            "a heap word classified {place:?}"
        );
    }

    /// gen r5w2/roots6: the heap-write rule. A range inside one region passes,
    /// one that straddles two adjacent regions or leaves them does not, and a
    /// range whose end overflows never passes.
    #[test]
    fn a_bulk_write_passes_only_inside_one_region() {
        let regions = [(0x1000usize, 0x2000usize), (0x2000, 0x3000)];
        assert!(span_inside_one(0x1000, 0x1000, &regions));
        assert!(span_inside_one(0x2ff8, 8, &regions));
        assert!(span_inside_one(0x1800, 0, &regions));
        assert!(
            !span_inside_one(0x1ff8, 16, &regions),
            "straddles two regions"
        );
        assert!(
            !span_inside_one(0x3000, 8, &regions),
            "one past the last region"
        );
        assert!(!span_inside_one(0x0ff8, 8, &regions), "below every region");
        assert!(
            !span_inside_one(usize::MAX - 4, 8, &regions),
            "overflowing end"
        );
        assert!(
            !span_inside_one(0x1000, 8, &[(0x2000, 0x1000)]),
            "an empty region"
        );
        assert!(!span_inside_one(0x1000, 8, &[]));
    }

    /// gen r5w2/roots6: an unchanged map passes every checkpoint, including
    /// one reached after reads through it.
    #[test]
    fn a_map_nobody_writes_passes_every_checkpoint() {
        let mut map = cratonvm_types::PointerMap::default();
        for k in 0..64usize {
            map.insert(0x1000 + k * 8, 0x9000 + k * 8);
        }
        let mut watch = MapWatch::arm(&map, "armed");
        assert_eq!(map.get(&0x1008), Some(&0x9008));
        watch.check(&map, "after a read");
        watch.check(&map, "again");
        assert_eq!(watch.site, "again");
    }

    #[test]
    fn every_heap_write_kind_has_a_distinct_label() {
        let kinds = [
            HeapWriteKind::EvacuationCopy,
            HeapWriteKind::InPlaceZero,
            HeapWriteKind::InPlaceFiller,
            HeapWriteKind::PromotionCopy,
            HeapWriteKind::SweepHeaderWrite,
            HeapWriteKind::SweepStretchRewrite,
            // gen r5w4/pin8: the quarantine stamp.
            HeapWriteKind::InPlaceQuarantine,
        ];
        let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), kinds.len());
    }

    /// gen r5w4/pin8: only the value `ring` (any case, trimmed) selects ring
    /// mode; the token's plain arming (`1`) keeps the printing audit.
    #[test]
    fn only_the_ring_value_selects_ring_mode() {
        assert!(is_ring_value("ring"));
        assert!(is_ring_value(" RING\n"));
        assert!(!is_ring_value("1"));
        assert!(!is_ring_value(""));
        assert!(!is_ring_value("rings"));
    }

    /// gen r5w4/pin8: the allocation-free line formatter the crash dump uses.
    #[test]
    fn the_ring_line_formatter_writes_hex_decimal_and_labels() {
        let mut line = LineBuf::new();
        line.hex(0);
        line.push(b" ");
        line.hex(0x7f00_dead_beef);
        line.push(b" ");
        line.dec(0);
        line.push(b" ");
        line.dec(18_446_744_073_709_551_615);
        assert_eq!(
            line.bytes(),
            b"0x0 0x7f00deadbeef 0 18446744073709551615".as_slice()
        );
        let tag = kind_code(RootWriteKind::JitFrameSlot)
            | (place_code(SlotPlace::Foreign) << 8)
            | (1 << 16);
        let mut rec = LineBuf::new();
        format_record(&mut rec, b"write", 7, [42, tag, 0x1000, 0x2000, 0x3000, 0x4000]);
        assert_eq!(
            std::str::from_utf8(rec.bytes()).unwrap(),
            "[root-write-audit ring] write #7 os_tid=42 kind=jit-frame-slot slot=0x1000 \
             old=0x2000 new=0x3000 place=foreign written=true sp=0x4000\n"
        );
        // A line longer than the buffer is cut, never overrun.
        let mut long = LineBuf::new();
        for _ in 0..100 {
            long.push(b"0123456789");
        }
        assert_eq!(long.bytes().len(), 256);
    }

    /// gen r5w4/pin8: a ring keeps the last records (or, `keep_first`, the
    /// first ones), and a slot overwritten by a later record no longer reads
    /// as the earlier one.
    #[test]
    fn a_ring_keeps_the_window_it_promises() {
        let ring: Vec<RingRec> = (0..4).map(|_| RingRec::new()).collect();
        let next = AtomicU64::new(0);
        for i in 0..6u64 {
            ring_push(&ring, &next, false, [i, 0, 0, 0, 0, 0]);
        }
        assert!(ring_read(&ring, 0).is_none(), "record 0 was overwritten by record 4");
        assert!(ring_read(&ring, 1).is_none(), "record 1 was overwritten by record 5");
        assert_eq!(ring_read(&ring, 5).map(|f| f[0]), Some(5));
        assert_eq!(ring_read(&ring, 2).map(|f| f[0]), Some(2));

        let first: Vec<RingRec> = (0..2).map(|_| RingRec::new()).collect();
        let next_first = AtomicU64::new(0);
        for i in 0..5u64 {
            ring_push(&first, &next_first, true, [i, 0, 0, 0, 0, 0]);
        }
        assert_eq!(ring_read(&first, 0).map(|f| f[0]), Some(0));
        assert_eq!(ring_read(&first, 1).map(|f| f[0]), Some(1));
        assert!(ring_read(&first, 2).is_none(), "a keep-first ring stops at capacity");
    }

    #[test]
    fn every_kind_and_place_has_a_distinct_label() {
        let kinds = [
            RootWriteKind::BlockedPeerNativeSlot,
            RootWriteKind::JitFrameSlot,
            RootWriteKind::JitFrameDuplicate,
            RootWriteKind::JitRegisterImage,
            RootWriteKind::ShadowStackHome,
        ];
        let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), kinds.len());
        let places = [
            SlotPlace::OwnLive,
            SlotPlace::OwnBelowSp,
            SlotPlace::Foreign,
            SlotPlace::Unknown,
        ];
        let mut labels: Vec<&str> = places.iter().map(|p| p.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), places.len());
    }
}
