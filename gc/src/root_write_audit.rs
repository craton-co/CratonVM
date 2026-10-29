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
//! `docs/feature-designs/generative-gc-architecture.md`.
//! That page records native words (an `Arc` pointer, hashbrown control bytes,
//! a cached `OnceLock<bool>`, mimalloc's thread-local heap pointer) overwritten
//! at an unpredictable moment from an unpredictable thread, and asks for the
//! write-back that did it. Every writer above runs on the thread whose stack
//! the word should belong to, so the one question this audit asks of each
//! store is: does the target word lie in the WRITING thread's own live stack?
//!
//! "Live" is the writer's GUARD: each writer bounds its stores by a band of
//! the calling thread's stack (`apply_native_slot_fixups`' wake band,
//! `jit_remap_store_band`, `ShadowStack::remap_in`'s owner band) and passes
//! that band to [`note_write`], so the verdict and the store decision are one
//! rule (gcj-j1pn-raw-root-write-owner-findings item 3: the audit used to
//! classify against its own, deeper stack pointer, and a store the guard
//! refused below its bound logged `place=own-live written=false`).
//!
//! * `own-live`: inside the writer's guard band, i.e. in a frame the guard
//!   says is still running. The expected answer for every kind.
//! * `own-below-sp`: in the writer's stack but below the guard's lower bound,
//!   i.e. a frame that has returned (or, for the wake write-back, one that did
//!   not exist when the word was captured). Harmless to the program, and a
//!   sign the slot was recorded for a frame that has since returned.
//! * `foreign`: not in the writer's stack at all -- another thread's stack,
//!   thread-local storage, the heap of the allocator -- or above the guard's
//!   top (the static TLS block the band is clamped below). A store there is
//!   the corruption the page describes.
//! * `unknown`: the platform cannot say where the stack is.
//!
//! Each audited store prints one line on stderr, rate-limited (the first
//! [`ORDINARY_LINE_BUDGET`] `own-live` stores, and separately the first
//! [`ANOMALY_LINE_BUDGET`] of every other verdict, so a flood of ordinary
//! stores cannot hide the one that matters):
//!
//! ```text
//! [root-write-audit] vm=<id> os_tid=<tid> kind=<kind> slot=<addr> old=<value> new=<value> place=<verdict> written=<bool> stack=[<lo>,<hi>) guard=[<lo>,<hi>)
//! ```
//!
//! `written=false` is a store the caller DECLINED (for the native-stack
//! write-back: a word outside the own stack, which the gen r5w1/crash5 guard
//! refuses, or a word that no longer reads its captured value). To match a
//! crash, compare the crash report's faulting value and address against the
//! `slot=` / `new=` pairs.
//!
//! `vm=` is the writing VM's `vm_identity` ([`VmTagScope`]: the collection
//! pause and the blocked-region wake, the two places a raw write-back runs,
//! open one), `0` for a store made outside both. The budgets, rings and
//! counters are process-wide on purpose -- one dump per process, at exit or
//! at a crash -- so two co-resident VMs share them; the tag is what separates
//! their records.
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
//! The dump's lines are `[root-write-audit ring] #<seq> vm=.. os_tid=.. kind=..
//! slot=.. old=.. new=.. place=.. written=.. guard=[..,..)`: to match a crash,
//! grep the crashing thread's `os_tid` and the corrupted word's address
//! (`slot=`) or value (`new=`).
//!
//! The capture side ([`note_band_overrun`], [`note_foreign_capture`], the
//! registry's declined-stale-claim line) prints nothing in ring mode either;
//! its counts stay in [`capture_census`].
//!
//! Any number of threads may write the rings (gcj-j1pn-raw-root-write-owner-
//! findings item 7): a writer CLAIMS a record before it writes it
//! ([`ring_push`]), so two writers a whole ring apart can no longer interleave
//! their fields into one record that passes the reader's sequence check. A
//! writer that finds the record claimed by another drops its own record
//! (counted in the dump header) rather than wait: the crash handler reads the
//! rings too, and nothing here may block.

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
///
/// gci i3/fl: by value (`runtime_flag_on`), not presence -- `=0` / `=off`
/// leave the audit off, `=1` / `=ring` arm it ([`ring_mode`] still asks for
/// the `ring` word on top).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ROOT_WRITE_AUDIT"))
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

/// The verdict for `slot` against a writer's GUARD band `guard` inside the
/// calling thread's stack `stack` (`None`: the platform cannot say). Pure.
///
/// gcj-j1pn-raw-root-write-owner-findings item 3: the same band the writer's
/// store decision used, so a store the guard admits is `own-live` and one it
/// refuses below its lower bound is `own-below-sp` -- never the contradictory
/// `own-live written=false` the old, probe-relative rule printed for a word
/// between the guard's bound and the audit's own (deeper) stack pointer.
#[inline]
fn place_against_guard(slot: usize, stack: Option<SlotBand>, guard: SlotBand) -> SlotPlace {
    let Some(stack) = stack else {
        return SlotPlace::Unknown;
    };
    if !stack.contains_slot(slot) {
        SlotPlace::Foreign
    } else if guard.contains_slot(slot) {
        SlotPlace::OwnLive
    } else if slot < guard.lo {
        SlotPlace::OwnBelowSp
    } else {
        // Inside the reported stack but at or above the guard's top: the
        // static TLS block `SlotBand::current_thread` clamps below.
        SlotPlace::Foreign
    }
}

/// What a line budget says about the `n`-th store (0-based) of its verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BudgetVerdict {
    /// Print the store's line.
    Print,
    /// The budget just ran out: print the one "exhausted" line.
    Exhausted,
    /// Counted, not printed.
    Silent,
}

/// The line budget's rule, pure (gcj-j1pn-raw-root-write-owner-findings item
/// 7: the cut-over had no test).
#[inline]
fn budget_verdict(n: u64, cap: u64) -> BudgetVerdict {
    match n.cmp(&cap) {
        std::cmp::Ordering::Less => BudgetVerdict::Print,
        std::cmp::Ordering::Equal => BudgetVerdict::Exhausted,
        std::cmp::Ordering::Greater => BudgetVerdict::Silent,
    }
}

/// Audit one raw write-back of `new` over `old` at `slot`, made under the
/// writer's own store bound `guard` (a band of the calling thread's stack;
/// see the module docs). `written` says whether the caller made (or is about
/// to make) the store; `false` is a store it declined. Call only when
/// [`enabled`]; the hook sites test it first so the disarmed cost stays one
/// load.
#[cold]
#[inline(never)]
pub fn note_write(
    kind: RootWriteKind,
    guard: SlotBand,
    slot: usize,
    old: usize,
    new: usize,
    written: bool,
) {
    let stack = crate::shadow_stack::current_thread_stack();
    let place = place_against_guard(slot, stack, guard);
    let (lo, hi) = stack.map_or((0, 0), |s| (s.lo, s.hi));
    let idx = match place {
        SlotPlace::OwnLive => 0,
        SlotPlace::OwnBelowSp => 1,
        SlotPlace::Foreign => 2,
        SlotPlace::Unknown => 3,
    };
    AUDITED[idx].fetch_add(1, Ordering::Relaxed);
    // gen r5w4/pin8: ring mode records the store and prints nothing.
    if ring_mode() {
        ring_record(kind, place, slot, old, new, written, guard);
        return;
    }
    let (budget, cap) = if place == SlotPlace::OwnLive {
        (&ORDINARY_LINES, ORDINARY_LINE_BUDGET)
    } else {
        (&ANOMALY_LINES, ANOMALY_LINE_BUDGET)
    };
    match budget_verdict(budget.fetch_add(1, Ordering::Relaxed), cap) {
        BudgetVerdict::Print => eprintln!(
            "[root-write-audit] vm={} os_tid={} kind={} slot={slot:#x} old={old:#x} \
             new={new:#x} place={} written={written} stack=[{lo:#x},{hi:#x}) \
             guard=[{:#x},{:#x})",
            current_vm_tag(),
            os_tid(),
            kind.label(),
            place.label(),
            guard.lo,
            guard.hi,
        ),
        BudgetVerdict::Exhausted => eprintln!(
            "[root-write-audit] line budget for place={} exhausted after {cap} lines; \
             later stores of that verdict are counted, not printed",
            if place == SlotPlace::OwnLive {
                "own-live"
            } else {
                "own-below-sp/foreign/unknown"
            }
        ),
        BudgetVerdict::Silent => {}
    }
}

thread_local! {
    /// The `vm_identity` of the VM whose pause or wake this thread is running
    /// ([`VmTagScope`]); `0` outside both.
    static VM_TAG: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The calling thread's [`VmTagScope`] tag, `0` for none.
#[inline]
fn current_vm_tag() -> u64 {
    VM_TAG.try_with(|c| c.get()).unwrap_or(0)
}

/// gcj-j1pn-raw-root-write-owner-findings item 7: tags every audit line and
/// ring record this thread makes, for the scope's life, with the VM it is
/// working for, so co-resident VMs' records can be told apart. Opened by the
/// collection pause (`gc_and_alloc::run_collection_pause`'s caller) and the
/// blocked-region wake (`vm_exec::apply_native_slot_fixups`' callers); a
/// no-op unless the audit is [`enabled`]. Nests; restores the outer tag.
#[must_use = "the tag ends when the guard drops"]
pub struct VmTagScope(Option<u64>);

impl VmTagScope {
    /// Tag this thread's records with `vm` until the guard drops.
    #[inline]
    pub fn enter(vm: u64) -> Self {
        if !enabled() {
            return VmTagScope(None);
        }
        VmTagScope(VM_TAG.try_with(|c| c.replace(vm)).ok())
    }
}

impl Drop for VmTagScope {
    fn drop(&mut self) {
        if let Some(prev) = self.0 {
            let _ = VM_TAG.try_with(|c| c.set(prev));
        }
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
    // gcj j2/pn2: ring mode prints nothing (`capture_census` keeps the count);
    // a capture-side line changes the timing ring mode exists to keep.
    if ring_mode() {
        return;
    }
    if CAPTURE_LINES.fetch_add(1, Ordering::Relaxed) < ANOMALY_LINE_BUDGET {
        eprintln!(
            "[root-write-audit] vm={} helper-window band of peer os_tid={peer_os_tid} ran past \
             its own stack: band=[{rsp:#x},{band_end:#x}) own=[{:#x},{:#x}) overrun={} bytes",
            current_vm_tag(),
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
    // gcj j2/pn2: silent in ring mode, as `note_band_overrun`.
    if ring_mode() {
        return;
    }
    if CAPTURE_LINES.fetch_add(1, Ordering::Relaxed) < ANOMALY_LINE_BUDGET {
        eprintln!(
            "[root-write-audit] vm={} capture for peer os_tid={peer_os_tid} at {addr:#x} (value \
             {value:#x}) lies outside its own stack [{:#x},{:#x})",
            current_vm_tag(),
            own.lo,
            own.hi,
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

/// Fields of one ring record, in [`RingRec::fields`] order: the OS tid, the
/// tag word, the slot, the old and new values, the guard band's `lo` and
/// `hi`, and the VM tag.
const RING_FIELDS: usize = 8;

/// A record's `seq` while a writer holds it ([`ring_push`]). Never `n + 1`
/// for a real record `n`, so a reader rejects a record being written.
const RING_SEQ_CLAIMED: u64 = u64::MAX;

/// One ring record. `seq` is `0` while the record is unwritten,
/// [`RING_SEQ_CLAIMED`] while a writer holds it, and `n + 1` once record `n`
/// is complete, so a reader that sees the same `n + 1` before and after
/// reading the fields read one whole record.
struct RingRec {
    seq: AtomicU64,
    /// `[os tid, tag, slot, old, new, guard lo, guard hi, vm]`. The tag's bits
    /// 0..8: the kind code ([`kind_code`]); 8..16: the place code
    /// ([`place_code`]); bit 16: `written`.
    fields: [AtomicU64; RING_FIELDS],
}

impl RingRec {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            fields: [const { AtomicU64::new(0) }; RING_FIELDS],
        }
    }
}

static RING: [RingRec; RING_LEN] = [const { RingRec::new() }; RING_LEN];
static RING_NEXT: AtomicU64 = AtomicU64::new(0);
static ANOMALY_RING: [RingRec; ANOMALY_RING_LEN] = [const { RingRec::new() }; ANOMALY_RING_LEN];
static ANOMALY_NEXT: AtomicU64 = AtomicU64::new(0);
/// Records a writer dropped because another writer held the record
/// ([`ring_push`]); printed in the dump header.
static RING_DROPPED: AtomicU64 = AtomicU64::new(0);

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
/// capacity instead of wrapping. Returns whether the record was stored.
///
/// gcj-j1pn-raw-root-write-owner-findings item 7: safe for any number of
/// writers. The ring used to be single-writer by contract: two writers whose
/// sequence numbers were a whole ring apart wrote the same record at once,
/// each storing `seq = 0`, its fields and its `seq`, so the record could end
/// up holding one writer's `seq` over a mix of both writers' fields -- a torn
/// record the reader's check accepts. A writer now CLAIMS the record first
/// (`seq: old -> RING_SEQ_CLAIMED`, a compare-exchange), and drops its own
/// record instead when another writer holds it or a NEWER record is already
/// there. Dropping, not waiting: a crash handler reads the rings, and the
/// writer may be the thread it interrupted.
#[inline]
fn ring_push(
    ring: &[RingRec],
    next: &AtomicU64,
    keep_first: bool,
    fields: [u64; RING_FIELDS],
) -> bool {
    let seq = next.fetch_add(1, Ordering::Relaxed);
    let Ok(n) = usize::try_from(seq) else {
        return false;
    };
    if keep_first && n >= ring.len() {
        return false;
    }
    let r = &ring[n % ring.len()];
    let held = r.seq.load(Ordering::Relaxed);
    // `held` is `0` (unwritten) or an older record's `m + 1 <= seq`; anything
    // else is a writer holding it, or a newer record that must not be lost.
    if (held == RING_SEQ_CLAIMED || held > seq)
        || r
            .seq
            .compare_exchange(held, RING_SEQ_CLAIMED, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        RING_DROPPED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // gci i45/bh: the seqlock's writer fence. Without it the relaxed field
    // stores below may become visible BEFORE the claim above, so a reader
    // could see the previous record's `seq`, then this record's fields, then
    // the previous `seq` again, and accept a torn record (see `ring_read`'s
    // reader fence, the other half).
    std::sync::atomic::fence(Ordering::Release);
    for (cell, v) in r.fields.iter().zip(fields) {
        cell.store(v, Ordering::Relaxed);
    }
    r.seq.store(seq + 1, Ordering::Release);
    true
}

/// Record one audited write-back (ring mode's replacement for the line).
fn ring_record(
    kind: RootWriteKind,
    place: SlotPlace,
    slot: usize,
    old: usize,
    new: usize,
    written: bool,
    guard: SlotBand,
) {
    let tag = kind_code(kind) | (place_code(place) << 8) | (u64::from(written) << 16);
    let fields = [
        cached_os_tid(),
        tag,
        slot as u64,
        old as u64,
        new as u64,
        guard.lo as u64,
        guard.hi as u64,
        current_vm_tag(),
    ];
    ring_push(&RING, &RING_NEXT, false, fields);
    if place != SlotPlace::OwnLive {
        ring_push(&ANOMALY_RING, &ANOMALY_NEXT, true, fields);
    }
}

/// Bytes of one [`LineBuf`]: the longest record line (two 16-digit guard
/// bounds, the longest kind and place labels) with room to spare.
const LINE_BUF_LEN: usize = 320;

/// A fixed-size line buffer: formatting without allocating, so the crash dump
/// can use it from a signal handler.
struct LineBuf {
    buf: [u8; LINE_BUF_LEN],
    len: usize,
}

impl LineBuf {
    fn new() -> Self {
        Self {
            buf: [0u8; LINE_BUF_LEN],
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
fn ring_read(ring: &[RingRec], n: u64) -> Option<[u64; RING_FIELDS]> {
    let r = &ring[usize::try_from(n).ok()? % ring.len()];
    if r.seq.load(Ordering::Acquire) != n + 1 {
        return None;
    }
    let mut fields = [0u64; RING_FIELDS];
    for (v, cell) in fields.iter_mut().zip(&r.fields) {
        *v = cell.load(Ordering::Relaxed);
    }
    // gci i45/bh: the seqlock's reader fence. An `Acquire` LOAD of `seq` here
    // orders only what comes after it; the field loads above could still be
    // satisfied after it, i.e. after a writer had begun overwriting the
    // record. The fence keeps them before the re-check, so an unchanged `seq`
    // proves no writer touched the fields in between (with the writer fence
    // in `ring_push`).
    std::sync::atomic::fence(Ordering::Acquire);
    (r.seq.load(Ordering::Relaxed) == n + 1).then_some(fields)
}

/// Format one record as a dump line.
fn format_record(line: &mut LineBuf, which: &[u8], n: u64, f: [u64; RING_FIELDS]) {
    line.push(b"[root-write-audit ring] ");
    line.push(which);
    line.push(b" #");
    line.dec(n);
    line.push(b" vm=");
    line.dec(f[7]);
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
    line.push(b" guard=[");
    line.hex(f[5]);
    line.push(b",");
    line.hex(f[6]);
    line.push(b")\n");
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
    head.push(b" kept; ");
    head.dec(RING_DROPPED.load(Ordering::Relaxed));
    head.push(b" dropped to a concurrent writer\n");
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

/// gcj j2/pn2 (`gcj-j1pn-raw-root-write-owner-findings-FIXED-20261025` item 6): the
/// crash dump's write on Windows goes straight to `WriteFile` on the standard
/// error handle. `std::io::stderr().write_all` takes the stderr lock, so a
/// thread that crashed INSIDE an `eprintln!` (it holds the lock) deadlocked
/// its own dump.
#[cfg(windows)]
fn raw_stderr_write(mut b: &[u8]) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
        fn WriteFile(
            h_file: *mut std::ffi::c_void,
            lp_buffer: *const u8,
            n_number_of_bytes_to_write: u32,
            lp_number_of_bytes_written: *mut u32,
            lp_overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    // `STD_ERROR_HANDLE` is `(DWORD)-12`.
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    // SAFETY: a Win32 API that takes a constant and only reads process state.
    let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    // `INVALID_HANDLE_VALUE` is all ones; a null handle means no stderr.
    if handle.is_null() || handle as usize == usize::MAX {
        return;
    }
    while !b.is_empty() {
        let chunk = u32::try_from(b.len()).unwrap_or(u32::MAX);
        let mut written: u32 = 0;
        // SAFETY: `b` is a live byte slice of at least `chunk` bytes; WriteFile
        // reads at most `chunk` bytes from it and writes the count into the
        // local `written`; a null `lpOverlapped` is a synchronous write. It
        // takes no Rust lock and does not allocate.
        let ok = unsafe {
            WriteFile(
                handle,
                b.as_ptr(),
                chunk,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            return;
        }
        b = &b[(written as usize).min(b.len())..];
    }
}

#[cfg(not(any(unix, windows)))]
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
        format_record(
            &mut rec,
            b"write",
            7,
            [42, tag, 0x1000, 0x2000, 0x3000, 0x4000, 0x5000, 3],
        );
        assert_eq!(
            std::str::from_utf8(rec.bytes()).unwrap(),
            "[root-write-audit ring] write #7 vm=3 os_tid=42 kind=jit-frame-slot slot=0x1000 \
             old=0x2000 new=0x3000 place=foreign written=true guard=[0x4000,0x5000)\n"
        );
        // gcj j1pn item 7: the longest record a dump can print fits whole.
        let tag = kind_code(RootWriteKind::BlockedPeerNativeSlot)
            | (place_code(SlotPlace::OwnBelowSp) << 8);
        let mut widest = LineBuf::new();
        let mut wide = [u64::MAX; RING_FIELDS];
        wide[1] = tag;
        format_record(&mut widest, b"anomaly", u64::MAX, wide);
        assert!(widest.bytes().ends_with(b")\n"), "the widest record line was cut");
        // A line longer than the buffer is cut, never overrun.
        let mut long = LineBuf::new();
        for _ in 0..100 {
            long.push(b"0123456789");
        }
        assert_eq!(long.bytes().len(), LINE_BUF_LEN);
    }

    /// gen r5w4/pin8: a ring keeps the last records (or, `keep_first`, the
    /// first ones), and a slot overwritten by a later record no longer reads
    /// as the earlier one.
    #[test]
    fn a_ring_keeps_the_window_it_promises() {
        let ring: Vec<RingRec> = (0..4).map(|_| RingRec::new()).collect();
        let next = AtomicU64::new(0);
        for i in 0..6u64 {
            ring_push(&ring, &next, false, [i, 0, 0, 0, 0, 0, 0, 0]);
        }
        assert!(
            ring_read(&ring, 0).is_none(),
            "record 0 was overwritten by record 4"
        );
        assert!(
            ring_read(&ring, 1).is_none(),
            "record 1 was overwritten by record 5"
        );
        assert_eq!(ring_read(&ring, 5).map(|f| f[0]), Some(5));
        assert_eq!(ring_read(&ring, 2).map(|f| f[0]), Some(2));

        let first: Vec<RingRec> = (0..2).map(|_| RingRec::new()).collect();
        let next_first = AtomicU64::new(0);
        for i in 0..5u64 {
            ring_push(&first, &next_first, true, [i, 0, 0, 0, 0, 0, 0, 0]);
        }
        assert_eq!(ring_read(&first, 0).map(|f| f[0]), Some(0));
        assert_eq!(ring_read(&first, 1).map(|f| f[0]), Some(1));
        assert!(
            ring_read(&first, 2).is_none(),
            "a keep-first ring stops at capacity"
        );
    }

    /// gci i45/bh: a record `ring_read` accepts is one whole record, while a
    /// writer keeps overwriting the ring (several writers:
    /// `gcj1pn_two_writers_never_leave_a_torn_record`). Every field of record
    /// `n` is `n`, so a torn read shows as
    /// a field that is not its record's number. The fences this pins are
    /// no-ops for the compiler-visible order on x86-64, so the test is a
    /// regression guard for the invariant rather than a reproducer.
    #[test]
    fn bh45_an_accepted_ring_record_is_never_torn() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        const LEN: usize = 8;
        const RECORDS: u64 = 200_000;
        let ring: Arc<Vec<RingRec>> = Arc::new((0..LEN).map(|_| RingRec::new()).collect());
        let next = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let (ring, next, done) = (Arc::clone(&ring), Arc::clone(&next), Arc::clone(&done));
            std::thread::spawn(move || {
                for n in 0..RECORDS {
                    ring_push(&ring, &next, false, [n; RING_FIELDS]);
                }
                done.store(true, Ordering::Release);
            })
        };
        while !done.load(Ordering::Acquire) {
            let top = next.load(Ordering::Relaxed);
            for n in top.saturating_sub(LEN as u64)..top {
                if let Some(f) = ring_read(&ring, n) {
                    assert!(f.iter().all(|&v| v == n), "record {n} read torn: {f:?}");
                }
            }
        }
        writer.join().expect("writer");
        for n in RECORDS - LEN as u64..RECORDS {
            assert_eq!(ring_read(&ring, n), Some([n; RING_FIELDS]), "the last window is whole");
        }
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

    /// gcj-j1pn-raw-root-write-owner-findings item 3: the verdict is the
    /// writer's own guard. A word the guard admits is `own-live`; one in the
    /// stack below the guard's lower bound -- including a word ABOVE the
    /// audit's own stack pointer, the case that used to print
    /// `own-live written=false` -- is `own-below-sp`; one above the guard's
    /// top (the TLS clamp) or outside the stack is `foreign`.
    #[test]
    fn gcj1pn_the_verdict_follows_the_writers_guard() {
        let stack = Some(SlotBand::new(0x1000, 0x9000));
        let guard = SlotBand::new(0x4000, 0x8000);
        assert_eq!(place_against_guard(0x4000, stack, guard), SlotPlace::OwnLive);
        assert_eq!(place_against_guard(0x7ff8, stack, guard), SlotPlace::OwnLive);
        assert_eq!(place_against_guard(0x3ff8, stack, guard), SlotPlace::OwnBelowSp);
        assert_eq!(place_against_guard(0x1000, stack, guard), SlotPlace::OwnBelowSp);
        assert_eq!(place_against_guard(0x8000, stack, guard), SlotPlace::Foreign);
        assert_eq!(place_against_guard(0x9000, stack, guard), SlotPlace::Foreign);
        assert_eq!(place_against_guard(0x0ff8, stack, guard), SlotPlace::Foreign);
        assert_eq!(place_against_guard(0x4000, None, guard), SlotPlace::Unknown);
        // The real thing: a word between this test's frame and a guard set
        // ABOVE it (a store the guard refuses) is below the guard, whatever
        // the audit's own deeper probe would have said.
        let local = 0usize;
        let addr = std::hint::black_box(&local) as *const usize as usize;
        if let Some(stack) = crate::shadow_stack::current_thread_stack() {
            if stack.contains_slot(addr) {
                let above = SlotBand::new(addr + 8, stack.hi);
                assert_eq!(
                    place_against_guard(addr, Some(stack), above),
                    SlotPlace::OwnBelowSp
                );
                let around = SlotBand::new(addr, stack.hi);
                assert_eq!(place_against_guard(addr, Some(stack), around), SlotPlace::OwnLive);
            }
        }
    }

    /// gcj-j1pn-raw-root-write-owner-findings item 7: the budget's cut-over.
    /// The first `cap` stores print, the next prints the one "exhausted"
    /// line, every later one is only counted.
    #[test]
    fn gcj1pn_the_line_budget_cuts_over_once() {
        assert_eq!(budget_verdict(0, 3), BudgetVerdict::Print);
        assert_eq!(budget_verdict(2, 3), BudgetVerdict::Print);
        assert_eq!(budget_verdict(3, 3), BudgetVerdict::Exhausted);
        assert_eq!(budget_verdict(4, 3), BudgetVerdict::Silent);
        assert_eq!(budget_verdict(u64::MAX, 3), BudgetVerdict::Silent);
        assert_eq!(budget_verdict(0, 0), BudgetVerdict::Exhausted);
        let printed = (0..ORDINARY_LINE_BUDGET + 10)
            .filter(|&n| budget_verdict(n, ORDINARY_LINE_BUDGET) == BudgetVerdict::Print)
            .count() as u64;
        assert_eq!(printed, ORDINARY_LINE_BUDGET);
    }

    /// gcj-j1pn-raw-root-write-owner-findings item 7: two writers whose
    /// sequence numbers collide on one record (a ring of ONE record, so every
    /// pair collides) never leave a record the reader accepts with fields of
    /// two writers. Writer `w`'s record `n` holds `n` in every field but the
    /// last, which holds `w`; a torn record shows as a field that is not its
    /// record's number. A writer that loses the claim drops its record.
    #[test]
    fn gcj1pn_two_writers_never_leave_a_torn_record() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        const PER_WRITER: u64 = 100_000;
        let ring: Arc<Vec<RingRec>> = Arc::new(vec![RingRec::new()]);
        let next = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = (0..2u64)
            .map(|w| {
                let (ring, next) = (Arc::clone(&ring), Arc::clone(&next));
                std::thread::spawn(move || {
                    let mut stored = 0u64;
                    for _ in 0..PER_WRITER {
                        let n = next.load(Ordering::Relaxed);
                        let mut f = [n; RING_FIELDS];
                        f[RING_FIELDS - 1] = w;
                        // `n` is a guess at the sequence number `ring_push`
                        // takes; a wrong guess is a record whose fields do
                        // not match its `seq`, which the check below skips.
                        if ring_push(&ring, &next, false, f) {
                            stored += 1;
                        }
                    }
                    stored
                })
            })
            .collect();
        let reader = {
            let (ring, next, done) = (Arc::clone(&ring), Arc::clone(&next), Arc::clone(&done));
            std::thread::spawn(move || {
                let mut whole = 0u64;
                while !done.load(Ordering::Acquire) {
                    let top = next.load(Ordering::Relaxed);
                    if let Some(f) = top.checked_sub(1).and_then(|n| ring_read(&ring, n)) {
                        // Every field a writer stored is the same guess `g`;
                        // one writer's `g` beside another's is a tear.
                        let g = f[0];
                        assert!(
                            f[..RING_FIELDS - 1].iter().all(|&v| v == g),
                            "a torn record: {f:?}"
                        );
                        assert!(f[RING_FIELDS - 1] < 2, "a torn writer field: {f:?}");
                        whole += 1;
                    }
                }
                whole
            })
        };
        let stored: u64 = writers.into_iter().map(|h| h.join().expect("writer")).sum();
        done.store(true, Ordering::Release);
        let _ = reader.join().expect("reader");
        assert!(stored > 0, "no writer ever claimed the record");
        assert!(stored <= 2 * PER_WRITER);
    }

    /// gcj-j1pn-raw-root-write-owner-findings item 7: a [`VmTagScope`] tags
    /// only while the audit is armed, nests, and restores the outer tag.
    #[test]
    fn gcj1pn_the_vm_tag_scope_nests_and_restores() {
        let before = current_vm_tag();
        {
            let _outer = VmTagScope::enter(11);
            let outer = current_vm_tag();
            {
                let _inner = VmTagScope::enter(22);
                if enabled() {
                    assert_eq!(current_vm_tag(), 22);
                }
            }
            assert_eq!(current_vm_tag(), outer);
            if enabled() {
                assert_eq!(outer, 11);
            } else {
                assert_eq!(outer, before, "a disarmed audit tags nothing");
            }
        }
        assert_eq!(current_vm_tag(), before);
    }

    /// gcj-j1pn-raw-root-write-owner-findings item 7: the capture side counts
    /// every call, printed or not (ring mode prints nothing; the budget is
    /// shared with other tests, so only the count is checked).
    #[test]
    fn gcj1pn_the_capture_side_counts_every_call() {
        let own = SlotBand::new(0x1000, 0x2000);
        let (bands0, caps0) = capture_census();
        note_band_overrun(7, 0x1800, 0x3000, own);
        note_foreign_capture(7, 0x2800, 0x9000, own);
        note_foreign_capture(7, 0x2808, 0x9008, own);
        let (bands1, caps1) = capture_census();
        assert!(bands1 >= bands0 + 1);
        assert!(caps1 >= caps0 + 2);
    }
}
