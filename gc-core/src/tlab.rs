// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Thread-Local Allocation Buffers (TLABs).
//!
//! Each thread gets a small chunk of the young generation's from-space
//! that it can bump-allocate from without taking the global arena lock.
//! When the TLAB is exhausted, the thread requests a new one from the
//! shared arena (which does take the lock, but amortized over many
//! allocations).
//!
//! TLABs dramatically reduce lock contention on the allocation path.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock, Weak};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Tail sinks: who gets the unused end of a retiring TLAB
// ---------------------------------------------------------------------------
//
// A TLAB is carved by a heap and handed to a thread; when the thread retires
// it, the bytes between the cursor and the end have to go SOMEWHERE the
// collector can account for. The two collectors that own linear-walkable
// storage (Generational, G1) get an `int[]` filler written over the tail so
// their sweeps can parse across it (`install_tail_filler`). ZGC's sweep walks
// an object-start registry rather than memory, so a filler it never registered
// would simply never be reclaimed: the tail has to go back to the arena's free
// list instead, exactly as `zgc::arena_tlab` returns its own tails.
//
// `Tlab::retire` has no heap in scope -- it is called from three dozen VM
// sites that hold only the thread -- so the heap that carved the chunk
// registers itself here as a sink. A sink DECLINES a tail outside its own
// address range, which is what makes the registry safe when several heaps
// exist in one process (every gc-crate unit test builds its own): a tail from
// a Generational test arena is declined by every ZGC sink and takes the
// filler path it always did. The registry holds `Weak`s so a dropped heap
// simply stops answering; nothing has to unregister on the way out.

/// A heap that can take back the unused tail of a retiring [`Tlab`].
pub trait TlabTailSink: Send + Sync {
    /// Reclaim `[start, end)` -- an 8-aligned, non-empty span the owning
    /// thread bump-allocated nothing into. Return `false` to decline (the span
    /// is not this heap's); the caller then installs a filler object instead.
    fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool;

    /// zr z8/st: the buffer `[start, end)` is retiring with its cursor at
    /// `cursor` (`start <= cursor <= end`), BEFORE its tail is reclaimed or
    /// filled. Called only while [`retired_span_listener_count`] is non-zero,
    /// i.e. only when a ZGC heap runs with `CRATONVM_ZGC_LAZY_TLAB_STARTS`
    /// on; the default does nothing, so Generational and G1 (which never
    /// register a process-wide sink) are untouched. A sink ignores a span it
    /// did not carve.
    fn note_retired_span(&self, _start: usize, _cursor: usize, _end: usize) {}
}

/// zr z8/st: heaps that asked for [`TlabTailSink::note_retired_span`]. Zero
/// unless a ZGC heap was built with `CRATONVM_ZGC_LAZY_TLAB_STARTS` on, so a
/// default run pays one relaxed load per retire and nothing else. A count,
/// not compatibility state: it only gates a per-heap callback.
static RETIRED_SPAN_LISTENERS: AtomicUsize = AtomicUsize::new(0);

/// zr z8/st: a heap built with `CRATONVM_ZGC_LAZY_TLAB_STARTS` on registers
/// interest in retired spans (never undone: a dropped heap's `Weak` simply
/// stops answering, as for the tail sinks).
pub fn register_retired_span_listener() {
    RETIRED_SPAN_LISTENERS.fetch_add(1, Ordering::AcqRel);
}

/// zr z8/st: how many heaps registered through [`register_retired_span_listener`].
pub fn retired_span_listener_count() -> usize {
    RETIRED_SPAN_LISTENERS.load(Ordering::Relaxed)
}

/// zr z8/st: offer `[start, cursor)` of a retiring buffer to every sink.
fn note_retired_span_via_sinks(start: usize, cursor: usize, end: usize) {
    if RETIRED_SPAN_LISTENERS.load(Ordering::Relaxed) == 0
        || TAIL_SINK_COUNT.load(Ordering::Acquire) == 0
    {
        return;
    }
    let sinks = tail_sinks().read().unwrap_or_else(|e| e.into_inner());
    for weak in sinks.iter() {
        if let Some(sink) = weak.upgrade() {
            sink.note_retired_span(start, cursor, end);
        }
    }
}

/// Non-zero once any sink has registered. Read with one atomic load on every
/// retire so a process with no sink never touches the lock.
static TAIL_SINK_COUNT: AtomicUsize = AtomicUsize::new(0);

fn tail_sinks() -> &'static RwLock<Vec<Weak<dyn TlabTailSink>>> {
    static SINKS: OnceLock<RwLock<Vec<Weak<dyn TlabTailSink>>>> = OnceLock::new();
    SINKS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a heap as a tail sink. Dead entries (heaps already dropped) are
/// pruned on every registration, so the list is as long as the number of live
/// heaps rather than the number ever built.
pub fn register_tlab_tail_sink(sink: Weak<dyn TlabTailSink>) {
    let mut sinks = tail_sinks().write().unwrap_or_else(|e| e.into_inner());
    sinks.retain(|w| w.strong_count() > 0);
    sinks.push(sink);
    TAIL_SINK_COUNT.store(sinks.len(), Ordering::Release);
}

/// How many sinks are registered (live, or dead and not yet pruned).
///
/// LANE W6-A — printed as `tail_sinks=` on [`tlab_census_lines`]'s
/// `[GC] tlab-waste:` line. Before wave 6 this had **zero callers anywhere in
/// the workspace**, which made `sink_tail=0` on that same line unreadable: with
/// no sink registered, [`reclaim_tail_via_sinks`] returns on its first atomic
/// load and the byte count cannot be anything but zero.
pub fn tlab_tail_sink_count() -> usize {
    TAIL_SINK_COUNT.load(Ordering::Acquire)
}

fn reclaim_tail_via_sinks(start: usize, end: usize) -> bool {
    if TAIL_SINK_COUNT.load(Ordering::Acquire) == 0 {
        return false;
    }
    let sinks = tail_sinks().read().unwrap_or_else(|e| e.into_inner());
    for weak in sinks.iter() {
        if let Some(sink) = weak.upgrade() {
            if sink.reclaim_tlab_tail(start, end) {
                return true;
            }
        }
    }
    false
}

/// Default TLAB size: 256 KB — large enough to amortize the lock cost
/// across ~10k small-object allocations in a tight loop and to keep
/// `refill_tlab` out of the hot path during allocation storms.
///
/// T19.3.G1 (GC allocation-storm): the original 64 KB default caused
/// `ConcurrentHashMap.initTable`-style 2-object loops running at
/// ~25 MB/s to pound the shared arena lock every ~3 ms, triggering a
/// young-GC every ~1.2 s. Bumping the baseline to 256 KB reduces
/// refill pressure by 4× without meaningfully increasing tail waste
/// (typical thread carries one TLAB; waste bounded by `MAX_TLAB_SIZE`).
const DEFAULT_TLAB_SIZE: usize = 256 * 1024;

/// T5.5.1 — minimum TLAB size under low allocation pressure.
const MIN_TLAB_SIZE: usize = 8 * 1024;

/// Fragmentation-mode TLAB floor (perf/halfgap-20260717, the second
/// TLAB-remnant wedge): the smallest reclaimed span still worth serving as
/// a mini-TLAB when nothing `MIN_TLAB_SIZE`-big exists.
///
/// The first wedge (see `refill_tlab`'s fragmentation fallback) was a free
/// list made entirely of just-under-`requested` remnants. Fixing that by
/// probing for `MIN_TLAB_SIZE` blocks merely moved the wedge one level
/// down: steady-state splitting converges on remnants just UNDER the new
/// floor (observed live on BinTreesClassic d=18: 33 MB of free list, every
/// block exactly 4080 bytes = 8 KiB split minus header slack, gate floor
/// 8192 → 10.5 million consecutive guarded-refill failures, every
/// allocation crawling through the per-object helper slow path at a ~3x
/// whole-benchmark cost). Any bump-allocated span ≥ this floor beats the
/// per-object slow path by orders of magnitude (a 4080-byte span serves
/// ~100 small objects at bump speed), so the floor is deliberately tiny;
/// it exists only to keep degenerate slivers (< a few objects' worth) from
/// churning the refill machinery.
const FRAG_TLAB_FLOOR: usize = 256;

/// T5.5.1 — maximum TLAB size under high allocation pressure.
///
/// T19.3.G1: raised from 256 KB to 1 MB so the adaptive sizer can
/// grow a heavily-loaded thread's TLAB past the baseline when a
/// tight loop sustains high allocation rate.
const MAX_TLAB_SIZE: usize = 1024 * 1024;

/// T19.3.G1 — initial refill size for a freshly-created thread.
///
/// Returned by [`initial_refill_size`] so call sites that have no
/// per-thread pressure history (e.g. the first allocation on a new
/// worker thread) start with a size large enough to absorb a
/// static-init burst without an early refill.
const INITIAL_REFILL_SIZE: usize = DEFAULT_TLAB_SIZE;

/// T5.5.1 — threshold for "large" allocations, above which filling
/// a TLAB in fewer allocations still suggests high allocation pressure.
const LARGE_ALLOC_THRESHOLD: usize = 256;

/// T5.5.1 — a TLAB filled faster than this signals high pressure.
const FAST_REFILL_THRESHOLD_MS: u128 = 1;

/// T5.5.1 — a TLAB that took longer than this signals low pressure.
const SLOW_REFILL_THRESHOLD_MS: u128 = 100;

/// T5.5.1 — fewer than this many large allocations before refill → double.
const FAST_REFILL_ALLOC_COUNT: usize = 16;

/// T5.5.1 — fewer than this many total allocations before refill → halve.
const SLOW_REFILL_ALLOC_COUNT: usize = 4;

/// Minimum allocation that goes through the TLAB. Anything larger is
/// allocated directly from the shared arena (slow path).
///
/// Kept at 32 KB — half the *old* 64 KB default — so the cap on
/// TLAB-eligible object size is independent of the adaptive-sizing
/// baseline. A larger `TLAB_MAX_ALLOC` would cause tail-waste spikes
/// when a single oversized object kicks out an otherwise-full TLAB.
const TLAB_MAX_ALLOC: usize = 32 * 1024;

/// A thread-local allocation buffer.
///
/// This is a view into a slice of the young generation's from-space.
/// The thread owns this range exclusively — no locking needed for
/// bump-pointer allocation within the TLAB.
///
/// # Layout (JIT contract — DO NOT REORDER hot fields)
///
/// The struct is `#[repr(C)]` so the JIT-emitted inline TLAB bump-pointer
/// fast path (`jit/src/x64.rs` `new` opcode) can address `cursor` and
/// `end` at fixed offsets:
///
/// | Offset | Field    | Size |
/// |--------|----------|------|
/// |   0    | cursor   |   8  |
/// |   8    | end      |   8  |
/// |  16    | start    |   8  |
/// |  24    | zgc_owned_epoch | 8 |
/// |  32    | zgc_announce    | 8 |
/// |  40    | zgc_owned_words | 8 (two u32) |
/// |  48..  | pressure | rest |
///
/// `start`, `zgc_owned_epoch` and `zgc_announce` are read by the single-pass
/// JIT's inline ZGC start-bit store (`CRATONVM_ZGC_JIT_INLINE_ANNOUNCE`, round
/// 9 wave 8); see [`JitZgcAnnounceTable`]. `jit/src/x64/objects.rs` carries
/// copies of the three offsets (the JIT cannot depend on this crate) and a
/// test that pins them to [`Self::START_OFFSET`],
/// [`Self::ZGC_OWNED_EPOCH_OFFSET`] and [`Self::ZGC_ANNOUNCE_OFFSET`].
///
/// `cursor` is at offset 0 so the most-common load (cursor read) is a
/// register+0 mode (1 byte shorter on x86-64 ModR/M). `end` follows so
/// the comparison-and-spill check uses [reg+8] (still 1-byte disp8).
///
/// The runtime test `test_tlab_offsets` enforces the layout — if a
/// future edit reorders fields, the test fails and reminds the editor to
/// update [`Self::CURSOR_OFFSET`] / [`Self::END_OFFSET`] in lockstep.
#[repr(C)]
pub struct Tlab {
    /// Current allocation cursor within the TLAB.
    /// JIT contract: must remain at byte offset 0.
    cursor: *mut u8,
    /// End of the TLAB region (exclusive).
    /// JIT contract: must remain at byte offset 8.
    end: *mut u8,
    /// Start of the TLAB region (inclusive). Cold (only used at retire).
    /// JIT contract: must remain at byte offset 16 (read by the inline ZGC
    /// start-bit store).
    start: *mut u8,
    /// The ZGC VM-TLAB ownership epoch this buffer's chunk was carved under
    /// (`zgc::vm_tlab::OwnedVmTlabChunk`), or 0. The inline start-bit store
    /// may treat `[start, end)` as owned only while this equals
    /// [`JitZgcAnnounceTable::epoch`]. Set by [`Self::new`], zeroed by every
    /// retire. JIT contract: byte offset 24.
    zgc_owned_epoch: u64,
    /// Address of [`JIT_ZGC_ANNOUNCE`] when the inline start-bit store is
    /// armed for this chunk, else 0 (the JIT then calls the announce helper).
    /// Set with `zgc_owned_epoch`. JIT contract: byte offset 32.
    zgc_announce: usize,
    /// zr z11/ia: the start-bitmap words this buffer OWNS whole, as
    /// `[first, past_last)` word indices counted from the table's `base`
    /// ([`zgc_owned_word_window`]): word `k` covers
    /// `[base + 512k, base + 512(k + 1))`, and is owned when that lies inside
    /// `[start, end)`. Read by the JIT's long inline start-bit store
    /// (`CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW`) in place of its per-object
    /// granule arithmetic. Set with `zgc_owned_epoch`, `[0, 0)` (empty) when
    /// unarmed. JIT contract: byte offset 40 (first) and 44 (past last).
    zgc_owned_words: [u32; 2],
    /// T5.5.1 — per-thread adaptive-sizing state. Updated each
    /// allocation and consulted on refill. Cold — JIT inline fast path
    /// only updates `cursor`; the slow path (helper call) re-syncs the
    /// pressure tracker.
    pressure: TlabPressureTracker,
    /// Cumulative bytes this **thread** has allocated, excluding whatever the
    /// live TLAB has handed out so far — see [`Self::thread_allocated_bytes`],
    /// which adds the live span back.
    ///
    /// This is the backing store for `com.sun.management.ThreadMXBean
    /// .getThreadAllocatedBytes`, and it lives on the `Tlab` for one reason:
    /// the TLAB cursor is the **only** allocation signal that sees the JIT's
    /// inline bump. A counter incremented at the interpreter's allocation
    /// sites would silently report a fraction of a compiled thread's true
    /// allocation, which is exactly the shape of undercount that reads as
    /// good news (a chunk-reuse assertion measuring "did we allocate less
    /// than 8 MB" passes vacuously when the instrument sees nothing).
    ///
    /// Two contributions land here:
    /// * [`Self::retire`] rolls in `cursor - start` before nulling the
    ///   pointers, so a retired TLAB's consumption is not lost;
    /// * [`Self::note_external_allocation`] records the bytes of allocations
    ///   that bypassed the TLAB entirely (humongous objects and arrays go
    ///   straight to the heap).
    ///
    /// `Tlab::new` builds a *fresh* struct at every refill, so the running
    /// total must be carried across by the refill site — see
    /// [`Self::adopt_allocation_total`]. Both refill sites do that; a new one
    /// that forgets restarts the thread's counter at zero (monotonicity is
    /// asserted by `thread_allocated_bytes_survives_refill`).
    thread_alloc_carry: u64,
    /// How much of [`Self::thread_alloc_carry`] has already been published to
    /// the owning VM's allocation total (`vm_thread_total`). A high-water
    /// mark, not a counter: every publication sends
    /// `thread_alloc_carry - published` and then moves the mark up.
    ///
    /// Stated this way rather than as "add the bytes I just added" so the two
    /// counters cannot drift apart at all: a publication site that is
    /// forgotten is caught up by the next one, and a site that runs twice
    /// sends zero the second time. The old process-wide counter had both
    /// defects -- see [`TlabAccounting`].
    ///
    /// The mark moves only while a VM total is attached. Bytes settled on an
    /// unattached buffer (a JvmThread's first `Tlab::empty()`) wait behind
    /// it, and [`Self::attach_vm_thread_allocation_total`] credits them.
    /// Carried across refills by the refill site, which passes the outgoing
    /// buffer's mark to that attach.
    published: u64,
    /// Unpublished VM-level allocation observations. `gc_and_alloc` drains
    /// these in bounded batches, keeping shared counter traffic off the
    /// per-object bump path.
    vm_pending_allocated_bytes: u64,
    /// Per-VM destination for the bounded observations. Installed only on a
    /// Java thread's live TLAB, never on heap-internal/test buffers. The
    /// owning `SharedVm` outlives its JvmThreads, and the AtomicU64 is stable
    /// in its HeapRealm for that lifetime.
    vm_allocation_total: Option<std::ptr::NonNull<AtomicU64>>,
    /// `CRATONVM_DBG_WATCH_ADDR`, latched when the buffer is carved (0 =
    /// unarmed).
    ///
    /// gen r4/alloc (2026-09-23), the same fix `dbg_deadref_store` below
    /// received and for the same reason: [`Self::alloc_initialized`] asked
    /// [`watch_covers`] on EVERY bump, and `watch_covers` reads a process-wide
    /// `OnceLock` (its completion state, then the value) — two loads of a
    /// global per interpreted allocation, the shape
    /// `perf-every-allocation-reads-two-process-globals-and-bumps-two-shared-counters-FIXED-20260921.md`
    /// measured at ~13 % of an allocation-bound run for the `FLAGS` read it
    /// removed from this same path. The watch address is itself latched once
    /// per process, so copying it into the buffer at carve time cannot change
    /// a single answer. Last field, so the JIT's fixed offsets are untouched.
    watch_addr: usize,
    // zr z11/ia: `accounting`, `dbg_deadref_store` and `tuning` (four
    // bytes) sit together so they share ONE padded word of this `repr(C)`
    // struct; they used to pad a word each, and the word they gave back holds
    // `zgc_owned_words`, so `size_of::<Tlab>()` (and with it `JvmThread`'s
    // layout ratchet) did not move.
    /// Whether this buffer's consumption is the PROGRAM's allocation, and so
    /// belongs in the VM's allocation total. See [`TlabAccounting`].
    accounting: TlabAccounting,
    /// Latched when the buffer is carved. The dead-reference-store tripwire
    /// is a process-start diagnostic, but consulting `flags()` here used to
    /// make every bump allocation read the process-wide `FLAGS` cell.
    dbg_deadref_store: bool,
    /// Opt-in sizing / retire policies, latched when the buffer is carved.
    /// See [`TlabTuning`]. gen r4w3/alloc3 (2026-09-23); after `watch_addr`,
    /// so the JIT's fixed offsets are untouched.
    tuning: TlabTuning,
    /// gci i28/tl: HotSpot's `_gc_waste` for the `-Xlog:gc+tlab=debug`
    /// `TLAB totals` line -- the 8-byte words of every tail a retire buried
    /// under a filler (a tail a sink took back is not waste), except a
    /// refill's own retire, which [`Self::retire_for_refill`] moves to
    /// [`TlabCycleCounters::refill_waste_words`]. A wrapping running total,
    /// read as a difference ([`TlabCycleMark::since`]) by the pause, which
    /// never writes another thread's buffer. Carried across refills with the
    /// rest of [`TlabCycleMark`]. It fills the four bytes `accounting`,
    /// `dbg_deadref_store` and `tuning` leave before `vm_buffer_noted`, so
    /// `size_of::<Tlab>()` does not move.
    gc_waste_words: u32,
    /// Bytes this buffer has reported to the per-VM allocation counter through
    /// [`Self::note_vm_tlab_allocation`] (published or still pending). gen
    /// r4w4/alloc4: the retire tops the counter up to the buffer's CONSUMED
    /// span, so the per-VM total stops being blind to the JIT's inline bump —
    /// see [`Self::top_up_vm_allocation_total`]. Reset with the struct at every
    /// refill.
    vm_buffer_noted: u64,
    /// The HotSpot-shaped sizer's per-thread history (gen r4w4/alloc4). Carried
    /// across refills by the refill site, like `thread_alloc_carry` — see
    /// [`TlabShareSizer`].
    share: TlabShareSizer,
    /// The owning VM's retired-allocation total
    /// (`HeapRealm::thread_allocated_total`), the source of
    /// `getTotalThreadAllocatedBytes`. It replaced the process-wide
    /// `PROCESS_ALLOCATED_BYTES` (gc-common w6-c,
    /// `common-f-process-global-allocation-state`), which made VM A's answer
    /// include VM B's allocation in an embedding. Installed by
    /// [`Self::attach_vm_thread_allocation_total`] at the refill site (and by
    /// [`Self::ensure_vm_thread_allocation_total`] at the VM's non-TLAB
    /// allocation sites). `None` on heap-internal and test buffers. Same
    /// lifetime argument as `vm_allocation_total`. Cold (touched only when
    /// the carry is published), so it lives after the JIT's fixed offsets.
    vm_thread_total: Option<std::ptr::NonNull<AtomicU64>>,
    /// The heap that carved THIS buffer, when it takes unused tails back
    /// (gen r4w6/tlab6, `CRATONVM_GEN_TLAB_TAIL_SINK`). [`Self::retire`] offers
    /// the tail here first, then to the process-wide sinks, and writes the
    /// filler only if nobody takes it. Installed by the refill site with
    /// [`Self::attach_tail_sink`]; `None` on every other buffer, so their
    /// retires are unchanged. Cold, after the JIT's fixed offsets.
    tail_sink: Option<TailSinkRef>,
    /// gci i28/tl: this thread's running refill / slow-allocation /
    /// refill-waste counters for the `TLAB totals` line (see
    /// [`Self::gc_waste_words`]). Eight bytes in what was the struct's
    /// trailing padding (264 bytes of fields, 16-aligned to 272 by the
    /// pressure tracker's `u128`), so `size_of::<Tlab>()` and with it
    /// `JvmThread`'s layout ratchet do not move.
    cycle: TlabCycleCounters,
}

/// gci i28/tl: the per-thread counters behind HotSpot's `-Xlog:gc+tlab=debug`
/// `TLAB totals` line (`ThreadLocalAllocBuffer::_number_of_refills`,
/// `_slow_allocations`, `_refill_waste`). Wrapping running totals, never
/// reset: the pause reads them as a difference against the mark it kept
/// (per VM, `TlabTotalsState`), so the collector never writes a buffer that
/// another thread owns. `repr(C)` and eight bytes, for [`Tlab`]'s layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlabCycleCounters {
    /// Successful refills ([`Tlab::adopt_cycle_mark_for_refill`]).
    refills: u16,
    /// Misses that KEPT the buffer and allocated outside it
    /// ([`Tlab::keep_on_miss`]): HotSpot's `record_slow_allocation`.
    slow_allocations: u16,
    /// 8-byte words of the tails a refill's own retire buried
    /// ([`Tlab::retire_for_refill`]).
    refill_waste_words: u32,
}

/// gci i28/tl: a copy of one thread's TLAB cycle counters
/// ([`Tlab::cycle_mark`]): what the refill site carries to the new buffer and
/// what the pause keeps per thread to difference against next time.
/// gcj j2/yg2: `refills` and `slow_allocations` are the counters' full
/// 24-bit values (the low 16 bits from [`TlabCycleCounters`], bits 16..24 from
/// the share sizer's spare bytes), so [`Self::since`] wraps at
/// [`TLAB_CYCLE_COUNTER_MASK`]` + 1`, not at 65 536.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlabCycleMark {
    refills: u32,
    slow_allocations: u32,
    refill_waste_words: u32,
    gc_waste_words: u32,
}

/// gcj j2/yg2: the width of the `TLAB totals` refill and slow-allocation
/// counters (24 bits: 16 in [`TlabCycleCounters`], 8 in the share sizer's
/// padding, see `TlabShareSizer::cycle_refills_hi`). A thread is exact up to
/// this many refills (and slow allocations) in one cycle -- 16 777 215, i.e.
/// 16 GiB of 1 KiB fragmentation-floor mini-TLABs -- where it used to wrap at
/// 65 536.
pub const TLAB_CYCLE_COUNTER_MASK: u32 = 0x00FF_FFFF;

/// gci i28/tl: one thread's counts between two marks, in HotSpot's units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlabCycleDelta {
    pub refills: u32,
    pub slow_allocations: u32,
    pub gc_waste_bytes: u64,
    pub refill_waste_bytes: u64,
}

impl TlabCycleMark {
    /// The counts since `earlier` (the SAME thread's previous mark), with the
    /// counters' own wrapping widths: exact while one cycle stays under
    /// 2^24 refills (and 2^24 slow allocations; gcj j2/yg2, 65 536 before).
    ///
    /// gci i43/tj (`gci-i42ta-tlab-totals-refill-counter-wraps-on-a-hot-thread-RETIRED-UNFIXED-20261018`):
    /// this used to answer `None` for a difference above `0x8000`, a
    /// plausibility test meant to catch another thread's mark at a reused
    /// buffer address -- and it caught every hot thread that refilled more
    /// than 32 768 times in one cycle, whose line then counted its whole
    /// history. The caller now keys marks by thread id
    /// ([`TlabTotalsState::publish`]), so a mark is always the same thread's
    /// and the test has nothing left to catch.
    pub fn since(self, earlier: TlabCycleMark) -> TlabCycleDelta {
        let refills = self.refills.wrapping_sub(earlier.refills) & TLAB_CYCLE_COUNTER_MASK;
        let slow =
            self.slow_allocations.wrapping_sub(earlier.slow_allocations) & TLAB_CYCLE_COUNTER_MASK;
        TlabCycleDelta {
            refills,
            slow_allocations: slow,
            gc_waste_bytes: u64::from(self.gc_waste_words.wrapping_sub(earlier.gc_waste_words)) * 8,
            refill_waste_bytes: u64::from(
                self.refill_waste_words
                    .wrapping_sub(earlier.refill_waste_words),
            ) * 8,
        }
    }

    /// The counts since the thread began (no earlier mark).
    pub fn since_start(self) -> TlabCycleDelta {
        TlabCycleDelta {
            refills: self.refills,
            slow_allocations: self.slow_allocations,
            gc_waste_bytes: u64::from(self.gc_waste_words) * 8,
            refill_waste_bytes: u64::from(self.refill_waste_words) * 8,
        }
    }
}

/// gci i28/tl: HotSpot's `ThreadLocalAllocStats` for one collection: the
/// per-thread deltas summed (`update_fast_allocations` for a thread with a
/// refill this cycle, `update_slow_allocations` for every thread), printed
/// by [`Self::line`] as `ThreadLocalAllocStats::publish` does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlabTotals {
    pub threads: u32,
    pub refills: u64,
    pub max_refills: u32,
    pub slow_allocations: u64,
    pub max_slow_allocations: u32,
    /// The bytes the refilling threads allocated this cycle -- HotSpot's
    /// denominator is the bytes of the buffers it handed them
    /// (`_allocated_size`), which this VM does not keep per thread; the
    /// difference is the waste itself plus allocation outside the buffers.
    pub allocated_bytes: u64,
    pub gc_waste_bytes: u64,
    pub max_gc_waste_bytes: u64,
    pub refill_waste_bytes: u64,
    pub max_refill_waste_bytes: u64,
}

impl TlabTotals {
    /// Fold one thread's cycle in, as HotSpot's
    /// `accumulate_and_reset_statistics` does: the fast-allocation figures
    /// only for a thread that refilled this cycle, the slow allocations for
    /// every thread.
    pub fn add_thread(&mut self, d: TlabCycleDelta, allocated_bytes: u64) {
        if d.refills > 0 {
            self.threads += 1;
            self.refills += u64::from(d.refills);
            self.max_refills = self.max_refills.max(d.refills);
            self.allocated_bytes = self.allocated_bytes.saturating_add(allocated_bytes);
            self.gc_waste_bytes += d.gc_waste_bytes;
            self.max_gc_waste_bytes = self.max_gc_waste_bytes.max(d.gc_waste_bytes);
            self.refill_waste_bytes += d.refill_waste_bytes;
            self.max_refill_waste_bytes = self.max_refill_waste_bytes.max(d.refill_waste_bytes);
        }
        self.slow_allocations += u64::from(d.slow_allocations);
        self.max_slow_allocations = self.max_slow_allocations.max(d.slow_allocations);
    }

    /// HotSpot 25's `ThreadLocalAllocStats::publish` line, without the
    /// `GC(n) ` prefix; `None` when no thread refilled (`publish` returns
    /// early then). The figures are this VM's: its buffer sizing is not
    /// HotSpot's, so only the line's presence and shape can match.
    ///
    /// gci i30/ob hand-back 1 (vi30-ob T1: `GC(198) Pause Young (Concurrent
    /// Start)` and three more pauses of one javac cycle printed no line;
    /// `waste: 148881.8%` on 1-thread lines): HotSpot's gate is
    /// `_total_allocations == 0`, the sum of the refilling threads'
    /// `_allocated_size`, which `ThreadLocalAllocBuffer::fill` grows by every
    /// buffer it hands out -- so ANY refill prints the line. This gated on the
    /// refilling threads' allocated-byte counter, which lags a fresh buffer
    /// (a thread that refilled once and was stopped before its counter moved
    /// counted 0), and dropped such pauses' lines. The waste denominator is
    /// HotSpot's handed-out bytes, approximated as allocated + both wastes
    /// (a buffer's bytes were either used or wasted), so it stays <= 100 %.
    pub fn line(&self) -> Option<String> {
        if self.threads == 0 {
            return None;
        }
        let waste = self.gc_waste_bytes + self.refill_waste_bytes;
        let handed_out = self.allocated_bytes.saturating_add(waste);
        let waste_percent = if handed_out == 0 {
            0.0
        } else {
            waste as f64 * 100.0 / handed_out as f64
        };
        Some(format!(
            "TLAB totals: thrds: {}  refills: {} max: {} slow allocs: {} max {} waste: {:4.1}% \
             gc: {}B max: {}B slow: {}B max: {}B",
            self.threads,
            self.refills,
            self.max_refills,
            self.slow_allocations,
            self.max_slow_allocations,
            waste_percent,
            self.gc_waste_bytes,
            self.max_gc_waste_bytes,
            self.refill_waste_bytes,
            self.max_refill_waste_bytes,
        ))
    }
}

/// gci i28/tl: one thread's state as the pause reads it for the `TLAB
/// totals` line: its key, its cycle counters, and its running allocated
/// bytes ([`Tlab::thread_allocated_bytes`]).
///
/// gci i43/tj: the key is the thread's id (`ThreadRegistry::tlab_cycle_snapshots`,
/// the registry's never-reused `ThreadId`), no longer its `Tlab` field
/// address: an address can be reused by a thread started after the owner
/// died, an id cannot, so [`TlabCycleMark::since`] needs no plausibility
/// test (which also refused every same-thread cycle above 32 768 refills).
pub type TlabCycleSnapshot = (usize, TlabCycleMark, u64);

/// gci i28/tl: the per-VM half of the `TLAB totals` line (a field of the VM's
/// heap realm, never a process global): the marks of the previous
/// collection, and the line the current collection computed at its start
/// for the `-Xlog` writer to print after its `Heap Before GC` block.
#[derive(Default)]
pub struct TlabTotalsState {
    // addr-keyed: not an address since gci i43/tj -- the key is the thread's
    // id (see `TlabCycleSnapshot`); replaced whole at every publish (dead
    // threads drop out).
    marks: parking_lot::Mutex<Vec<TlabCycleSnapshot>>,
    pending: parking_lot::Mutex<Option<String>>,
}

impl TlabTotalsState {
    /// Sum this collection's per-thread deltas against the previous marks
    /// and keep `now` as the next marks. A thread with no previous mark
    /// counts from its start; so does one whose allocated total went
    /// backwards (a fresh `Tlab` carry under a reused key: never the same
    /// thread's history).
    pub fn publish(&self, mut now: Vec<TlabCycleSnapshot>) -> TlabTotals {
        // Kept sorted by key, so each lookup is a binary search.
        now.sort_unstable_by_key(|s| s.0);
        let mut marks = self.marks.lock();
        let mut totals = TlabTotals::default();
        for &(key, mark, allocated) in &now {
            let earlier = marks
                .binary_search_by_key(&key, |s| s.0)
                .ok()
                .map(|i| marks[i]);
            let (delta, alloc) = match earlier {
                Some((_, before, before_alloc)) if allocated >= before_alloc => {
                    (mark.since(before), allocated - before_alloc)
                }
                _ => (mark.since_start(), allocated),
            };
            totals.add_thread(delta, alloc);
        }
        *marks = now;
        totals
    }

    /// Replace the pending line (`None` clears a line no writer took).
    pub fn stash(&self, line: Option<String>) {
        *self.pending.lock() = line;
    }

    /// Take the pending line.
    pub fn take(&self) -> Option<String> {
        self.pending.lock().take()
    }
}

/// A type-erased borrow of the [`TlabTailSink`] a buffer was armed with: the
/// sink's address and the monomorphised call that knows its type (gen
/// r4w6/tlab6). Thin pointers rather than a `NonNull<dyn TlabTailSink>`, so
/// arming a buffer changes none of `Tlab`'s auto traits (`dyn TlabTailSink` is
/// not `RefUnwindSafe`, and a closure over a thread that holds a `Tlab` must
/// not stop being unwind-safe because of a diagnostic-cold field).
#[derive(Clone, Copy)]
struct TailSinkRef {
    data: *const (),
    reclaim: unsafe fn(*const (), usize, usize) -> bool,
}

/// [`TailSinkRef::reclaim`] for a sink of type `S`.
///
/// # Safety
/// `data` must come from a `&S` (by [`Tlab::attach_tail_sink`]) that is still
/// live.
unsafe fn reclaim_tail_via<S: TlabTailSink>(data: *const (), start: usize, end: usize) -> bool {
    // SAFETY: the caller's contract — `data` is a live `&S`, and `S: Sync`
    // (a `TlabTailSink` supertrait), so a shared reference from any thread is
    // sound.
    let sink = unsafe { &*(data as *const S) };
    sink.reclaim_tlab_tail(start, end)
}

/// Per-thread state of the HotSpot-shaped TLAB sizer (`CRATONVM_TLAB_SHARE_SIZER`,
/// gen r4w4/alloc4, 2026-09-24).
///
/// # The model
///
/// HotSpot sizes a thread's TLAB as
/// `desired = alloc_fraction × eden_capacity / target_refills`, where
/// `alloc_fraction` is an exponentially weighted average
/// (`TLABAllocationWeight` = 35 %) of the thread's share of the eden bytes
/// allocated between two young collections, and
/// `target_refills = 100 / (2 × TLABWasteTargetPercent)` = 50 — a thread that
/// refills 50 times per young cycle wastes on average half a buffer per
/// cycle, i.e. 1 % of its allocation. `share × eden bytes per cycle` is
/// simply the thread's OWN bytes per cycle, so this averages that product
/// directly: [`Tlab::thread_allocated_bytes`] is the cursor-based total (the
/// JIT's inline bump included), where the per-VM counter the share's
/// denominator would need was, until this wave, interpreter-only. When eden's
/// size is steady the two are the same number; when it changes, HotSpot's
/// fraction rescales at once and this average follows within a few cycles.
///
/// # Sampling without a collector hook
///
/// HotSpot resizes every thread AT each young collection. Nothing here visits
/// threads at a collection, so the window is closed LAZILY: every refill
/// passes the heap's collection count, and the first refill that sees it move
/// folds the bytes allocated since the window opened (divided by the number
/// of collections it spans, so an idle thread averages down) into the average
/// and resizes. A thread's TLAB is retired at the collection's safepoint, so
/// that window's bytes are exactly the ones allocated before the collection.
///
/// # Refill-waste limit
///
/// On a fast-path miss HotSpot RETIRES only when the remaining tail is at most
/// `desired / TLABRefillWasteFraction` (64); a bigger tail is kept and the
/// object is allocated outside the buffer, raising the limit so a thread cannot
/// fall into per-object slow allocation. The shared path here takes the young
/// lock (HotSpot's is a CAS), so the raise is `desired / 64` per keep rather
/// than 4 words: at most 64 keeps per thread per cycle, and each keep needs an
/// object bigger than the limit, so a buffer keeps at most
/// `TLAB_MAX_ALLOC / limit` times. See [`Tlab::keep_on_miss`].
///
/// # One deliberate deviation
///
/// A thread predicted small (it was idle last cycle) and now allocating hard
/// would refill hundreds of times before the next collection resizes it; our
/// young cycles can be seconds long and each refill takes the young lock. So a
/// window that reaches `2 × target_refills` refills doubles its desired size
/// in place (an "underpredicted raise", counted on the `[GC] tlab-sizer:`
/// line). The next collection's sample replaces it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TlabShareSizer {
    /// Collection count at which the current window opened; [`u64::MAX`] until
    /// the thread's first sized refill.
    window_epoch: u64,
    /// [`Tlab::thread_allocated_bytes`] when the window opened.
    window_start_bytes: u64,
    /// Exponentially weighted average of this thread's bytes allocated per
    /// young-collection interval.
    avg_bytes_per_cycle: f64,
    /// Samples folded into the average (drives the warm-up weight). gcj
    /// j2/yg2: `u16` (saturating; past 100 samples the weight is
    /// `TLABAllocationWeight` whatever the count), so two bytes of this
    /// struct's padding carry the `TLAB totals` counters' high bytes below.
    samples: u16,
    /// What every refill in the current window requests; 0 before the first.
    desired: usize,
    /// HotSpot's `_refill_waste_limit`: the largest tail a miss may retire.
    refill_waste_limit: usize,
    /// Refills sized in the current window.
    refills_in_window: u32,
    /// gcg g1/j: the LAST [`Tlab::share_refill_request`] was asked on a
    /// buffer that an early retire (a park, a blocking native, every JNI
    /// entry into native under the package) had already given up while it
    /// was not well used, in the same window. [`Tlab::note_share_refill`]
    /// then does not count the granted refill toward the underpredicted
    /// raise: the thread did not run out of buffer, it left it. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    refill_after_early_retire: bool,
    // gci i45/sc: the VM's TLAB flags ([`TlabSizing`]) the calls that take no
    // sizing argument ([`Tlab::note_share_refill`], [`Tlab::keep_on_miss`],
    // the sample) read, copied in by every [`Tlab::share_refill_request_sized`].
    // Seven bytes in what was this struct's trailing padding (49 bytes of
    // fields, 8-aligned to 56), so `size_of::<TlabShareSizer>()` and with it
    // `size_of::<Tlab>()` (272, `JvmThread`'s layout ratchet) do not move.
    /// `TLABRefillWasteFraction` (64 unless typed).
    refill_waste_fraction: u32,
    /// `TLABWasteTargetPercent` (1 unless typed).
    waste_target_percent: u8,
    /// `TLABAllocationWeight` (35 unless typed).
    allocation_weight: u8,
    /// `ResizeTLAB` (on unless `-XX:-ResizeTLAB`).
    resize: bool,
    /// gcj j2/yg2 (`gcj-j1yg-tlab-totals-counters-wrap-above-65536-refills-per-cycle`):
    /// bits 16..24 of the `TLAB totals` refill counter
    /// ([`TlabCycleCounters::refills`] holds bits 0..16), so a thread's line
    /// stays exact up to [`TLAB_CYCLE_COUNTER_MASK`] refills per cycle. Here
    /// because `Tlab`'s 272 bytes (`JvmThread`'s layout ratchet) have no byte
    /// left, and this struct travels with the counters at every refill
    /// ([`Tlab::adopt_share_sizer`] before [`Tlab::adopt_cycle_mark_for_refill`]).
    cycle_refills_hi: u8,
    /// gcj j2/yg2: bits 16..24 of the slow-allocation counter, as above.
    cycle_slow_hi: u8,
}

/// gcj j3/yg3 (`CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`): what one granted
/// refill of `granted` bytes adds to the share sizer's window counter, and the
/// count at which the window raises `desired`, pure. Refill rule (`bytes`
/// false): `(1, 2 x target_refills)`, the historical rule. Byte rule: the
/// granted KiB (at least 1) and `2 x target_refills x desired` in KiB (at
/// least 1), so `2 x target_refills` grants of exactly `desired` bytes raise
/// as the refill rule does.
fn share_raise_step_and_threshold(
    bytes: bool,
    granted: usize,
    desired: usize,
    target_refills: usize,
) -> (u32, usize) {
    if !bytes {
        return (1, 2 * target_refills);
    }
    let step = u32::try_from(granted >> 10).unwrap_or(u32::MAX).max(1);
    let threshold = (2 * target_refills).saturating_mul(desired >> 10).max(1);
    (step, threshold)
}

// gci i45/sc: the sizer's four flag fields live in its padding (see above).
const _: () = assert!(
    std::mem::size_of::<TlabShareSizer>() <= 56,
    "TlabShareSizer grew: Tlab's 272-byte layout pin moves with it"
);

impl Default for TlabShareSizer {
    fn default() -> Self {
        Self {
            window_epoch: u64::MAX,
            window_start_bytes: 0,
            avg_bytes_per_cycle: 0.0,
            samples: 0,
            desired: 0,
            refill_waste_limit: 0,
            refills_in_window: 0,
            refill_after_early_retire: false,
            refill_waste_fraction: TLAB_REFILL_WASTE_FRACTION as u32,
            waste_target_percent: TLAB_WASTE_TARGET_PERCENT as u8,
            allocation_weight: TLAB_ALLOCATION_WEIGHT_PERCENT as u8,
            resize: true,
            cycle_refills_hi: 0,
            cycle_slow_hi: 0,
        }
    }
}

/// `CRATONVM_TLAB_SHARE_SIZER_MAX_KIB` — the share sizer's largest desired
/// size, in KiB, clamped to `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]` and to the 8-byte
/// grid; unset or unparsable = `MAX_TLAB_SIZE` (1 MiB), the cap it always had.
/// gen r5w5/sizer9: a bisection lever for
/// `docs/feature-designs/generative-gc-architecture.md`
/// mechanism A, which the orchestrator measured only once the sizer grew
/// buffers to 700 KiB–1 MiB (the ladder re-arms at 256 KiB after every
/// retire). Read once per process, like `CRATONVM_GEN_CONC_MARK_SLICE`: a
/// tuning knob, not per-VM state.
pub fn share_sizer_max() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
            .map(|kib| kib.saturating_mul(1024).clamp(MIN_TLAB_SIZE, MAX_TLAB_SIZE) & !7)
            .unwrap_or(MAX_TLAB_SIZE)
    })
}

/// HotSpot's `TLABWasteTargetPercent` default: the tail waste a thread's
/// sizing aims at, as a percentage of its allocation.
const TLAB_WASTE_TARGET_PERCENT: usize = 1;
/// `100 / (2 × TLABWasteTargetPercent)` = 50 refills per young cycle.
const TLAB_TARGET_REFILLS: usize = 100 / (2 * TLAB_WASTE_TARGET_PERCENT);
/// HotSpot's `TLABAllocationWeight`: the weight, in percent, of the newest
/// sample in the exponential average.
const TLAB_ALLOCATION_WEIGHT_PERCENT: u32 = 35;
/// gcj j2/yg2: HotSpot 25's `TLABWasteIncrement` default, in words.
const HOTSPOT_TLAB_WASTE_INCREMENT_WORDS: u32 = 4;
/// HotSpot's `TLABRefillWasteFraction`: the refill-waste limit is
/// `desired / 64`, and so is each raise of it (see [`TlabShareSizer`]).
const TLAB_REFILL_WASTE_FRACTION: usize = 64;

/// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
/// the TLAB consumer half): HotSpot 25's TLAB flags as THIS VM's TLAB sizing
/// reads them -- `-XX:TLABSize`, `-XX:MinTLABSize`, `-XX:±UseTLAB`,
/// `-XX:±ResizeTLAB`, `-XX:TLABWasteTargetPercent`,
/// `-XX:TLABRefillWasteFraction`, `-XX:TLABAllocationWeight`, in the terms of
/// `ThreadLocalAllocBuffer::initial_desired_size` (a typed `TLABSize` is the
/// first desired size, clamped to `[min, max]`), `resize` (`avg / target_refills`
/// clamped to `[min, max]`, skipped under `-ResizeTLAB`), `startup_initialization`
/// (`target_refills = max(100 / (2 * TLABWasteTargetPercent), 2)`) and the
/// refill-waste limit (`desired / TLABRefillWasteFraction`).
///
/// Per VM: built once from `VmConfig::tlab_flags` by the VM's construction
/// and kept on its heap realm (`HeapRealm::tlab_sizing`), never a process
/// global; a `Copy` value the refill site passes to
/// [`Tlab::share_refill_request_sized`] and reads for the ladder and for
/// `-UseTLAB`. [`Self::DEFAULT`] (nothing typed, or
/// `CRATONVM_TLAB_HOTSPOT_FLAGS=0`) is exactly this file's constants, so a
/// command line without the flags sizes every TLAB byte for byte as before.
///
/// Two of this VM's deliberate deviations stay: the untyped first desired
/// size is the "start big" [`INITIAL_REFILL_SIZE`] (HotSpot divides the TLAB
/// capacity by the thread count), and the untyped floor is [`MIN_TLAB_SIZE`]
/// (8 KiB; HotSpot's `MinTLABSize` default is 2 KiB). A TYPED `MinTLABSize`
/// replaces the floor (on the 8-byte grid, never below [`FRAG_TLAB_FLOOR`],
/// never above [`MAX_TLAB_SIZE`]); a typed `TLABSize` replaces the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlabSizing {
    use_tlab: bool,
    resize_tlab: bool,
    initial_size: usize,
    min_size: usize,
    /// Per-VM ergonomic upper bound. Explicit HotSpot sizing is selected by
    /// the VM before applying an ergonomic cap; the process tuning override
    /// `share_sizer_max` remains an additional upper bound.
    max_size: usize,
    waste_target_percent: u8,
    refill_waste_fraction: u32,
    allocation_weight: u8,
    /// gcj j2/yg2 (`CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`, opt-in, Generational
    /// only; `gci-i45sc-proposal-tlab-refill-waste-limit-per-fill`): HotSpot's
    /// refill-waste limit -- re-armed to `desired / TLABRefillWasteFraction`
    /// at every refill ([`Tlab::share_refill_request_sized`]) and raised by
    /// `waste_increment_words` words per kept miss
    /// ([`Tlab::keep_on_miss_sized`]). `false` is this VM's deviation (see
    /// [`TlabShareSizer`]).
    hotspot_waste_limit: bool,
    /// `-XX:TLABWasteIncrement` in 8-byte words (HotSpot's default 4), read
    /// only under `hotspot_waste_limit`.
    waste_increment_words: u32,
    /// gcj j3/yg3 (`CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`, opt-in,
    /// Generational only; `gcj-j2yg2-proposal-underpredicted-raise-counts-granted-bytes`):
    /// the share sizer's underpredicted raise counts the window's granted
    /// bytes, not its refills ([`Tlab::note_share_refill_sized`]).
    raise_by_granted_bytes: bool,
}

impl Default for TlabSizing {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TlabSizing {
    /// Nothing typed: this file's constants.
    pub const DEFAULT: Self = Self {
        use_tlab: true,
        resize_tlab: true,
        initial_size: INITIAL_REFILL_SIZE,
        min_size: MIN_TLAB_SIZE,
        max_size: MAX_TLAB_SIZE,
        waste_target_percent: TLAB_WASTE_TARGET_PERCENT as u8,
        refill_waste_fraction: TLAB_REFILL_WASTE_FRACTION as u32,
        allocation_weight: TLAB_ALLOCATION_WEIGHT_PERCENT as u8,
        hotspot_waste_limit: false,
        waste_increment_words: HOTSPOT_TLAB_WASTE_INCREMENT_WORDS,
        raise_by_granted_bytes: false,
    };

    /// The sizing for the launcher's values (`None` = not typed). The
    /// launcher already refused what HotSpot refuses (ranges and the
    /// `AfterMemoryInit` constraints, gci i43/lf); out-of-range input from an
    /// embedder is clamped to HotSpot's ranges rather than trusted. Pure.
    pub fn from_hotspot_flags(
        use_tlab: Option<bool>,
        resize_tlab: Option<bool>,
        tlab_size: Option<u64>,
        min_tlab_size: Option<u64>,
        waste_target_percent: Option<u64>,
        refill_waste_fraction: Option<u64>,
        allocation_weight: Option<u64>,
    ) -> Self {
        let d = Self::DEFAULT;
        // On the 8-byte grid `Tlab::new` requires, never above the cap.
        let grid = |b: u64| (b.min(MAX_TLAB_SIZE as u64) as usize).saturating_add(7) & !7;
        let min_size = match min_tlab_size {
            Some(m) => grid(m).clamp(FRAG_TLAB_FLOOR, MAX_TLAB_SIZE),
            None => d.min_size,
        };
        // `TLABSize=0` is HotSpot's "ergonomic": the untyped start.
        let initial_size = match tlab_size {
            Some(s) if s > 0 => grid(s),
            _ => d.initial_size,
        }
        .clamp(min_size, MAX_TLAB_SIZE);
        Self {
            use_tlab: use_tlab.unwrap_or(d.use_tlab),
            resize_tlab: resize_tlab.unwrap_or(d.resize_tlab),
            initial_size,
            min_size,
            max_size: d.max_size,
            waste_target_percent: waste_target_percent
                .map_or(d.waste_target_percent, |p| p.clamp(1, 100) as u8),
            refill_waste_fraction: refill_waste_fraction
                .map_or(d.refill_waste_fraction, |f| f.clamp(1, u64::from(u32::MAX)) as u32),
            allocation_weight: allocation_weight
                .map_or(d.allocation_weight, |w| w.min(100) as u8),
            hotspot_waste_limit: d.hotspot_waste_limit,
            waste_increment_words: d.waste_increment_words,
            raise_by_granted_bytes: d.raise_by_granted_bytes,
        }
    }

    /// gcj j2/yg2 (`CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`): these flags with
    /// HotSpot's refill-waste limit armed and `-XX:TLABWasteIncrement`
    /// (`None` = HotSpot's default 4 words; clamped to HotSpot's range
    /// `0 ... max_jint`). The VM's construction calls it only on the
    /// Generational heap with the switch on. Pure.
    pub fn with_hotspot_waste_limit(self, waste_increment: Option<u64>) -> Self {
        let words = waste_increment
            .map_or(HOTSPOT_TLAB_WASTE_INCREMENT_WORDS, |w| w.min(i32::MAX as u64) as u32);
        Self {
            hotspot_waste_limit: true,
            waste_increment_words: words,
            ..self
        }
    }

    /// gcj j3/yg3 (`CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`): these flags
    /// with the share sizer's raise counted in granted bytes. The VM's
    /// construction calls it only on the Generational heap with the switch
    /// on. Pure.
    pub fn with_raise_by_granted_bytes(self) -> Self {
        Self {
            raise_by_granted_bytes: true,
            ..self
        }
    }

    /// Bound ergonomic carves without changing the TLAB/JIT ABI or another
    /// VM's sizer. Keep the VM's minimum, and bound its initial carve too.
    pub fn with_maximum(self, bytes: usize) -> Self {
        let max_size = (bytes & !7).clamp(self.min_size, MAX_TLAB_SIZE);
        Self { max_size, initial_size: self.initial_size.min(max_size), ..self }
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// gcj j3/yg3: the raise counts granted bytes (see the field).
    pub fn raise_by_granted_bytes(&self) -> bool {
        self.raise_by_granted_bytes
    }

    /// gcj j2/yg2: HotSpot's refill-waste limit is armed (see the field).
    pub fn hotspot_waste_limit(&self) -> bool {
        self.hotspot_waste_limit
    }

    /// gcj j2/yg2: `-XX:TLABWasteIncrement`, words.
    pub fn waste_increment_words(&self) -> u32 {
        self.waste_increment_words
    }

    /// Nothing typed (or every value HotSpot's own default where that is this
    /// VM's too).
    pub fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }

    /// `-XX:-UseTLAB` reads `false`: the refill site carves no buffer beyond
    /// the object in hand, so every allocation takes the shared path.
    pub fn use_tlab(&self) -> bool {
        self.use_tlab
    }

    /// `-XX:-ResizeTLAB` reads `false`: the desired size stays the initial one.
    pub fn resize_tlab(&self) -> bool {
        self.resize_tlab
    }

    /// The first desired size (bytes, 8-aligned, in `[min_size, MAX_TLAB_SIZE]`):
    /// a typed `TLABSize`, else [`INITIAL_REFILL_SIZE`] (at least `min_size`).
    pub fn initial_size(&self) -> usize {
        self.initial_size
    }

    /// The smallest desired size (bytes, 8-aligned): a typed `MinTLABSize`,
    /// else [`MIN_TLAB_SIZE`].
    pub fn min_size(&self) -> usize {
        self.min_size
    }

    /// HotSpot's `_target_refills`: `max(100 / (2 * TLABWasteTargetPercent), 2)`
    /// (50 at the default 1 %).
    pub fn target_refills(&self) -> usize {
        (100 / (2 * usize::from(self.waste_target_percent.max(1)))).max(2)
    }

    /// `TLABRefillWasteFraction`.
    pub fn refill_waste_fraction(&self) -> u32 {
        self.refill_waste_fraction
    }

    /// `TLABAllocationWeight`, percent.
    pub fn allocation_weight(&self) -> u8 {
        self.allocation_weight
    }

    /// The ladder's request (the share sizer off: G1 and ZGC by default)
    /// under these flags. `first` = the thread's buffer never held a chunk;
    /// `ladder` = the pressure tracker's answer, asked only when it is used
    /// (`-ResizeTLAB` freezes the size at [`Self::initial_size`], as HotSpot's
    /// `resize_all_tlabs` does nothing then). The default sizing answers
    /// [`INITIAL_REFILL_SIZE`] / `max(ladder, MIN_TLAB_SIZE)`, the refill
    /// site's arithmetic before this type.
    pub fn ladder_refill_request(&self, first: bool, ladder: impl FnOnce() -> usize) -> usize {
        if first || !self.resize_tlab {
            self.initial_size
        } else {
            ladder().max(self.min_size).min(self.max_size)
        }
    }
}

impl TlabShareSizer {
    /// gci i45/sc: copy the VM's TLAB flags the calls without a sizing
    /// argument read (see the struct's last four fields).
    fn adopt_flags(&mut self, sizing: &TlabSizing) {
        self.refill_waste_fraction = sizing.refill_waste_fraction.max(1);
        self.waste_target_percent = sizing.waste_target_percent.max(1);
        self.allocation_weight = sizing.allocation_weight.min(100);
        self.resize = sizing.resize_tlab;
    }

    /// gci i45/sc: `target_refills` for the flags this sizer last adopted.
    fn target_refills(&self) -> usize {
        (100 / (2 * usize::from(self.waste_target_percent.max(1)))).max(2)
    }

    /// Fold one sample (bytes allocated per collection interval) into the
    /// average. HotSpot's `AdaptiveWeightedAverage`: the weight is
    /// `max(TLABAllocationWeight, 100 / samples)` percent, so the first sample
    /// IS the average and the next few move it quickly. gci i45/sc: the VM's
    /// `TLABAllocationWeight` (35 unless typed).
    fn sample(&mut self, bytes_per_cycle: f64) {
        self.samples = self.samples.saturating_add(1);
        let weight =
            u32::from(self.allocation_weight).max(u32::from(100 / self.samples)) as f64 / 100.0;
        self.avg_bytes_per_cycle =
            (1.0 - weight) * self.avg_bytes_per_cycle + weight * bytes_per_cycle;
    }

    /// `avg / target_refills`, clamped to `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]` and
    /// kept on the 8-byte grid `Tlab::new` requires.
    #[cfg_attr(not(test), allow(dead_code))] // gci i45/sc: the default-flags reference
    fn desired_from_average(avg_bytes_per_cycle: f64) -> usize {
        Self::desired_from_average_with(avg_bytes_per_cycle, TLAB_TARGET_REFILLS, MIN_TLAB_SIZE)
    }

    /// gci i45/sc: [`Self::desired_from_average`] at the VM's
    /// `target_refills` and minimum ([`TlabSizing`]): HotSpot's `resize`,
    /// `clamp(avg / target_refills, min_size, max_size)`. A `min_size` above
    /// [`MAX_TLAB_SIZE`] reads as the maximum (no panic). Pure.
    fn desired_from_average_with(
        avg_bytes_per_cycle: f64,
        target_refills: usize,
        min_size: usize,
    ) -> usize {
        let raw = avg_bytes_per_cycle / target_refills.max(1) as f64;
        // `as usize` saturates for a huge or non-finite value and maps NaN to 0.
        let raw = if raw.is_finite() && raw > 0.0 {
            raw as usize
        } else {
            0
        };
        raw.max(min_size).min(MAX_TLAB_SIZE) & !7
    }

    /// Install a new desired size and re-arm the refill-waste limit to it.
    ///
    /// gen r5w5/sizer9: capped at [`share_sizer_max`] (1 MiB unless
    /// `CRATONVM_TLAB_SHARE_SIZER_MAX_KIB` lowers it), on the 8-byte grid.
    /// gci i45/sc: the limit is `desired / TLABRefillWasteFraction` at the
    /// VM's fraction (64 unless typed).
    fn set_desired(&mut self, desired: usize) {
        let desired = if desired > share_sizer_max() {
            share_sizer_max()
        } else {
            desired
        };
        self.desired = desired;
        self.refill_waste_limit = desired / (self.refill_waste_fraction.max(1) as usize);
    }

    /// The desired size the current window asks for (0 before the first
    /// sized refill). Diagnostics and tests.
    pub fn desired(&self) -> usize {
        self.desired
    }

    /// The exponentially weighted bytes-per-cycle average. Diagnostics and tests.
    pub fn average_bytes_per_cycle(&self) -> f64 {
        self.avg_bytes_per_cycle
    }

    /// The current refill-waste limit. Diagnostics and tests.
    pub fn refill_waste_limit(&self) -> usize {
        self.refill_waste_limit
    }
}

/// Opt-in TLAB policies, latched from `cratonvm_types::flags()` once per
/// carved buffer (the same latch point as `Tlab::dbg_deadref_store`), so
/// neither the bump path nor the retire path reads the process-wide snapshot.
///
/// gen r4w3/alloc3 (2026-09-23). Default OFF: each changes what every
/// backend's Java-thread TLABs do. With them off every answer in this file is
/// byte-for-byte what it was — the A/B protocols are on the gap pages named
/// below. (gce e2/o removed the third, `CRATONVM_TLAB_WASTE_SHRINK`, with the
/// `CRATONVM_TLAB_SIZE_RETIRED` refill arm that was its only way in.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TlabTuning {
    /// `CRATONVM_TLAB_FILLER_SKIP_ZERO` — [`Tlab::install_tail_filler`] writes
    /// the `int[]` filler HEADER but not the memset of its data area, on a
    /// [`TlabAccounting::JavaThread`] buffer whose tail tripwire read zero.
    /// Write (3) of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    filler_skip_zero: bool,
    /// `CRATONVM_TLAB_SHARE_SIZER` — the refill-waste limit
    /// ([`Tlab::keep_on_miss`]) is armed on this buffer. gen r4w4/alloc4. The
    /// sizing half is decided per refill by the refill site, which reads the
    /// same switch (a thread's first buffer is `Tlab::empty()`, which latched
    /// nothing).
    ///
    /// gen r5w4/defaults8: the switch's default is per backend now, and
    /// `Tlab::new` has no heap in hand, so the carve latches the heap-less
    /// reading ([`cratonvm_types::GcFlags::tlab_share_sizer_for`]`(false)`:
    /// on only when explicitly set) and the refill site, which knows the heap,
    /// re-arms the buffer it just installed with the heap's answer
    /// ([`Tlab::arm_share_sizer`], from `VmHeap::tlab_share_sizer_enabled`).
    share_sizer: bool,
}

impl TlabTuning {
    /// The policies in force for a buffer carved now.
    fn latched() -> Self {
        let gc = &cratonvm_types::flags().gc;
        Self {
            filler_skip_zero: gc.tlab_filler_skip_zero,
            // No heap here: the non-Generational default (off unless set).
            // The refill site re-arms per heap — see the field doc.
            share_sizer: gc.tlab_share_sizer_for(false),
        }
    }
}

/// A bounded batch of VM-level TLAB observations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlabAllocationBatch {
    pub bytes: u64,
}

const VM_ALLOCATION_BATCH_BYTES: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// TLAB waste census (gengc-round1-alloc, 2026-09-20)
// ---------------------------------------------------------------------------
//
// # Why this exists
//
// `docs/gc-tuning.md` says of the 256 KiB baseline that it raises throughput
// "without meaningfully increasing tail waste (typical thread carries one
// TLAB; waste bounded by `MAX_TLAB_SIZE`)", and `DEFAULT_TLAB_SIZE`'s own
// comment repeats it. Nothing in this tree could have measured either claim:
// there was no counter anywhere for the bytes a retire gives up, and the
// adaptive sizer ([`TlabPressureTracker`]) reacts to fill TIME and allocation
// COUNT and never to waste at all. "Waste is bounded by MAX_TLAB_SIZE per
// thread" is a worst case, not an observation, and the number that decides
// whether 256 KiB is the right baseline -- what fraction of the bytes carved
// out of the young arena the program actually asked for -- was unavailable.
//
// So: one relaxed `fetch_add` per RETIRE (not per allocation -- a retire is
// one event per ~256 KiB of allocation, so this is several orders of
// magnitude off the bump path) and the same per tail filler. Diagnostics
// only; nothing reads these to make a decision.
//
// The census is process-wide and spans every backend, because every backend's
// TLAB retires through this file. A reader that wants one collector's share
// should sample the difference across a window rather than the total.
static TLAB_RETIRES: AtomicU64 = AtomicU64::new(0);
static TLAB_CARVED_BYTES: AtomicU64 = AtomicU64::new(0);
static TLAB_CONSUMED_BYTES: AtomicU64 = AtomicU64::new(0);
static TLAB_FILLER_TAIL_BYTES: AtomicU64 = AtomicU64::new(0);
static TLAB_GAP_TAIL_BYTES: AtomicU64 = AtomicU64::new(0);
/// Retires that gave up at least HALF the buffer they were carved with.
///
/// gengc-round2-alloc2, 2026-09-20. The aggregate `carved - consumed` says how
/// much is lost; it cannot say whether that is a thin tail on every retire or a
/// fat one on a few, and those want opposite fixes. This is the shape counter,
/// and it is the precondition measurement for
/// `gengc-alloc-tlab-sizer-is-blind-to-waste-DONE-20260929.md` step 2: a shrink
/// clause driven by tail size is worth adding exactly when this number is a
/// material fraction of `retires`, and is an unfireable clause when it is not.
///
/// Half, rather than the sizer's 75 % `well_used` threshold, on purpose: a
/// drained TLAB reports a short tail because the last object that did not fit
/// is what ends its life. That tail is below the missed object, so below
/// `TLAB_MAX_ALLOC` (32 KiB) — but "so a drain never reaches half" holds only
/// for a buffer of at least 64 KiB, and the ladder, the retired arm and the
/// share sizer all hand out 8-32 KiB buffers. gce e1/o
/// (`gcd-d5q-wasteful-retire-census-counts-small-drains-FIXED-20260929.md`): a
/// retire counts here only when its tail is ALSO at least `TLAB_MAX_ALLOC`
/// ([`retire_counts_as_wasteful`]), which no drain's tail can be, so every hit
/// is an early retire (a park, a blocking native, a thread exit, a forced GC).
/// An early retire of a buffer under 64 KiB is not counted either; the count
/// is a lower bound on early retires, never inflated by drains.
static TLAB_WASTEFUL_RETIRES: AtomicU64 = AtomicU64::new(0);
static TLAB_SINK_TAIL_BYTES: AtomicU64 = AtomicU64::new(0);

// The share sizer's census (gen r4w4/alloc4), same kind and cost as the waste
// census above: diagnostics only, one relaxed add per REFILL or per
// refill-waste keep, never per allocation. Printed as the `[GC] tlab-sizer:`
// line (see `tlab_census_lines`). The shape a HotSpot `-Xlog:gc+tlab` reader
// knows — refills, slow (outside-the-TLAB) allocations, the desired-size
// spread — as run totals rather than per-collection lines.
/// Granted refills the share sizer sized (`Tlab::note_share_refill`).
static TLAB_SIZER_REFILLS: AtomicU64 = AtomicU64::new(0);
/// Sum of the sizes those refills were GRANTED (`/ refills` = the mean chunk;
/// below the desired size when the young tail or a free block was shorter).
static TLAB_SIZER_GRANTED_BYTES: AtomicU64 = AtomicU64::new(0);
/// Smallest / largest desired size of a granted sized refill.
static TLAB_SIZER_MIN_DESIRED: AtomicU64 = AtomicU64::new(u64::MAX);
static TLAB_SIZER_MAX_DESIRED: AtomicU64 = AtomicU64::new(0);
/// Windows closed by a collection: exponential-average samples taken.
static TLAB_SIZER_SAMPLES: AtomicU64 = AtomicU64::new(0);
/// In-window doublings of an underpredicted thread.
static TLAB_SIZER_RAISES: AtomicU64 = AtomicU64::new(0);
/// Refill-waste-limit keeps: misses allocated OUTSIDE the buffer (HotSpot's
/// "slow allocs"), and their bytes.
static TLAB_SIZER_KEEPS: AtomicU64 = AtomicU64::new(0);
static TLAB_SIZER_KEEP_BYTES: AtomicU64 = AtomicU64::new(0);

/// The share sizer's run totals — see the statics above.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TlabSizerStats {
    /// Granted refills the share sizer sized.
    pub refills: u64,
    /// Mean size those refills were granted (0 when none).
    pub mean_granted: u64,
    /// Smallest desired size requested (0 when none).
    pub min_desired: u64,
    /// Largest desired size requested.
    pub max_desired: u64,
    /// Exponential-average samples (collection-closed windows).
    pub samples: u64,
    /// Underpredicted in-window raises.
    pub raises: u64,
    /// Refill-waste-limit keeps, and their bytes.
    pub keeps: u64,
    pub keep_bytes: u64,
}

/// The share sizer's census. See [`TlabSizerStats`].
pub fn tlab_sizer_stats() -> TlabSizerStats {
    let refills = TLAB_SIZER_REFILLS.load(Ordering::Relaxed);
    let granted = TLAB_SIZER_GRANTED_BYTES.load(Ordering::Relaxed);
    let min = TLAB_SIZER_MIN_DESIRED.load(Ordering::Relaxed);
    TlabSizerStats {
        refills,
        mean_granted: granted.checked_div(refills).unwrap_or(0),
        min_desired: if min == u64::MAX { 0 } else { min },
        max_desired: TLAB_SIZER_MAX_DESIRED.load(Ordering::Relaxed),
        samples: TLAB_SIZER_SAMPLES.load(Ordering::Relaxed),
        raises: TLAB_SIZER_RAISES.load(Ordering::Relaxed),
        keeps: TLAB_SIZER_KEEPS.load(Ordering::Relaxed),
        keep_bytes: TLAB_SIZER_KEEP_BYTES.load(Ordering::Relaxed),
    }
}

/// What every retired TLAB in this process was carved with and what the
/// program actually asked for out of it. See the census note above.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TlabWasteStats {
    /// Retires that actually held a chunk (an idempotent second retire of an
    /// already-retired buffer is not counted -- see [`Tlab::retire`]).
    pub retires: u64,
    /// `end - start` summed over those retires: the bytes the arena
    /// handed to a thread.
    pub carved_bytes: u64,
    /// `cursor - start` at retire, read BEFORE any tail filler moved the
    /// cursor: the bytes the program asked for, JIT inline bumps included.
    pub consumed_bytes: u64,
    /// Tail bytes covered by an `int[]` [`TLAB_FILLER_CLASS_ID`] filler.
    pub filler_tail_bytes: u64,
    /// Tail bytes covered by a sub-header [`GAP_FILLER_CLASS_ID`] sentinel.
    pub gap_tail_bytes: u64,
    /// Tail bytes a registered [`TlabTailSink`] took back onto a free list
    /// instead. These are NOT waste -- the allocator gets them back -- which
    /// is exactly why they are counted apart from the two filler buckets.
    pub sink_tail_bytes: u64,
    /// Retires that gave up at least half their buffer. See
    /// [`TLAB_WASTEFUL_RETIRES`].
    pub wasteful_retires: u64,
}

impl TlabWasteStats {
    /// Carved bytes the program never asked for. Includes the sink-reclaimed
    /// tails, which are recovered rather than lost; subtract
    /// [`Self::sink_tail_bytes`] for the lost-for-a-cycle figure.
    pub fn unused_bytes(&self) -> u64 {
        self.carved_bytes.saturating_sub(self.consumed_bytes)
    }

    /// [`Self::unused_bytes`] as a percentage of what was carved, or 0.0 when
    /// nothing has been carved yet.
    pub fn unused_percent(&self) -> f64 {
        if self.carved_bytes == 0 {
            return 0.0;
        }
        (self.unused_bytes() as f64) * 100.0 / (self.carved_bytes as f64)
    }
}

/// `(filler_over_object, refill_over_object)` — the two TLAB tripwires.
///
/// gengc-round1-alloc, 2026-09-20: both counters were WRITE-ONLY. Nothing in
/// the workspace loaded [`FILLER_OVER_OBJECT`] or [`REFILL_OVER_OBJECT`], so
/// the conditions their own docs call "a use-after-free in waiting, not a
/// diagnostic curiosity" could fire on every collection of a run and leave no
/// trace anywhere but the first eight `tracing::error!` lines — which a
/// release run with the default subscriber may never show. An instrument that
/// is never read reports zero for the same reason an unfireable one does, a
/// distinction this file's own `note_region_leak` sibling in `arena.rs` was
/// caught by. This is the reader; wiring it into the shutdown summary is
/// `gc_metrics`' side of the fence.
pub fn tlab_tripwire_counts() -> (u64, u64) {
    (
        FILLER_OVER_OBJECT.load(Ordering::Relaxed),
        REFILL_OVER_OBJECT.load(Ordering::Relaxed),
    )
}

/// The process-wide TLAB waste census. See [`TlabWasteStats`].
pub fn tlab_waste_stats() -> TlabWasteStats {
    TlabWasteStats {
        retires: TLAB_RETIRES.load(Ordering::Relaxed),
        carved_bytes: TLAB_CARVED_BYTES.load(Ordering::Relaxed),
        consumed_bytes: TLAB_CONSUMED_BYTES.load(Ordering::Relaxed),
        filler_tail_bytes: TLAB_FILLER_TAIL_BYTES.load(Ordering::Relaxed),
        gap_tail_bytes: TLAB_GAP_TAIL_BYTES.load(Ordering::Relaxed),
        sink_tail_bytes: TLAB_SINK_TAIL_BYTES.load(Ordering::Relaxed),
        wasteful_retires: TLAB_WASTEFUL_RETIRES.load(Ordering::Relaxed),
    }
}

/// The two `[GC]` shutdown lines for the TLAB census and the TLAB tripwires,
/// ready to print.
///
/// # Why this is a formatter and not a print
///
/// `tlab.rs` has no heap handle and no output policy — that is the stated
/// reason `WATCH_COLLECTION` is a static here in the first place — so the pull
/// belongs to `gc_metrics` / `VmHeap::print_gc_summary`, which owns the
/// `[GC] …:` block and the `--verbose:gc` gate. Neither file is this
/// reviewer's to edit, so this is the half that is: one call, no arguments,
/// the wording already settled. See
/// `docs/feature-designs/generative-gc-architecture.md`
/// for the insertion point.
///
/// # The two lines are NOT the same kind of thing
///
/// `tlab-waste` is tuning: print it under `--verbose:gc`. `tlab-guard` is
/// CORRECTNESS — `FILLER_OVER_OBJECT`'s own doc calls a non-zero count "a
/// use-after-free in waiting, not a diagnostic curiosity", and the comment at
/// its write site names the live lambda capture at `0x200868400d8` that
/// reached `invokevirtual` as an all-zero header. A run can trip it on every
/// collection today and finish looking clean, because the only trace is the
/// first eight `tracing::error!` lines on a target nothing greps. So the guard
/// line should be printed **whenever either count is non-zero**, verbose or
/// not; [`tlab_guard_line_is_urgent`] is that predicate. A zero-valued guard
/// line under `--verbose:gc` is still worth having — it is what distinguishes
/// "the tripwire says no" from "nobody asked".
pub fn tlab_census_lines() -> [String; 2] {
    // gen r5w4/defaults8: the heap-less reading of the share sizer's switch
    // (its non-Generational default). The Generational summary calls
    // `tlab_census_lines_for(true)`.
    tlab_census_lines_for(false)
}

/// [`tlab_census_lines`]' body, with the `[GC] tlab-sizer:` line already
/// rendered for the caller's heap family.
fn census_lines_with_sizer(sizer: String) -> [String; 2] {
    let w = tlab_waste_stats();
    let (filler_over, refill_over) = tlab_tripwire_counts();
    // gen r4w4/alloc4: the TUNING element is two physical lines — the waste
    // census and, after a newline, the share sizer's (`tlab_sizer_line`) —
    // so every summary that prints the tuning line prints the sizer's too,
    // with no second call to wire. A grep for `^\[GC\] tlab-waste:` still
    // matches the first line exactly.
    [
        format!(
            // LANE W6-A — `tail_sinks=` is `sink_tail=`'s denominator and it
            // goes on the same line, not a neighbouring one.
            //
            // `reclaim_tail_via_sinks` opens with `if TAIL_SINK_COUNT == 0 {
            // return false }`, so with no sink registered `sink_tail` is zero
            // by construction and no retire ever reaches a sink to decline.
            // "No heap offered to take tails back" and "the heap took none of
            // the tails offered" print as the same zero and are different
            // facts — the first says the mechanism is absent, the second says
            // it is present and idle. `tlab_tail_sink_count` existed to tell
            // them apart and had no caller anywhere in the workspace
            // (`w3c-instrumentation-audit.md` §6, routed by W4-B, closed here).
            "[GC] tlab-waste: retires={} wasteful_retires={} carved={} consumed={} \
             unused={} ({:.1}%) filler_tail={} gap_tail={} sink_tail={} tail_sinks={}\n{sizer}",
            w.retires,
            w.wasteful_retires,
            w.carved_bytes,
            w.consumed_bytes,
            w.unused_bytes(),
            w.unused_percent(),
            w.filler_tail_bytes,
            w.gap_tail_bytes,
            w.sink_tail_bytes,
            tlab_tail_sink_count(),
        ),
        format!(
            "[GC] tlab-guard: filler_over_object={filler_over} refill_over_object={refill_over}"
        ),
    ]
}

/// [`tlab_census_lines`] for a summary that knows its heap: the
/// `sizer_on=` it prints is the share sizer's setting for that heap family
/// (gen r5w4/defaults8 — its default is per backend, ON on the Generational
/// heap). [`tlab_census_lines`] is this with `generational = false`, which is
/// right for every caller that has no Generational heap in hand.
pub fn tlab_census_lines_for(generational: bool) -> [String; 2] {
    census_lines_with_sizer(tlab_sizer_line(generational))
}

/// gci i48/zl3: [`tlab_census_lines_for`] with the G1 heap told apart from
/// ZGC, so a G1 summary's `sizer_on=` is G1's default
/// (`alloc_policy_defaults::TLAB_SHARE_SIZER_G1`, ON) and a ZGC one's stays
/// OFF. `g1 == false` is `tlab_census_lines_for(generational)`.
pub fn tlab_census_lines_for_backend(generational: bool, g1: bool) -> [String; 2] {
    census_lines_with_sizer(tlab_sizer_line_for_backend(generational, g1))
}

/// The `[GC] tlab-sizer:` line: whether the share sizer is on for a heap of
/// this family (`generational`, see [`tlab_census_lines_for`]), and its run
/// totals (gen r4w4/alloc4). Printed as the second physical line of
/// [`tlab_census_lines`]'s tuning element.
///
/// Read against HotSpot's `-Xlog:gc+tlab` totals: `sizer_refills=` is
/// `refills:`, `sizer_keeps=` / `sizer_keep_bytes=` are the `slow allocs:` the
/// refill-waste limit sent outside the TLAB, and the desired-size spread
/// (`sizer_min_desired=` .. `sizer_max_desired=`, `sizer_mean_granted=`) is
/// what HotSpot prints per thread at `trace` level. `sizer_samples=` counts
/// collection-closed windows; `sizer_raises=` the in-window doublings. The
/// waste itself is the `[GC] tlab-waste:` line directly above. With the sizer
/// off every count is 0 — `sizer_on=false` says it was not asked, not that it
/// found nothing.
pub fn tlab_sizer_line(generational: bool) -> String {
    tlab_sizer_line_for_backend(generational, false)
}

/// gci i48/zl3: [`tlab_sizer_line`] with the G1 heap told apart from ZGC
/// (`GcFlags::tlab_share_sizer_for_backend`, the answer
/// `VmHeap::tlab_share_sizer_enabled` gives the refill site).
pub fn tlab_sizer_line_for_backend(generational: bool, g1: bool) -> String {
    let s = tlab_sizer_stats();
    let on = cratonvm_types::flags()
        .gc
        .tlab_share_sizer_for_backend(generational, g1);
    format!(
        "[GC] tlab-sizer: sizer_on={on} sizer_target_refills={} sizer_refills={} \
         sizer_mean_granted={} sizer_min_desired={} sizer_max_desired={} sizer_samples={} \
         sizer_raises={} sizer_keeps={} sizer_keep_bytes={}",
        TLAB_TARGET_REFILLS,
        s.refills,
        s.mean_granted,
        s.min_desired,
        s.max_desired,
        s.samples,
        s.raises,
        s.keeps,
        s.keep_bytes,
    )
}

/// gen r5w2/alloc6 — does the first word of a retiring TLAB's reserved tail
/// read zero? The gate [`Tlab::retire`] puts in front of the ATTACHED tail
/// sink: a tail is zero by construction, and a non-zero first word means the
/// cursor sits on an object already handed out, so the span must not go back
/// to the allocator (see the call site). The same word `install_tail_filler`'s
/// O(1) tripwire reads.
///
/// # Safety
/// `tail_start` must be 8-aligned with eight readable bytes behind it (the
/// start of a non-empty [`Tlab::reserved_tail`]).
#[inline]
unsafe fn tail_head_reads_zero(tail_start: usize) -> bool {
    // SAFETY: the caller's contract.
    unsafe { std::ptr::read(tail_start as *const u64) == 0 }
}

/// gen r5w5/sizer9 — does the LAST word of a retiring TLAB's reserved tail
/// read zero? The second O(1) half of the attached sink's gate (see
/// [`Tlab::tail_is_clean_for_sink`]).
///
/// # Safety
/// `tail_end` must be 8-aligned with eight readable bytes BELOW it (the end of
/// a non-empty [`Tlab::reserved_tail`], whose span is at least one word).
#[inline]
unsafe fn tail_last_word_reads_zero(tail_end: usize) -> bool {
    // SAFETY: the caller's contract.
    unsafe { std::ptr::read((tail_end - 8) as *const u64) == 0 }
}

/// gen r5w5/sizer9 — does every word of `[start, end)` read zero? The whole-
/// tail form of the sink's gate, used only under `CRATONVM_DBG_DEADREF_STORE`
/// (the mode in which `install_tail_filler` already scans the whole tail).
///
/// # Safety
/// `[start, end)` must be readable, with both ends 8-aligned.
unsafe fn span_reads_zero(start: usize, end: usize) -> bool {
    let words = (end.saturating_sub(start)) / 8;
    // SAFETY: the caller's contract; `i < words` keeps every read inside it.
    (0..words).all(|i| unsafe { std::ptr::read((start as *const u64).add(i)) } == 0)
}

/// Did this retire give up at least half the buffer it was carved with?
///
/// Split out of [`Tlab::finish_retire`] so the threshold is testable without
/// racing the process-wide counter it feeds — every other test in this binary
/// is retiring into [`TLAB_WASTEFUL_RETIRES`] at the same time, so an
/// equality assertion on the counter would be flaky by construction.
#[inline]
fn retire_is_wasteful(carved: u64, consumed: u64) -> bool {
    carved > 0 && consumed.min(carved).saturating_mul(2) <= carved
}

/// The census's rule for [`TLAB_WASTEFUL_RETIRES`]: [`retire_is_wasteful`]
/// AND a tail of at least `TLAB_MAX_ALLOC` bytes (gce e1/o, gcd d5/q's page).
///
/// A DRAIN ends when an object of at most `TLAB_MAX_ALLOC` bytes misses, so
/// its tail is strictly below that; a tail at or above it can only come from
/// an early retire. Without this second term an 8 KiB buffer that drained
/// with a 4.5 KiB tail counted as wasteful, and the ratio two page rules decide
/// on (`wasteful_retires / retires`) mixed small drains into early retires.
#[inline]
fn retire_counts_as_wasteful(carved: u64, consumed: u64) -> bool {
    retire_is_wasteful(carved, consumed) && carved - consumed.min(carved) >= TLAB_MAX_ALLOC as u64
}

/// May [`Tlab::install_tail_filler`] skip the memset of its `int[]` filler's
/// DATA area? gen r4w3/alloc3 (2026-09-23), write (3) of
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// # Why the memset is redundant when this says so
///
/// [`Tlab::new`]'s safety contract hands the buffer over ZEROED (every
/// backend's refill zeroes it, or skips only the arena's proven-never-written
/// pristine range), and the owning thread's cursor only moves forward over
/// what it allocated — `alloc_initialized` writes inside the span it has
/// reserved, and the JIT's inline bump likewise. So `[cursor, end)` still
/// reads zero at retire, and the memset rewrites zeros over zeros: on an
/// early retire that is most of the buffer (up to `MAX_TLAB_SIZE`), paid on
/// the way INTO a safepoint. Walkers stride a filler by its length and never
/// read its data.
///
/// # When it still runs
///
/// * the flag (`CRATONVM_TLAB_FILLER_SKIP_ZERO`, latched per buffer) is off —
///   the default;
/// * the buffer is not a Java thread's ([`TlabAccounting::HeapStaging`]): the
///   heap-internal staging layers' tail provenance was not audited;
/// * the tail tripwire read a non-zero word (`occupant != 0`) — the invariant
///   above is broken for this buffer, and the memset is the repair it has
///   always been. Under `CRATONVM_DBG_DEADREF_STORE` the tripwire scans the
///   WHOLE tail, so there a zero `occupant` is a proof that the skipped memset
///   would have written only zeros; without it, only the first word is known.
#[inline]
fn filler_data_memset_is_redundant(
    filler_skip_zero: bool,
    accounting: TlabAccounting,
    occupant: u32,
) -> bool {
    filler_skip_zero && accounting == TlabAccounting::JavaThread && occupant == 0
}

/// Is the `tlab-guard` line from [`tlab_census_lines`] a finding rather than a
/// gauge? True when either tripwire has fired. See that function's note.
pub fn tlab_guard_line_is_urgent() -> bool {
    let (filler_over, refill_over) = tlab_tripwire_counts();
    filler_over != 0 || refill_over != 0
}

/// Whose allocation a [`Tlab`]'s consumed span represents.
///
/// A `Tlab` is used at two different layers of this tree, and only one of them
/// is measuring Java work:
///
/// * a **Java thread's** own bump buffer (`JvmThread::tlab`), whose consumed
///   span is exactly the bytes the program asked for; and
/// * a **heap-internal staging buffer** that the heap carves objects (or
///   thread TLABs) out of — ZGC's `ZArenaTlab` and `ZTlab` both wrap a `Tlab`
///   by value for its bump path, tail filler and idempotent retire.
///
/// The same byte passes through both layers. Crediting the process-wide total
/// from both counts every allocation twice, which is precisely how
/// `getTotalThreadAllocatedBytes` came to report 3.00x of retained heap under
/// ZGC (the heap-staging layer) against 2.00x under Generational and G1 (no
/// such layer — their remaining 1.00x was the reader adding the calling
/// thread's whole running total on top of a global that already contained it;
/// see `ThreadMXBean::total_allocated_bytes` in `vm/src/vm/vm_exec.rs`).
///
/// The default is [`TlabAccounting::JavaThread`] so a forgotten annotation
/// over-counts loudly rather than under-counting silently — an allocation
/// counter that reads low is the failure mode that passes a budget assertion
/// vacuously, which is the one this module has already been bitten by twice.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TlabAccounting {
    /// A Java thread's own buffer. Credits the owning VM's allocation total
    /// once attached ([`Tlab::attach_vm_thread_allocation_total`]).
    #[default]
    JavaThread,
    /// A heap-internal staging buffer whose span a Java thread's TLAB (or the
    /// VM's `note_external_allocation`) counts again. Never credits an
    /// allocation total, even if attached; its own `thread_alloc_carry` still
    /// moves, because that field is read per-buffer by the ZGC TLAB
    /// statistics.
    HeapStaging,
}

/// zs s5/ta: do this buffer's tail bytes belong in the waste census's
/// `filler_tail` / `gap_tail` buckets? Only a Java thread's buffer, the same
/// gate [`Tlab::finish_retire`] puts on `retires` / `carved` / `consumed`.
///
/// A [`TlabAccounting::HeapStaging`] buffer (ZGC's `ZArenaTlab` cells, the
/// only users) is excluded from `carved`, and its tail does not stay a
/// filler: `ZgcRealHeap::tlab_retire_locked` installs the filler and then
/// puts the same bytes on the arena free list. Counted, they made the
/// `[GC] tlab-waste:` line report recovered bytes as waste
/// (`filler_tail=96768` against `unused=299808` = `sink_tail=299808` on
/// `Zs1NdLauncherOome -Xmx16m`, where every Java-thread tail was returned)
/// and broke the census's `filler + gap + sink <= unused` reading. Diagnostic
/// only: no allocator reads these buckets, and Generational and G1 never build
/// a staging buffer.
#[inline]
fn tail_census_counts(accounting: TlabAccounting) -> bool {
    accounting == TlabAccounting::JavaThread
}

/// Unpublished NON-TLAB bytes a Java thread may hold back from its VM's
/// allocation total before [`Tlab::note_external_allocation`] publishes them.
///
/// # Why a cumulative total, and why per VM
///
/// `com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes` is a total
/// that never decreases. It has to be its own counter. The obvious source, the
/// heap's `allocated_bytes()`, is an occupancy gauge derived from
/// `used - free`, so it FALLS at every collection — and a caller measuring a
/// window that contains a GC gets the difference of two occupancies rather
/// than the bytes it allocated. The same Hibernate HQL parse read 488 MB under
/// ZGC, 49 MB under Generational at a 2 GB heap, and 458 MB under Generational
/// at 8 GB. Same bytecode, three answers, none of them cumulative.
///
/// The total is fed from the two places a thread's own total is fed (`retire`
/// rolling in the consumed span, and `note_external_allocation`), so it is the
/// sum of every thread's published total. Until gc-common w6-c it was the
/// process static `PROCESS_ALLOCATED_BYTES`, so in an embedding VM A's answer
/// grew while A was idle and VM B allocated, and every allocating thread of
/// every VM shared one cache line. It is now the owning VM's
/// `HeapRealm::thread_allocated_total`
/// (`common-f-process-global-allocation-state`).
///
/// # The batch, and why the reader stays exact for its own thread
///
/// Every native allocation that misses the TLAB (each object from the native
/// pool, each boxed value, each collection resize) is noted here, so an
/// immediate publish was one shared `lock xadd` per OBJECT on the native path
/// (the w2-d addendum on that page). The note now publishes only once the
/// thread's unpublished carry reaches this batch; `retire` always publishes
/// everything. The reader (`NativeContextImpl::total_allocated_bytes`) adds
/// the calling thread's [`Tlab::vm_thread_unpublished_bytes`], its live TLAB
/// span plus this held-back carry, so the calling thread's own allocation is
/// always fully visible. Another thread's in-flight bytes were never visible
/// (its live span cannot be read cross-thread); this adds at most one batch to
/// that per-thread bound, which is already up to one TLAB (1 MiB).
///
/// Only buffers marked [`TlabAccounting::JavaThread`] feed a total. A
/// heap-internal staging buffer's span is re-counted one layer up, so
/// crediting it double-counts every byte — see [`TlabAccounting`].
const EXTERNAL_PUBLISH_BATCH_BYTES: u64 = 64 * 1024;

/// What the single-pass JIT's INLINE ZGC start-bit store reads (round 9 wave
/// 8, lane `zgc8`; `perf-zgc-compiled-new-always-takes-the-rust-helper`).
///
/// # Why it exists
///
/// On ZGC every compiled `new` of a post-init no-op class ends in a call to
/// the announce helper (`jit_zgc_note_tlab_object` ->
/// `ZgcRealHeap::note_tlab_object`), whose only job on the common path is ONE
/// plain `or` into the object-start bitmap: the owned-word path of
/// `ZObjectStarts::insert_in_owned_chunk`. The call itself (prologue, panic
/// containment, `note_jit_boundary`, the thread-local ownership read) was
/// ~19 % of `CratonBench bintrees` in the wave-7 profile. With this table the
/// JIT performs that same `or` inline and keeps the helper for every other
/// case.
///
/// # Contract (the JIT's copies of these offsets live in
/// `jit/src/x64/objects.rs` and are pinned by a test there)
///
/// The inline store is taken only when ALL of these hold, otherwise the JIT
/// calls the helper exactly as before:
///
/// 1. the thread's `Tlab::zgc_announce` is non-zero (it is this table's
///    address, set by [`Tlab::new`] only for the chunk this thread just carved
///    through `ZgcRealHeap::refill_tlab` while the table was published);
/// 2. `Tlab::zgc_owned_epoch == epoch` -- the thread's ownership of
///    `[start, end)` is still valid (`zgc::vm_tlab::OwnedVmTlabChunk`: no
///    collection and no foreign tail reclaim since the carve);
/// 3. `blocked == 0` -- no concurrent mark is running (allocate-black) and
///    the heap is not generational (`note_young_page`);
/// 4. `words != 0` and `base <= obj < base + span` with `obj - base` on the
///    8-byte grid (the registry's `HeapBitmap::locate`);
/// 5. the bitmap word covering `obj` spans 512 bytes lying wholly inside
///    `[start, end)` -- the exact test `insert_in_owned_chunk` makes for its
///    plain store.
///
/// Then it sets bit `(obj - base) / 8` of `words` with an unlocked `or`:
/// the same load/or/store `insert_in_owned_chunk` performs, with the same
/// soundness argument (the word can only hold bases of objects this thread
/// bumps into its own chunk; the collector's writers run with the mutator
/// stopped at a poll, and the sequence has none).
///
/// # Publication
///
/// One heap owns the table at a time (`owner` = its arena base; first come,
/// owner-checked withdrawal in `Drop`), like `JIT_READ_BOUNDS`. It publishes
/// only with `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` on, the bitmap registry, owned
/// starts on and no forensic mode that the helper serves
/// (`CRATONVM_DBG_ZGC_CORPSE`, `a2dbg`). `words` is written LAST on publish
/// and FIRST on withdrawal.
///
/// `epoch` is the VM-TLAB ownership epoch itself (it used to be a private
/// static in `zgc::vm_tlab`); it lives here so the JIT can compare it inline.
#[repr(C)]
pub struct JitZgcAnnounceTable {
    /// Object-start bitmap word array (`HeapBitmap::words`), 0 = unpublished.
    pub words: AtomicUsize,
    /// Address bit 0 of word 0 denotes (`HeapBitmap::base`).
    pub base: AtomicUsize,
    /// Bytes covered from `base` (`HeapBitmap::span`).
    pub span: AtomicUsize,
    /// Non-zero while the owner is marking or generational.
    pub blocked: AtomicUsize,
    /// The VM-TLAB ownership epoch. Starts at 1, so an empty record (0) never
    /// matches.
    pub epoch: AtomicU64,
    /// The publishing heap's arena base, 0 when unowned.
    pub owner: AtomicUsize,
    /// zr z9/bo, the SHORT inline start-bit store
    /// (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`): zero exactly when that form may
    /// set a bit -- the table is published (`words != 0`), [`Self::blocked`]
    /// is zero, and the bitmap's `base` is 512-byte aligned, so a bitmap word's
    /// 512-byte granule is the address's own (`obj & !511`). Written with
    /// `blocked` (`set_jit_inline_announce_blocked`), set to 1 FIRST on
    /// withdrawal, and 1 in the static, so the short form needs no `words`,
    /// `base`, `span` or grid test of its own: an armed buffer (condition 1-2
    /// of the contract above) lies inside `[base, base + span)` by its arming
    /// (`zgc_inline_announce_for_chunk`), and every object the JIT announces is
    /// an 8-aligned bump of that buffer.
    pub short_blocked: AtomicUsize,
    /// zr z9/bo: `words - (base >> 9) * 8` (wrapping), so the bitmap word
    /// covering `obj` is at `granule_bias + (obj >> 9) * 8` when `base` is
    /// 512-aligned. Meaningful only while [`Self::short_blocked`] is 0; written
    /// before it opens.
    pub granule_bias: AtomicUsize,
    // zt t12/rt: `epoch_gate` (offset 64, collector-only, zs s2/ca2's
    // `CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE`) was the last field; retired
    // with its switch. No JIT offset moved.
}

impl JitZgcAnnounceTable {
    /// Byte offset of [`Self::words`]. The JIT's copy: `ZGC_ANNOUNCE_WORDS`.
    pub const WORDS_OFFSET: usize = 0;
    /// Byte offset of [`Self::base`]. The JIT's copy: `ZGC_ANNOUNCE_BASE`.
    pub const BASE_OFFSET: usize = 8;
    /// Byte offset of [`Self::span`]. The JIT's copy: `ZGC_ANNOUNCE_SPAN`.
    pub const SPAN_OFFSET: usize = 16;
    /// Byte offset of [`Self::blocked`]. The JIT's copy: `ZGC_ANNOUNCE_BLOCKED`.
    pub const BLOCKED_OFFSET: usize = 24;
    /// Byte offset of [`Self::epoch`]. The JIT's copy: `ZGC_ANNOUNCE_EPOCH`.
    pub const EPOCH_OFFSET: usize = 32;
    /// Byte offset of [`Self::short_blocked`]. The JIT's copy:
    /// `ZGC_ANNOUNCE_SHORT_BLOCKED`.
    pub const SHORT_BLOCKED_OFFSET: usize = 48;
    /// Byte offset of [`Self::granule_bias`]. The JIT's copy:
    /// `ZGC_ANNOUNCE_GRANULE_BIAS`.
    pub const GRANULE_BIAS_OFFSET: usize = 56;

    /// zr z9/bo: `(granule_bias, short form may open)` for a bitmap of
    /// `words` covering from `base`. The short store derives a word's granule
    /// as `obj & !511`, which is the bitmap's own granule only when `base` is
    /// 512-aligned; any other base keeps the short form closed (the long form
    /// and the helper are unaffected).
    pub fn short_geometry(words: usize, base: usize) -> (usize, bool) {
        let bias = words.wrapping_sub((base >> 9) << 3);
        (bias, words != 0 && base & 511 == 0)
    }
}

/// The one [`JitZgcAnnounceTable`].
pub static JIT_ZGC_ANNOUNCE: JitZgcAnnounceTable = JitZgcAnnounceTable {
    words: AtomicUsize::new(0),
    base: AtomicUsize::new(0),
    span: AtomicUsize::new(0),
    blocked: AtomicUsize::new(0),
    epoch: AtomicU64::new(1),
    owner: AtomicUsize::new(0),
    short_blocked: AtomicUsize::new(1),
    granule_bias: AtomicUsize::new(0),
};

/// Address of [`JIT_ZGC_ANNOUNCE`], as stored in `Tlab::zgc_announce`.
pub fn jit_zgc_announce_table_addr() -> usize {
    &JIT_ZGC_ANNOUNCE as *const JitZgcAnnounceTable as usize
}

/// zr z11/ia: the start-bitmap words a buffer over `[lo, hi)` owns whole,
/// for a bitmap covering `span` bytes from `base`: `[first, past_last)` word
/// indices from `base`, word `k` covering `[base + 512k, base + 512(k + 1))`.
/// `first = ceil((lo - base) / 512)` and `past_last = floor((hi - base) /
/// 512)`, so `first <= k < past_last` is exactly "`base + 512k >= lo` and
/// `hi - (base + 512k) >= 512`", the round-9 inline store's two granule tests.
///
/// Empty (`[0, 0)`) when the buffer is not inside `[base, base + span)` or the
/// span has more than `u32::MAX` words: the JIT compares the index's LOW
/// dword, which is the index only while every index of the span fits.
pub fn zgc_owned_word_window(base: usize, span: usize, lo: usize, hi: usize) -> [u32; 2] {
    if lo < base || hi < lo || hi - base > span || u32::try_from(span >> 9).is_err() {
        return [0, 0];
    }
    let first = (lo - base).div_ceil(512);
    let past_last = (hi - base) / 512;
    match (u32::try_from(first), u32::try_from(past_last)) {
        (Ok(f), Ok(p)) if f < p => [f, p],
        _ => [0, 0],
    }
}

/// `(zgc_owned_epoch, zgc_announce, zgc_owned_words)` for a buffer over
/// `[lo, hi)`.
fn zgc_inline_announce_for_chunk(lo: usize, hi: usize) -> (u64, usize, [u32; 2]) {
    if JIT_ZGC_ANNOUNCE.words.load(Ordering::Acquire) == 0 {
        return (0, 0, [0, 0]);
    }
    // Only a chunk of the PUBLISHING heap. zs s2/rv: this IS part of what
    // makes the inline store sound. Neither JIT store re-checks an object
    // against `[base, base + span)` any more (the short form never did; the
    // long one dropped it in zs s1/ca, `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST`),
    // so this filter and `zgc_owned_word_window`'s in-span test (an empty
    // window sends the store to the helper) are what keep the inline `or`
    // inside the bitmap. Do not relax it.
    let base = JIT_ZGC_ANNOUNCE.base.load(Ordering::Acquire);
    let span = JIT_ZGC_ANNOUNCE.span.load(Ordering::Acquire);
    if lo < base || hi > base.saturating_add(span) {
        return (0, 0, [0, 0]);
    }
    match call_owned_vm_tlab_chunk_epoch(lo, hi) {
        Some(epoch) => (
            epoch,
            jit_zgc_announce_table_addr(),
            zgc_owned_word_window(base, span, lo, hi),
        ),
        None => (0, 0, [0, 0]),
    }
}

/// Without the ZGC backend there is nothing to announce to.

impl Tlab {
    /// Byte offset of the `cursor` field from the start of `Tlab`.
    ///
    /// Read by the JIT-emitted inline TLAB bump in `jit/src/x64.rs`
    /// and verified at runtime by the `test_tlab_offsets` unit test.
    pub const CURSOR_OFFSET: usize = 0;

    /// Byte offset of the `end` field from the start of `Tlab`.
    ///
    /// Read by the JIT-emitted inline TLAB bump in `jit/src/x64.rs`
    /// and verified at runtime by the `test_tlab_offsets` unit test.
    pub const END_OFFSET: usize = 8;

    /// Byte offset of the `start` field. Read by the inline ZGC start-bit
    /// store (`jit/src/x64/objects.rs`, `ZGC_TLAB_START_OFFSET`).
    pub const START_OFFSET: usize = 16;

    /// Byte offset of the `zgc_owned_epoch` field (`ZGC_TLAB_OWNED_EPOCH_OFFSET`
    /// in `jit/src/x64/objects.rs`).
    pub const ZGC_OWNED_EPOCH_OFFSET: usize = 24;

    /// Byte offset of the `zgc_announce` field (`ZGC_TLAB_ANNOUNCE_OFFSET` in
    /// `jit/src/x64/objects.rs`).
    pub const ZGC_ANNOUNCE_OFFSET: usize = 32;

    /// Byte offset of the `zgc_owned_words` window (zr z11/ia;
    /// `ZGC_TLAB_OWNED_WORDS_OFFSET` in `jit/src/x64/objects.rs`): the first
    /// owned word index at `+0`, one past the last at `+4`.
    pub const ZGC_OWNED_WORDS_OFFSET: usize = 40;

    /// BUG-03 — the still-reserved, un-allocated tail of this TLAB as an
    /// absolute `(cursor, end)` address pair, or `None` if the TLAB is empty
    /// (retired / never refilled).
    ///
    /// Used by the cross-thread STW JIT root scan: a peer thread that was
    /// forcibly stopped while executing JIT code never reached a safepoint to
    /// `retire` its TLAB, so its un-filled tail `[cursor, end)` would desync
    /// the non-moving sweep's linear heap walk. The collector reads this tail
    /// (the peer is OS-suspended, so the read is race-free; the JIT commits
    /// the bump cursor only AFTER writing the full object header, so `cursor`
    /// is the linearization point and `[cursor, end)` wholesale-covers any
    /// in-flight object) and skips it during the sweep. Returns `None` for an
    /// empty TLAB so a retired/parked thread contributes no skip region.
    pub fn reserved_tail(&self) -> Option<(usize, usize)> {
        let c = self.cursor as usize;
        let e = self.end as usize;
        if c == 0 || e == 0 || c >= e {
            return None;
        }
        // TLAB AUDIT (tlab-and-card-audit.md): the consumer of this
        // pair, `GenerationalHeap::jit_tlab_skip_offsets`, SILENTLY DROPS any
        // region whose start is not 8-aligned. A dropped region is not a
        // conservative degrade — the sweep then walks the un-retired tail as if
        // it held objects, which is the exact desync `reserved_tail` exists to
        // prevent. Every production allocation path calls
        // `alloc_initialized(_, 8, _)`, so the cursor is 8-aligned by
        // convention; nothing enforced it, and `Tlab::alloc` is public with a
        // caller-supplied alignment.
        //
        // Debug builds name the violation. Release builds round the published
        // start UP to the next 8-byte boundary, which is the fail-safe
        // direction: the published span shrinks by at most 7 bytes of
        // inter-object padding (never into a live object, because the padding
        // follows the last allocation), and the region survives the consumer's
        // alignment filter instead of vanishing from it.
        debug_assert!(
            c & 7 == 0,
            "Tlab::reserved_tail: cursor {c:#x} is not 8-aligned — \
             GenerationalHeap::jit_tlab_skip_offsets would DROP this region and the \
             non-moving sweep would walk the un-retired tail as objects. Every \
             allocation into a TLAB must use align >= 8.",
        );
        let c = (c + 7) & !7usize;
        if c >= e {
            return None;
        }
        Some((c, e))
    }

    /// Has this TLAB been retired (or never refilled)?
    ///
    /// The single predicate the retire protocol is stated in terms of: a
    /// retired TLAB owns no backing memory, publishes no reserved tail, and
    /// serves no allocation. [`Self::retire`] establishes all three and is
    /// idempotent, so a transition path that retires twice (thread termination
    /// after a safepoint park, say) is correct rather than merely tolerated.
    #[inline]
    pub fn is_retired(&self) -> bool {
        self.start.is_null() && self.cursor.is_null() && self.end.is_null()
    }

    /// `(zgc_owned_epoch, zgc_announce)`: whether the JIT's inline ZGC
    /// start-bit store is armed for this buffer, and under which ownership
    /// epoch. `(0, 0)` = not armed. Diagnostics and tests.
    pub fn zgc_inline_announce_state(&self) -> (u64, usize) {
        (self.zgc_owned_epoch, self.zgc_announce)
    }

    /// zr z11/ia: the owned start-bitmap word window the JIT's long inline
    /// store reads (`[first, past_last)` from the table's `base`; `[0, 0)`
    /// when unarmed). Diagnostics and tests.
    pub fn zgc_owned_word_window(&self) -> [u32; 2] {
        self.zgc_owned_words
    }
}

// SAFETY: Tlab pointers are into arena memory owned by the GC heap.
// Each Tlab is exclusive to one thread — no concurrent access.
unsafe impl Send for Tlab {}

impl Tlab {
    /// Create an empty (exhausted) TLAB.
    pub fn empty() -> Self {
        Self {
            start: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
            zgc_owned_epoch: 0,
            zgc_announce: 0,
            zgc_owned_words: [0, 0],
            pressure: TlabPressureTracker::new(),
            thread_alloc_carry: 0,
            published: 0,
            accounting: TlabAccounting::JavaThread,
            dbg_deadref_store: false,
            vm_pending_allocated_bytes: 0,
            vm_allocation_total: None,
            // An empty buffer serves no bump, so it has nothing to watch.
            watch_addr: 0,
            // Nothing to retire and no history to size: the first arm of
            // `refill_request_size` answers before any policy is consulted.
            tuning: TlabTuning::default(),
            gc_waste_words: 0,
            vm_buffer_noted: 0,
            // A thread that has never refilled has no sizing history.
            share: TlabShareSizer::default(),
            vm_thread_total: None,
            tail_sink: None,
            // gci i28/tl: no refill yet.
            cycle: TlabCycleCounters::default(),
        }
    }

    /// An empty buffer that will never credit the process-wide allocation
    /// total. For the heap-internal staging layers — see [`TlabAccounting`].
    ///
    /// An empty TLAB consumes nothing, so this matters only because the
    /// wrappers hold one of these between refills and a reader could otherwise
    /// see the mode flip; the mode is restored at every refill by
    /// [`Self::new_heap_staging`].
    pub fn empty_heap_staging() -> Self {
        Self {
            accounting: TlabAccounting::HeapStaging,
            ..Self::empty()
        }
    }

    /// Create a TLAB backed by the given memory region.
    ///
    /// # Safety
    /// The caller must ensure `ptr..ptr+size` is valid, zeroed, writable
    /// memory that will not be accessed by any other thread until this
    /// TLAB is retired.
    ///
    /// Round-5 HIGH #5: the TLAB tail-filler installer assumes the `end`
    /// pointer is 8-aligned so the synthetic `int[]` filler exactly
    /// covers `[aligned_cursor, end)` without trailing padding. Caller
    /// must therefore supply an `end = ptr.add(size)` that is 8-aligned.
    /// In debug builds we assert that; in release we silently round the
    /// effective end down to 8 (giving up at most 7 bytes of tail) so a
    /// non-aligned caller does not corrupt the heap walker.
    pub unsafe fn new(ptr: *mut u8, size: usize) -> Self {
        let raw_end = ptr.add(size);
        debug_assert!(
            (raw_end as usize) & 7 == 0,
            "Tlab::new: end pointer must be 8-aligned for tail-filler safety \
             (ptr={:p}, size={}, end={:p})",
            ptr,
            size,
            raw_end
        );
        // Release-mode safety net: if the caller violates the alignment
        // contract, trim the TLAB by up to 7 bytes so install_tail_filler
        // can rely on (end - aligned_cursor) being a multiple of 8.
        let aligned_end_addr = (raw_end as usize) & !7usize;
        let effective_size = aligned_end_addr - (ptr as usize);
        let mut pressure = TlabPressureTracker::new();
        pressure.begin_refill(effective_size);
        // Arm the JIT's inline ZGC start-bit store for this chunk when it is
        // exactly the VM-TLAB chunk this thread just carved (and the heap
        // published the table). `(0, 0)` otherwise, which keeps the helper.
        let (zgc_owned_epoch, zgc_announce, zgc_owned_words) =
            zgc_inline_announce_for_chunk(ptr as usize, aligned_end_addr);
        Self {
            start: ptr,
            cursor: ptr,
            end: aligned_end_addr as *mut u8,
            zgc_owned_epoch,
            zgc_announce,
            zgc_owned_words,
            pressure,
            // A fresh buffer knows nothing about what the thread allocated
            // before it. The refill site carries the running total across
            // with `adopt_allocation_total`.
            thread_alloc_carry: 0,
            published: 0,
            accounting: TlabAccounting::JavaThread,
            // Flags are immutable in ordinary execution. Test-only thread
            // overrides apply before constructing the TLAB they exercise.
            dbg_deadref_store: cratonvm_types::flags().gc.dbg_deadref_store,
            vm_pending_allocated_bytes: 0,
            vm_allocation_total: None,
            // Latched here, once per refill, so the bump path reads a field
            // of the thread's own buffer instead of the process-wide cell.
            watch_addr: watch_addr(),
            // gen r4w3/alloc3: latched with the two fields above, for the
            // same reason.
            tuning: TlabTuning::latched(),
            // gci i28/tl: a fresh struct; the refill site carries the
            // outgoing buffer's counters with `adopt_cycle_mark_for_refill`.
            gc_waste_words: 0,
            vm_buffer_noted: 0,
            // A fresh struct knows no history; the refill site carries the
            // outgoing buffer's across with `adopt_share_sizer`.
            share: TlabShareSizer::default(),
            // Attached (and the mark carried across) by the refill site with
            // `attach_vm_thread_allocation_total`, after
            // `adopt_allocation_total`.
            vm_thread_total: None,
            // Attached by the refill site (`attach_tail_sink`) when the heap
            // that carved this chunk takes tails back.
            tail_sink: None,
            cycle: TlabCycleCounters::default(),
        }
    }

    /// [`Self::new`], for a **heap-internal staging buffer** — one that a Java
    /// thread's TLAB, or the VM's `note_external_allocation`, will count the
    /// same bytes out of. Its retires do not credit the process-wide
    /// allocation total. See [`TlabAccounting`].
    ///
    /// This is a separate constructor rather than a setter on purpose: the
    /// wrappers replace their whole `inner: Tlab` at every refill
    /// (`self.inner = Tlab::new(..)`), so a "remember to also call
    /// `mark_heap_staging()`" contract would silently lapse at exactly the
    /// point the mode matters — the same hazard
    /// [`Self::adopt_allocation_total`] documents for the carried total.
    ///
    /// # Safety
    /// Identical to [`Self::new`].
    pub unsafe fn new_heap_staging(ptr: *mut u8, size: usize) -> Self {
        Self {
            accounting: TlabAccounting::HeapStaging,
            // A heap-internal buffer is never the VM thread's buffer, and the
            // JIT never bumps into one: never armed for the inline store.
            zgc_owned_epoch: 0,
            zgc_announce: 0,
            zgc_owned_words: [0, 0],
            ..Self::new(ptr, size)
        }
    }

    /// Whose allocation this buffer's consumed span represents.
    pub fn accounting(&self) -> TlabAccounting {
        self.accounting
    }

    /// Send the SETTLED carry the attached VM total has not seen yet, and move
    /// the high-water mark up -- once at least `min_batch` bytes are pending
    /// (`0` = everything).
    ///
    /// A no-op for a heap-internal staging buffer, whose span is counted again
    /// one layer up -- see [`TlabAccounting`]. A no-op while no VM total is
    /// attached: the pending bytes stay behind the mark, and
    /// [`Self::attach_vm_thread_allocation_total`] credits them.
    ///
    /// "Settled" is [`Self::thread_alloc_carry`], NOT
    /// [`Self::thread_allocated_bytes`]: the LIVE TLAB span is deliberately
    /// left out, because the reader adds the calling thread's live span itself
    /// (it is the one in-flight span it may read). Publishing it here as well
    /// is a double-count.
    fn publish_settled(&mut self, min_batch: u64) {
        if self.accounting != TlabAccounting::JavaThread {
            return;
        }
        let Some(total) = self.vm_thread_total else {
            return;
        };
        let delta = self.thread_alloc_carry.saturating_sub(self.published);
        if delta != 0 && delta >= min_batch {
            self.published = self.thread_alloc_carry;
            // SAFETY: as in `publish_vm_allocation_batch`: the AtomicU64 lives
            // in the owning SharedVm's HeapRealm, which outlives this Java
            // thread and its TLAB; the atomic permits concurrent publishers.
            unsafe { total.as_ref() }.fetch_add(delta, Ordering::Relaxed);
        }
    }

    /// Attach this Java thread's TLAB to its VM's allocation total
    /// (`HeapRealm::thread_allocated_total`, the `getTotalThreadAllocatedBytes`
    /// source; gc-common w6-c). Same lifetime argument as
    /// [`Self::attach_vm_allocation_counter`].
    ///
    /// Call it at the refill site AFTER [`Self::adopt_allocation_total`],
    /// passing the OUTGOING buffer's [`Self::vm_thread_published_bytes`]. The
    /// attach then credits the carry the VM has not seen: nothing when the
    /// outgoing buffer was attached (its retire published everything), and,
    /// at a thread's first attach, whatever it settled on its unattached first
    /// buffer (`Tlab::empty()`), for example a JIT helper's humongous
    /// allocation before the first refill. So no byte is lost to the total
    /// and none is counted twice.
    ///
    /// A no-op on a heap-internal staging buffer: those never feed an
    /// allocation total (see [`TlabAccounting`]).
    pub fn attach_vm_thread_allocation_total(&mut self, total: &AtomicU64, published_before: u64) {
        if self.accounting != TlabAccounting::JavaThread {
            return;
        }
        self.vm_thread_total = Some(std::ptr::NonNull::from(total));
        self.published = published_before.min(self.thread_alloc_carry);
        self.publish_settled(0);
    }

    /// [`Self::attach_vm_thread_allocation_total`] for a buffer that may not
    /// have been attached yet, keeping its own mark: the VM's non-TLAB
    /// allocation sites call it before [`Self::note_external_allocation`], so
    /// a thread that allocates only outside its TLAB (and never refills one)
    /// still reaches its VM's total. One predictable branch once attached.
    #[inline]
    pub fn ensure_vm_thread_allocation_total(&mut self, total: &AtomicU64) {
        if self.vm_thread_total.is_none() {
            let published = self.published;
            self.attach_vm_thread_allocation_total(total, published);
        }
    }

    /// Bytes of [`Self::thread_alloc_carry`] published to the attached VM
    /// total (0 while never attached). The refill site passes the outgoing
    /// buffer's value to [`Self::attach_vm_thread_allocation_total`]; the
    /// accounting tests assert against it, because the VM total itself is not
    /// in this crate.
    pub fn vm_thread_published_bytes(&self) -> u64 {
        self.published
    }

    /// What this thread has allocated that its VM total does NOT yet hold:
    /// the live TLAB span, plus any settled carry held back (a sub-batch of
    /// external bytes, see [`EXTERNAL_PUBLISH_BATCH_BYTES`], or bytes settled
    /// while unattached). The `getTotalThreadAllocatedBytes` reader adds this
    /// to the VM total, so the calling thread's own allocation is always fully
    /// visible.
    pub fn vm_thread_unpublished_bytes(&self) -> u64 {
        self.thread_allocated_bytes().saturating_sub(self.published)
    }

    /// Carry a thread's running allocation total onto a freshly-built TLAB.
    ///
    /// Call this immediately after `thread.tlab = Tlab::new(…)`, passing the
    /// **outgoing** TLAB's [`Self::thread_allocated_bytes`]. Skipping it
    /// resets the thread's `getThreadAllocatedBytes` to zero at every refill,
    /// which the JMM forbids (the counter is specified as monotonic for the
    /// life of the thread).
    pub fn adopt_allocation_total(&mut self, prior_total: u64) {
        self.thread_alloc_carry = prior_total;
        // The outgoing buffer was RETIRED before it was replaced (the Bug-D
        // tail-filler fix in `gc_and_alloc.rs` requires that for heap-walk
        // reasons of its own), so, if it was attached, it published its whole
        // carry. Adopting the high-water mark alongside the total is what
        // stops the successor from publishing the same bytes a second time:
        // leaving it at 0 would re-send the thread's ENTIRE history at every
        // refill. The refill site then attaches with the outgoing buffer's
        // REAL mark, which differs from `prior_total` only when the outgoing
        // buffer was never attached.
        self.published = prior_total;
    }

    /// Record bytes allocated by this thread that never passed through the
    /// TLAB — humongous objects and arrays, and every post-GC retry that goes
    /// straight to the heap arena.
    ///
    /// Without this the counter would see only TLAB-sized allocations, and a
    /// caller measuring a 1 MiB-per-iteration loop (netty's chunk-reuse
    /// assertion is exactly that) would be told it allocated nothing.
    ///
    /// The thread's own total moves at once; the VM total receives the bytes
    /// in batches of [`EXTERNAL_PUBLISH_BATCH_BYTES`] (and in full at every
    /// retire), not one shared atomic per object.
    #[inline]
    pub fn note_external_allocation(&mut self, bytes: usize) {
        self.thread_alloc_carry = self.thread_alloc_carry.saturating_add(bytes as u64);
        self.publish_settled(EXTERNAL_PUBLISH_BATCH_BYTES);
    }

    /// Total bytes this thread has allocated since it started, in the sense
    /// `com.sun.management.ThreadMXBean.getThreadAllocatedBytes` means it:
    /// the carried total plus whatever the live TLAB has handed out.
    ///
    /// Reading the live span here (rather than only at retire) is what makes
    /// the answer current *and* JIT-aware — compiled code bumps `cursor`
    /// directly and tells no counter about it.
    pub fn thread_allocated_bytes(&self) -> u64 {
        self.thread_alloc_carry
            .saturating_add(self.consumed_bytes() as u64)
    }

    /// Attach this Java thread's TLAB to its VM-local allocation total. The
    /// counter belongs to the `SharedVm`, whose lifetime encloses every live
    /// JvmThread and therefore this TLAB.
    pub fn attach_vm_allocation_counter(&mut self, total: &AtomicU64) {
        self.vm_allocation_total = Some(std::ptr::NonNull::from(total));
        self.flush_vm_allocation_batch();
    }

    /// Offer this buffer's unused tail to `sink` — the heap that carved it —
    /// when it retires, before the process-wide sinks and before the filler
    /// (gen r4w6/tlab6). See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// Per buffer rather than through [`register_tlab_tail_sink`]: the
    /// generational heap is not behind an `Arc` it could hand out a `Weak` to
    /// (it lives inline in the VM's `VmHeap`), and the refill site, which has
    /// the heap in hand, is the one place that knows which heap a chunk came
    /// from. [`Self::new`] starts with no sink, so the attach has to be
    /// repeated at every refill, like the other carries.
    ///
    /// # Safety
    /// `sink` must outlive every [`Self::retire`] of this buffer: the pointer is
    /// kept, not the borrow. The VM's refill site passes the heap inside its
    /// `SharedVm`, which outlives every Java thread and so this buffer — the
    /// same argument [`Self::attach_vm_allocation_counter`] rests on.
    pub unsafe fn attach_tail_sink<S: TlabTailSink>(&mut self, sink: &S) {
        self.tail_sink = Some(TailSinkRef {
            data: sink as *const S as *const (),
            reclaim: reclaim_tail_via::<S>,
        });
    }

    /// Whether [`Self::attach_tail_sink`] armed this buffer. Tests and
    /// diagnostics.
    pub fn has_tail_sink(&self) -> bool {
        self.tail_sink.is_some()
    }

    /// Offer `[start, end)` to the attached sink, if any. `false` when none is
    /// attached or it declined.
    fn reclaim_tail_via_attached_sink(&self, start: usize, end: usize) -> bool {
        match self.tail_sink {
            // SAFETY: `attach_tail_sink`'s contract — `data` is the `&S` it was
            // given, still live at every retire of this buffer, and `reclaim`
            // is `reclaim_tail_via::<S>` for that same `S`.
            Some(sink) => unsafe { (sink.reclaim)(sink.data, start, end) },
            None => false,
        }
    }

    /// The body half of the attached sink's gate (gen r5w5/sizer9), asked
    /// only after the tail's FIRST word read zero.
    ///
    /// # Why the head alone was not enough
    ///
    /// A tail the sink takes goes back to the young allocator: a retraction
    /// makes it bump tail again, which the next carve hands out WITHOUT a
    /// memset (`GenerationalHeap::zero_young_hand_out`: "every byte at or
    /// above the cursor reads zero"), and a free-listed block is handed out
    /// again as reclaimed space. Either way the span's soundness needs the
    /// WHOLE tail to be unused, and gen r5w2/alloc6's gate proved only its
    /// first word. A cursor that is merely BEHIND live data (the objects past
    /// it were placed there by something other than this buffer's bump) has a
    /// zero first word whenever the word at the cursor is padding, a
    /// `java.lang.Object` header of class 0, or an object whose header the
    /// allocator has not written yet — and then the sink would hand those
    /// objects to the next owner, which zeroes or bumps over them. That is the
    /// "live bytes handed out again" shape of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// So: the tail's last word must read zero too (one more load per armed
    /// retire, never per allocation), and under `CRATONVM_DBG_DEADREF_STORE`
    /// — latched per buffer, the mode that already deep-scans every filler
    /// tail and every refill chunk — every word of it. A refusal sends the
    /// retire down the filler path. There the O(1) tripwire reads only the
    /// head, which was zero, so a dirty LAST word is counted here, on
    /// [`FILLER_OVER_OBJECT`] (the `[GC] tlab-guard: filler_over_object=`
    /// line); under the deep mode `install_tail_filler`'s own scan finds and
    /// counts it, so this counts nothing there.
    fn tail_is_clean_for_sink(&self, tail_start: usize, tail_end: usize) -> bool {
        if self.dbg_deadref_store {
            // SAFETY: `[tail_start, tail_end)` is this buffer's reserved tail
            // (`Tlab::reserved_tail`): mapped, both ends 8-aligned.
            return unsafe { span_reads_zero(tail_start, tail_end) };
        }
        // SAFETY: `tail_end` is the 8-aligned end of a non-empty reserved tail
        // of this buffer, so the word below it is inside the mapped chunk.
        if unsafe { tail_last_word_reads_zero(tail_end) } {
            return true;
        }
        FILLER_OVER_OBJECT.fetch_add(1, Ordering::Relaxed);
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n < 8 || n.is_power_of_two() {
            tracing::error!(
                target: "cratonvm::gc::guard",
                tail = format!("{tail_start:#x}..{tail_end:#x}"),
                start = format!("{:#x}", self.start as usize),
                occurrence = n + 1,
                "[tlab-audit] a retiring TLAB's reserved tail reads zero at its head but NOT \
                 at its last word: something other than this buffer's bump wrote past its \
                 cursor. Declining the tail sink (it would hand the span to the next \
                 allocation); the filler goes over it instead.",
            );
        }
        false
    }

    /// Record one successful VM TLAB allocation and return a bounded batch
    /// when it is published to VM-wide diagnostics. This is plain per-thread
    /// arithmetic; the shared atomic is updated only once per 64 KiB, not once
    /// per object.
    #[inline(always)]
    pub fn note_vm_tlab_allocation(&mut self, bytes: usize) -> Option<TlabAllocationBatch> {
        self.vm_pending_allocated_bytes =
            self.vm_pending_allocated_bytes.saturating_add(bytes as u64);
        self.vm_buffer_noted = self.vm_buffer_noted.saturating_add(bytes as u64);
        (self.vm_pending_allocated_bytes >= VM_ALLOCATION_BATCH_BYTES).then(|| {
            let batch = self.take_vm_allocation_batch();
            self.publish_vm_allocation_batch(batch);
            batch
        })
    }

    /// Drain unflushed VM-level allocation observations. This is public for
    /// direct unit tests; production retirement uses
    /// [`Self::flush_vm_allocation_batch`] so a residual always reaches the
    /// attached per-VM counter.
    pub fn take_vm_allocation_batch(&mut self) -> TlabAllocationBatch {
        TlabAllocationBatch {
            bytes: std::mem::take(&mut self.vm_pending_allocated_bytes),
        }
    }

    /// Publish any residual batch. `retire` calls this itself, which keeps
    /// accounting exact even at TLAB-retirement sites outside the interpreter.
    pub fn flush_vm_allocation_batch(&mut self) {
        let batch = self.take_vm_allocation_batch();
        self.publish_vm_allocation_batch(batch);
    }

    /// Bring the attached per-VM allocation counter up to this buffer's
    /// CONSUMED span, at retire (gen r4w4/alloc4).
    ///
    /// The counter (`HeapRealm::bytes_allocated_total`) was fed only by
    /// [`Self::note_vm_tlab_allocation`], whose one caller is the interpreter's
    /// TLAB allocation — the JIT's inline bump advances `cursor` and tells no
    /// one. So on a JIT-warm run the per-VM "bytes allocated" total, which
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// proposes as the honest cumulative denominator, counted a fraction of the
    /// program's allocation. `cursor - start` sees every bump; publishing what
    /// it exceeds the noted bytes by makes the counter whole at retire
    /// granularity (a live buffer's span is outside it, as it is outside the
    /// process-wide total). One relaxed add per retire, never per object, and
    /// nothing when the interpreter noted every byte (the difference is 0).
    fn top_up_vm_allocation_total(&mut self, consumed: u64) {
        let unreported = consumed.saturating_sub(self.vm_buffer_noted);
        if unreported != 0 {
            self.vm_buffer_noted = consumed;
            self.publish_vm_allocation_batch(TlabAllocationBatch { bytes: unreported });
        }
    }

    #[inline(always)]
    fn publish_vm_allocation_batch(&self, batch: TlabAllocationBatch) {
        if batch.bytes == 0 {
            return;
        }
        if let Some(total) = self.vm_allocation_total {
            // SAFETY: `attach_vm_allocation_counter` accepts an AtomicU64 in
            // the owning SharedVm, which outlives this Java thread and its
            // TLAB. The atomic itself permits concurrent publishers.
            unsafe { total.as_ref() }.fetch_add(batch.bytes, Ordering::Relaxed);
        }
    }

    /// Try to bump-allocate `size` bytes with 8-byte alignment from this TLAB.
    ///
    /// Returns `None` if the TLAB doesn't have enough space.
    ///
    /// **Fast path** (post CRIT-1 fix): load `cursor`, add `size`, compare
    /// to `end`, branch on overflow, store `cursor`, then update three
    /// plain-integer counters on the `pressure` tracker. No atomics, no
    /// mutex, no helper call — the compiler can inline the whole bump
    /// path including the counter bumps. The `Tlab` is per-thread
    /// (`unsafe impl Send`) and only ever borrowed `&mut` from its
    /// owning thread, so the pressure counters need no synchronization.
    #[inline(always)]
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_initialized(size, align, |_| {})
    }

    /// Bump-allocate a region, initialize it, then publish the new cursor.
    ///
    /// The publication order matters when a compiled frame is forcibly
    /// suspended for a non-moving collection: the collector uses `cursor` as
    /// the boundary of the walkable TLAB prefix.  Publishing it before the
    /// caller writes an object header lets the walk treat an all-zero
    /// in-flight object as a `HEADER_SIZE`-byte object and later create a hole
    /// in its body.  The initializer therefore runs while the reservation is
    /// still private; only a fully walker-coherent object is made visible.
    #[inline(always)]
    pub fn alloc_initialized<F>(&mut self, size: usize, align: usize, init: F) -> Option<*mut u8>
    where
        F: FnOnce(*mut u8),
    {
        debug_assert!(align.is_power_of_two());
        // A zero-byte request on a RETIRED buffer (cursor == end == null)
        // passes the bounds test below with `new_cursor == 0` and would hand
        // `init` a null pointer. No production caller asks for 0 bytes (every
        // object has a header); say so where it would matter (gc-common w1-f).
        debug_assert!(size != 0, "Tlab::alloc_initialized: zero-byte request");
        let cursor = self.cursor as usize;
        let aligned = (cursor + align - 1) & !(align - 1);
        // Compact object bodies can be non-aligned. Reserve their complete
        // aligned footprint so the next object and the published reserved
        // tail never begin inside the prior object's collector-visible span.
        let footprint = size.checked_add(align - 1)? & !(align - 1);
        let new_cursor = aligned.checked_add(footprint)?;
        if new_cursor > self.end as usize {
            return None;
        }
        let ptr = aligned as *mut u8;
        // `self.watch_addr`, not `watch_covers(..)`: the latched copy — see
        // the field. Same predicate as `watch_covers`, without the global.
        if self.watch_addr != 0 && self.watch_addr >= aligned && self.watch_addr < new_cursor {
            watch_note(format_args!(
                "TLAB BUMP handed it out: ptr=0x{:x} size={size} tlab=[0x{:x},0x{:x}) cursor was 0x{cursor:x}",
                ptr as usize,
                self.start as usize,
                self.end as usize,
            ));
        }
        // TRIPWIRE (`CRATONVM_DBG_DEADREF_STORE`): is this TLAB bumping over
        // memory that is not free?
        //
        // A chunk is zeroed when it is carved and the cursor only moves
        // forward, so every byte this bump is about to hand out is zero — in a
        // correct run. A non-zero word here is the one shape the two tripwires
        // in `install_tail_filler` and `refill_tlab` cannot see, because both
        // of those look at the moment the span is HANDED OUT and this looks at
        // the moment it is USED: a TLAB that keeps bumping after its chunk has
        // been retired and filled, or re-served to someone else, passes both of
        // them and fails this one. `0xF111E700` as the value names the filler
        // directly.
        if self.dbg_deadref_store {
            // SAFETY: `[ptr, ptr+footprint)` is inside `[cursor, end)`, memory
            // this TLAB owns and that is mapped.
            let w = unsafe { std::ptr::read_unaligned(ptr as *const u32) };
            if w != 0 {
                static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 8 {
                    eprintln!(
                        "[tlab-audit] BUMP-OVER-OCCUPIED #{n}: TLAB alloc at 0x{:x} \
                         (size={size} align={align}) is handing out memory whose first word \
                         is 0x{w:x}, not zero. start=0x{:x} cursor=0x{:x} end=0x{:x}. The \
                         chunk this TLAB is bumping through is not free.",
                        ptr as usize, self.start as usize, cursor, self.end as usize,
                    );
                }
            }
        }
        init(ptr);
        // Commit last: a cross-thread root scan may publish the remaining
        // `[cursor, end)` tail while this thread is suspended in JIT code.
        // Once this store is visible, `ptr` must already hold a valid header.
        //
        // Publication protocol:
        //   initializer stores
        //   -> Release fence
        //   -> cursor commit
        //   -> STW handshake / thread suspension
        //   -> collector Acquire reads of the TLAB boundary/header
        //
        // The TLAB remains single-writer, so the cursor itself can stay a
        // plain pointer used by generated code. The explicit fence defines the
        // data-before-boundary order independently of compiler and CPU store
        // reordering.
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        self.cursor = new_cursor as *mut u8;
        // Inlined `pressure.record_allocation(size)` — direct field
        // updates so the bump path is a pure pointer-bump + bounds-check
        // + a few register-sized adds with no helper call boundary.
        self.pressure.allocations_since_last_refill += size as u64;
        self.pressure.alloc_count += 1;
        if size > LARGE_ALLOC_THRESHOLD {
            self.pressure.large_alloc_count += 1;
        }
        Some(ptr)
    }

    /// Returns the remaining bytes available in this TLAB.
    pub fn remaining(&self) -> usize {
        (self.end as usize).saturating_sub(self.cursor as usize)
    }

    /// Returns true if this TLAB has no backing memory.
    pub fn is_empty(&self) -> bool {
        self.start.is_null()
    }

    /// Retire this TLAB (mark it as exhausted).
    /// The unused tail goes to the sink that carved the buffer when one is
    /// attached and takes it (the Generational heap's young cursor or free
    /// list, gen r4w6/tlab6; ZGC's free list through the process-wide sinks);
    /// otherwise it becomes a filler and is reclaimed at GC time when the
    /// arena is reset. (gen r5w5/sizer9: this line used to say only the
    /// latter.)
    ///
    /// Round-9 gc HIGH-6: wire `install_tail_filler` into the retire path
    /// so a concurrent heap walker that visits the backing arena between
    /// this call and the arena reset can step over the unused tail as a
    /// single synthetic `int[]` instead of stopping at the first byte of
    /// garbage past the cursor. Before this round, `install_tail_filler`
    /// existed but had no production callers (round-7 wave-2 claimed the
    /// fix but never wired it). The filler is harmless when the arena is
    /// reset immediately after retire — it's just a one-time header
    /// write — so always installing it is strictly safer than the
    /// "callers should remember" contract that nobody honoured.
    ///
    /// # Safety considerations
    /// `install_tail_filler` requires the TLAB backing memory to still be
    /// valid (not yet reset) and unique to this thread. Both hold at
    /// every production retire point: the arena owns the buffer and is
    /// not reset until the next GC cycle, and the TLAB is per-thread
    /// (`unsafe impl Send`, never shared). For TLABs that are already
    /// empty (start/cursor/end null), the filler call short-circuits via
    /// the leading null check inside `install_tail_filler`.
    /// # Idempotence (TLAB audit, `tlab-and-card-audit.md`)
    ///
    /// `retire` is idempotent and **must stay so**. Several transition paths can
    /// retire the same TLAB twice with no synchronisation between them — a
    /// thread that parks at a safepoint (`safepoint_check` retires), is then
    /// selected as the next GC initiator (`maybe_gc` retires), and finally
    /// terminates (`thread_start`'s teardown retires) runs three retires with
    /// no refill in between. The second and third are no-ops because
    /// `install_tail_filler` short-circuits on a null cursor and the three
    /// stores are already-null stores.
    pub fn retire(&mut self) {
        self.flush_vm_allocation_batch();
        // Read the span the THREAD consumed before the filler runs.
        // `install_tail_filler` sets `cursor = end` by design (see
        // `install_tail_filler_always_consumes_the_tlab`), so reading
        // `consumed_bytes()` after it charges the thread for the whole TLAB
        // chunk including the unused tail. That made
        // `ThreadMXBean.getThreadAllocatedBytes` report the collector's TLAB
        // sizing rather than the program's allocation: the same Hibernate HQL
        // parse reported 488 MB under ZGC (large chunks) and 49 MB under
        // Generational (small ones). A counter whose answer depends on which
        // collector is running cannot be measuring the Java work.
        let consumed = self.consumed_bytes() as u64;
        // zr z8/st (`CRATONVM_ZGC_LAZY_TLAB_STARTS`): the consumed span goes to
        // the ZGC heap that carved it BEFORE the tail is reclaimed or filled,
        // while the buffer still names it. One relaxed load when no heap asked.
        {
            let (s, c, e) = (self.start as usize, self.cursor as usize, self.end as usize);
            if s != 0 && s <= c && c <= e {
                note_retired_span_via_sinks(s, c, e);
            }
        }
        // A registered sink (ZGC) takes the unused tail back into its free
        // list; otherwise the tail becomes a filler object the linear sweeps
        // can parse across. Either way the TLAB owns nothing afterwards.
        let reclaimed = match self.reserved_tail() {
            Some((tail_start, tail_end)) => {
                // gen r4w6/tlab6: the heap that carved this buffer first (armed
                // only by `attach_tail_sink`), then the process-wide sinks.
                //
                // gen r5w2/alloc6: the attached sink is offered the tail only
                // when its first word reads zero. A tail is zero by
                // construction (carved zero, never written past the cursor),
                // and the filler path below has always checked exactly that
                // word (`install_tail_filler`'s O(1) tripwire) because a
                // non-zero one means the CURSOR is wrong — sitting on an object
                // already handed out. The sink skipped that check: it would
                // hand the span, live object included, back to the young
                // allocator (cursor retraction or free list), and the next
                // carve would zero it under its owner. Declining instead sends
                // the retire down the filler path, whose tripwire counts and
                // reports it (`[GC] tlab-guard: filler_over_object=`). The
                // process-wide sinks (ZGC) are unchanged.
                //
                // SAFETY: `reserved_tail` returned `[tail_start, tail_end)`
                // inside this buffer's own mapped chunk, both ends 8-aligned
                // and `tail_start < tail_end`, so eight bytes are readable at
                // `tail_start`; nobody else writes the tail.
                //
                // gen r5w5/sizer9: and its LAST word too (the whole tail under
                // `CRATONVM_DBG_DEADREF_STORE`) — see `tail_is_clean_for_sink`.
                // Both extra reads happen only on an armed buffer whose head
                // passed, so a buffer with no attached sink retires exactly as
                // before.
                let attached_ok = self.tail_sink.is_some()
                    && unsafe { tail_head_reads_zero(tail_start) }
                    && self.tail_is_clean_for_sink(tail_start, tail_end);
                let taken = (attached_ok
                    && self.reclaim_tail_via_attached_sink(tail_start, tail_end))
                    || reclaim_tail_via_sinks(tail_start, tail_end);
                if taken {
                    // Recovered, not wasted -- counted apart from the two
                    // filler buckets for exactly that reason.
                    TLAB_SINK_TAIL_BYTES
                        .fetch_add((tail_end - tail_start) as u64, Ordering::Relaxed);
                }
                taken
            }
            None => false,
        };
        if reclaimed {
            self.cursor = self.end;
        } else {
            // gci i28/tl: the buried tail is this thread's waste (HotSpot's
            // `_gc_waste`; a refill's retire moves it to the refill bucket,
            // `retire_for_refill`). Before the filler moves the cursor.
            if self.accounting == TlabAccounting::JavaThread {
                if let Some((tail_start, tail_end)) = self.reserved_tail() {
                    let words = ((tail_end - tail_start) / 8) as u32;
                    self.gc_waste_words = self.gc_waste_words.wrapping_add(words);
                }
            }
            // SAFETY: see method-level note — backing memory valid, single owner.
            unsafe {
                self.install_tail_filler(TLAB_FILLER_CLASS_ID);
            }
        }
        // Roll the consumed span into the thread's running total BEFORE the
        // pointers are nulled: `consumed_bytes()` is `cursor - start` and
        // reads 0 the instant either is null. Idempotent for the same reason:
        // a second `retire()` adds 0 (the pre-filler read above is 0 too).
        //
        // The post-condition `finish_retire` asserts is not a formality: the
        // whole cross-thread STW protocol reads "a parked or blocked peer has
        // already retired its TLAB, therefore `reserved_tail()` is `None`,
        // therefore it contributes no skip region"
        // (`ThreadRegistry::tlab_addr`'s safety note). If a future edit made
        // `retire` leave any of the three pointers live, the collector would
        // publish a skip region for a TLAB whose backing arena is about to be
        // reset.
        self.finish_retire(consumed);
    }

    /// [`Self::retire_taking_tail`], with the walkable tail filler installed
    /// over `[cursor, end)` first — for a caller that needs BOTH (ZGC's
    /// `tlab_retire_locked` returns the tail to its own arena free list and
    /// still wants the span parsable while it sits there).
    ///
    /// It exists so that caller does not have to write
    /// `install_tail_filler(); retire_taking_tail();` by hand, which is the
    /// exact ordering [`Self::retire`]'s own body warns about:
    /// `install_tail_filler` sets `cursor = end` by design, so a
    /// `consumed_bytes()` read after it charges the whole chunk including the
    /// unused tail. That defect was fixed inside `retire` and then
    /// reintroduced at the ZGC call site by open-coding the two steps.
    ///
    /// The returned tail is the PRE-filler `[cursor, end)`, which is the span
    /// the caller must hand back to its allocator.
    ///
    /// # Safety
    /// Same as [`Self::install_tail_filler`]: the backing memory must still be
    /// valid and uniquely owned by the caller.
    pub unsafe fn retire_with_filler_taking_tail(
        &mut self,
        class_id: cratonvm_types::ClassId,
    ) -> Option<(usize, usize)> {
        // BEFORE the filler — see the method doc and `Tlab::retire`.
        let consumed = self.consumed_bytes() as u64;
        let tail = self.reserved_tail();
        self.install_tail_filler(class_id);
        self.finish_retire(consumed);
        tail
    }

    /// Retire, handing the reserved tail to the CALLER instead of to a sink or
    /// a filler. For an allocator that owns both the buffer and the arena it
    /// was carved from (`zgc::arena_tlab`), which returns the tail to its own
    /// free list and must not have a process-wide sink do it a second time.
    ///
    /// Same accounting as [`Self::retire`]: the consumed span is credited to
    /// the thread and process totals, the three pointers are nulled, and the
    /// "sized before retired" tripwire is armed.
    pub fn retire_taking_tail(&mut self) -> Option<(usize, usize)> {
        let consumed = self.consumed_bytes() as u64;
        let tail = self.reserved_tail();
        self.finish_retire(consumed);
        tail
    }

    /// The shared tail of every retire: credit `consumed` (read by the caller
    /// BEFORE any tail filler ran), null the three pointers, arm the
    /// "sized before retired" tripwire.
    fn finish_retire(&mut self, consumed: u64) {
        // gci i1/pp: publish the pending sub-batch on EVERY retire, not only
        // `retire`'s. `top_up_vm_allocation_total` below publishes
        // `consumed - vm_buffer_noted`, and `vm_buffer_noted` already counts
        // the pending bytes, so a retire that skipped this flush
        // (`retire_taking_tail`, `retire_with_filler_taking_tail`) dropped
        // them from the per-VM total for good when the struct was replaced.
        // A no-op after `retire`'s own flush, and on a buffer with no counter.
        self.flush_vm_allocation_batch();
        // Waste census (gengc-round1-alloc, 2026-09-20). Gated on a live
        // `start` so an idempotent second retire -- which `retire`'s own doc
        // says is a supported and routine transition -- adds nothing and
        // cannot inflate the retire count. `end` is the OWNED end of the
        // buffer, so this is the span the arena actually gave up.
        //
        // MERGE NOTE 2026-09-21: this read `chunk_end` when it was written,
        // because a ZGC lazily-zeroed buffer then had a SOFT `end` that was
        // not the chunk. `dev`'s `00274a14c` removed `CRATONVM_ZGC_LAZY_TLAB_ZERO`
        // after measuring it neutral, and with it the soft end, so `end` is
        // now unambiguously the owned end and the distinction is gone. Both
        // fields are still live here; the three stores below are what null
        // them.
        //
        // `JavaThread` buffers only, for [`TlabAccounting`]'s reason: ZGC's
        // heap-internal staging layer carves the SAME bytes a Java thread's
        // TLAB then carves out of it, so counting both would make carved
        // bytes exceed the arena and the carved-vs-consumed ratio meaningless
        // -- the exact double-count that made `getTotalThreadAllocatedBytes`
        // report 3.00x of retained heap.
        if !self.start.is_null() && self.accounting == TlabAccounting::JavaThread {
            let carved = (self.end as usize).saturating_sub(self.start as usize) as u64;
            TLAB_RETIRES.fetch_add(1, Ordering::Relaxed);
            TLAB_CARVED_BYTES.fetch_add(carved, Ordering::Relaxed);
            TLAB_CONSUMED_BYTES.fetch_add(consumed.min(carved), Ordering::Relaxed);
            // Shape, not just total -- see `TLAB_WASTEFUL_RETIRES`. One
            // compare per retire, i.e. one per ~256 KiB allocated. The drain
            // discriminator (gce e1/o) is `retire_counts_as_wasteful`'s.
            if retire_counts_as_wasteful(carved, consumed) {
                TLAB_WASTEFUL_RETIRES.fetch_add(1, Ordering::Relaxed);
            }
            self.top_up_vm_allocation_total(consumed);
        }
        // Bank what the sizer needs BEFORE the pointers go (gen r4/alloc): a
        // live `start` means this is the window's first retire. See
        // `Tlab::refill_request_size`.
        if !self.start.is_null() {
            self.pressure.bank_retire(consumed as usize);
        }
        self.thread_alloc_carry = self.thread_alloc_carry.saturating_add(consumed);
        self.publish_settled(0);
        self.start = std::ptr::null_mut();
        self.cursor = std::ptr::null_mut();
        self.end = std::ptr::null_mut();
        // Disarm the inline ZGC start-bit store: the chunk is no longer this
        // buffer's (a null `end` already fails the bump, this makes the
        // ownership claim itself say so).
        self.zgc_owned_epoch = 0;
        self.zgc_announce = 0;
        self.zgc_owned_words = [0, 0];
        self.pressure.retired_since_refill = true;
        debug_assert!(self.is_retired() && self.reserved_tail().is_none());
    }

    /// Round-5 #9 / round-7 #9 — Install a synthetic `int[]` filler object
    /// at the TLAB cursor before retiring, so a subsequent heap iterator
    /// can skip the entire unused tail in O(1).
    ///
    /// Before this fix, when a thread reached a safepoint with a partially
    /// used TLAB and the GC walked the backing arena, the iterator stopped
    /// at the first byte past `cursor` (where bytes are zero / garbage),
    /// losing up to `MAX_TLAB_SIZE` (1 MiB) bytes per thread per cycle to
    /// invisible tail waste.
    ///
    /// # How the filler is laid out
    ///
    /// The filler is an `Array<i32>` header whose `array_length` is chosen
    /// so that `HEADER_SIZE + array_data_size(len, Int) == tail_bytes`.
    /// Element size is 4 bytes; the data area is rounded up to 8-byte
    /// alignment by `array_data_size`, which matches the TLAB's natural
    /// 8-byte alignment for cursor/end.
    ///
    /// If the cursor is not 8-aligned (rare — most allocations use 8-byte
    /// align), it is bumped up to the next 8-byte boundary; the skipped
    /// padding bytes are zeroed.
    ///
    /// If fewer than `HEADER_SIZE` bytes remain after alignment, the tail
    /// is simply zeroed — there is no room for a header. A zero header
    /// terminates the heap iterator naturally (class_id = 0 / num_slots = 0
    /// → size = HEADER_SIZE; iterator either stops or skips a tiny
    /// well-formed empty record).
    ///
    /// # Safety
    /// The caller must guarantee that the TLAB backing memory is still
    /// valid (the arena has not been reset yet) and that no other thread
    /// will read or write the tail concurrently — both conditions hold at
    /// a safepoint when the owning thread is parked.
    ///
    /// `class_id` must be a class id that the heap walker will treat as a
    /// well-formed `int[]`. Use [`tlab_filler_class_id`] to pick one.
    pub unsafe fn install_tail_filler(&mut self, class_id: cratonvm_types::ClassId) {
        if self.cursor.is_null() || self.end.is_null() {
            return;
        }

        // Align cursor up to 8 so the synthetic header lives at an
        // 8-byte-aligned address (ObjectHeader requires it).
        let cursor_addr = self.cursor as usize;
        let aligned = (cursor_addr + 7) & !7;
        let end_addr = self.end as usize;
        if aligned >= end_addr {
            // Fewer than 8 bytes of unaligned slack remain — there is no room
            // for even the GAP-filler sentinel. TLAB AUDIT: consume the slack
            // anyway so this function is TOTAL, i.e. every exit path leaves
            // `cursor == end`. Callers (and `retire`'s post-condition) treat
            // "the filler was installed" as "the TLAB publishes no reserved
            // tail"; leaving `cursor < end` here made that false for the one
            // case where the slack is unaligned, so `reserved_tail()` would
            // still hand the collector a sub-8-byte span. Reachable only if
            // something allocated with `align < 8` (no production path does —
            // see `reserved_tail`'s debug assertion), and the bytes being
            // consumed are inter-object padding after the last allocation, so
            // nothing live is covered.
            if tail_census_counts(self.accounting) {
                TLAB_GAP_TAIL_BYTES.fetch_add(
                    end_addr.saturating_sub(cursor_addr) as u64,
                    Ordering::Relaxed,
                );
            }
            self.cursor = self.end;
            return;
        }

        // Zero any pre-alignment padding so the iterator sees clean bytes
        // up to the synthetic header.
        if aligned > cursor_addr {
            let pad = aligned - cursor_addr;
            // SAFETY: cursor..aligned lies within [cursor, end), still owned by this TLAB.
            unsafe {
                std::ptr::write_bytes(self.cursor, 0, pad);
            }
        }

        let tail = end_addr - aligned;
        use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        // TRIPWIRE: is there already an object AT the cursor?
        //
        // A TLAB's memory is zeroed when the chunk is carved (`refill_tlab`),
        // and the cursor only ever moves forward over what this thread
        // allocated. So the bytes at `cursor` are zero unless the cursor is
        // WRONG — pointing at or before an object that was already handed out.
        // Filling from there buries live objects under one synthetic `int[]`:
        // the next collection's object-start walk strides over them, their
        // addresses stop being object starts, and every reference to them is
        // refused by the evacuator and left dangling.
        //
        // That is exactly how `0x200868400d8` — a live lambda capture 261 336
        // bytes inside a 261 792-byte filler — reached `invokevirtual` as an
        // all-zero header on BindableTests (2026-09-09). O(1), always on: one
        // load of a word this function is about to overwrite anyway, and the
        // silence of a correct run costs a predicted-not-taken branch.
        //
        // SAFETY: `aligned` is inside `[cursor, end)`, which this TLAB owns and
        // which is mapped until the arena is reset.
        if watch_covers(aligned, end_addr) {
            watch_note(format_args!(
                "TAIL FILLER buried it: filler=[0x{aligned:x},0x{end_addr:x}) tlab=[0x{:x},0x{end_addr:x}) consumed={}",
                self.start as usize,
                aligned.saturating_sub(self.start as usize),
            ));
        }
        let mut occupant = unsafe { std::ptr::read_unaligned(aligned as *const u32) };
        // Same widening as the refill tripwire, and for the same reason: the
        // O(1) word above catches a cursor that sits exactly ON an object, but
        // not a filler span that swallows objects further along. The tail of a
        // TLAB is untouched memory, so any non-zero word inside it is live data
        // this filler is about to hide from every object-start walk.
        if occupant == 0 && self.dbg_deadref_store {
            // SAFETY: `[aligned, end_addr)` is this TLAB's own tail, mapped and
            // 8-aligned at both ends.
            let words = (end_addr - aligned) / 8;
            for i in 0..words {
                let w = unsafe { std::ptr::read_unaligned((aligned as *const u64).add(i)) };
                if w != 0 {
                    occupant = w as u32;
                    break;
                }
            }
        }
        if occupant != 0 {
            FILLER_OVER_OBJECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 || n.is_power_of_two() {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    cursor = format!("{aligned:#x}"),
                    start = format!("{:#x}", self.start as usize),
                    end = format!("{end_addr:#x}"),
                    consumed = aligned.saturating_sub(self.start as usize),
                    tail,
                    occupant_class_id = occupant,
                    occurrence = n + 1,
                    "[tlab-audit] install_tail_filler is about to stamp a filler over an \
                     ALREADY-ALLOCATED object: the word at the TLAB cursor is a live class \
                     id, not the zero this chunk was carved with. Everything from here to \
                     the TLAB end is about to become one synthetic int[], so the next \
                     object-start walk will stride over it and the evacuator will refuse \
                     every reference into it.",
                );
            }
        }
        if tail < HEADER_SIZE {
            // Bug-D fix (2026-06-12): a sub-`HEADER_SIZE` tail cannot hold a
            // walkable `int[]` filler, and ZEROING it (the old behaviour) is
            // unsafe — a zeroed sub-`HEADER_SIZE` region is byte-identical to
            // a live `new Object()` (class_id 0 / num_slots 0), so the
            // non-moving young sweep's linear walk strides a phantom
            // `HEADER_SIZE`-byte object off the object grid (the
            // `RemoteCIDRFilter` "implausible object size" desync that a later
            // moving GC turns into a SIGSEGV).
            //
            // Stamp the GAP-filler sentinel instead: class_id at offset 0 and
            // the exact gap length at offset 4 — both inside the smallest
            // (8-byte) gap. The sweep walker reclaims the span in O(1) on the
            // `class_id == GAP_FILLER_CLASS_ID` match.
            //
            // HOW WIDE CAN THIS TAIL BE? `aligned` and `end_addr` are both
            // 8-aligned, so `tail` is a non-zero multiple of 8, and this arm
            // additionally has `tail < HEADER_SIZE`. `HEADER_SIZE` is **16**
            // (`cratonvm_types::HEADER_SIZE`; it was 32, then 24, then 16 --
            // the 2026-08-06 shrink), so the only value that satisfies both is
            // `tail == 8` and the sentinel fills the span exactly.
            // gengc-round1-alloc read this arm as covering 8..32 and wrote a
            // test around a 24-byte tail, which does not reach here at all --
            // 24 >= HEADER_SIZE takes the `int[]` arm below. Nothing in this
            // file may assume a particular `HEADER_SIZE`, which is why the
            // zeroing below stays; but a reader sizing an argument on this arm
            // should check the constant first.
            debug_assert!(
                tail >= 8 && tail % 8 == 0,
                "sub-header tail must be an 8-aligned, >=8-byte span (tail={tail})",
            );
            // SAFETY: aligned..aligned+8 lies within [aligned, end) (tail>=8)
            // and is 8-aligned for the two u32 writes.
            unsafe {
                std::ptr::write(aligned as *mut u32, GAP_FILLER_CLASS_ID.as_u32());
                std::ptr::write((aligned + 4) as *mut u32, tail as u32);
            }
            // Zero the rest of the gap (gengc-round1-alloc, 2026-09-20). The
            // sentinel occupies the first 8 bytes and the walkers that know it
            // stride the whole span off the length at offset 4 -- but the
            // `int[]` arm below zeroes its entire data area, and leaving a
            // remainder holding whatever the span held before would be the one
            // asymmetry between the two arms. It would matter where the reader
            // is NOT a walker that knows the sentinel: a conservative
            // candidate landing inside the gap is resolved by
            // `is_object_address`, which DEDUCES an answer from the bytes at
            // the address, and stale bytes are what that deduction's own
            // comment names as its false-positive source. (When this was
            // written, a lazily zeroed buffer could make the remainder
            // genuinely un-zeroed rather than merely stale; `dev`'s
            // `00274a14c` removed that mode, so only the stale case remains.)
            //
            // AT TODAY'S `HEADER_SIZE` (16) THIS BRANCH CANNOT BE TAKEN: see
            // the arm's own note -- `tail` is exactly 8 here, so there is no
            // remainder and nothing can go stale. It is kept because it is the
            // cheap half of a total function and because `HEADER_SIZE` has
            // moved twice already (32 -> 24 -> 16); the day it moves up, this
            // arm starts seeing 8/16/24-byte tails and the asymmetry is real
            // again. Deliberately NOT presented as a fix in force -- an
            // unreachable branch described as protection is how the wrong
            // thing gets believed about a file.
            if tail > 8 {
                // SAFETY: `[aligned + 8, aligned + tail)` is the rest of this
                // TLAB's own tail -- mapped, owned, and about to be dead.
                unsafe { std::ptr::write_bytes((aligned + 8) as *mut u8, 0, tail - 8) };
            }
            if tail_census_counts(self.accounting) {
                TLAB_GAP_TAIL_BYTES.fetch_add(tail as u64, Ordering::Relaxed);
            }
            self.cursor = self.end;
            return;
        }

        // tail >= HEADER_SIZE; compute array length so the filler exactly
        // consumes `tail` bytes. (tail - HEADER_SIZE) is a multiple of 8
        // because both `aligned` and `end_addr` are 8-aligned.
        let data_bytes = tail - HEADER_SIZE;
        // data_bytes is a multiple of 8; (n*4 + 7) & !7 == n*4 iff n is even.
        // data_bytes/4 is even because data_bytes is a multiple of 8 → length fits exactly.
        // If a future change to HEADER_SIZE breaks the 8-multiple invariant
        // we want to fail loudly here, not silently corrupt memory by writing
        // past the synthetic array's payload.
        debug_assert_eq!(
            data_bytes % 8,
            0,
            "tail filler assumes 8-aligned data_bytes (tail={tail}, HEADER_SIZE={HEADER_SIZE})"
        );
        let length = (data_bytes / 4) as u32;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            ArrayElementType::Int,
            length,
            0,
        );
        // SAFETY: aligned..aligned+HEADER_SIZE lies within [cursor, end)
        // and is properly aligned for ObjectHeader.
        unsafe {
            std::ptr::write(aligned as *mut ObjectHeader, header);
        }
        // gen r5w5/sizer9: the skip also needs the tail's LAST word to read
        // zero — the O(1) half the tail sink's gate reads too
        // (`tail_is_clean_for_sink`), which is how a dirty tail that sink
        // refused reaches this line with a clean head. Read only when the skip
        // would otherwise apply, so the default (opt-in off) is unchanged.
        //
        // SAFETY (the read): `end_addr` is 8-aligned and `tail >= HEADER_SIZE`
        // on this arm, so the word below it is inside this buffer's tail.
        if !filler_data_memset_is_redundant(self.tuning.filler_skip_zero, self.accounting, occupant)
            || !unsafe { tail_last_word_reads_zero(end_addr) }
        {
            // SAFETY: `[aligned + HEADER_SIZE, end_addr)` is the rest of this
            // TLAB's own tail (`tail >= HEADER_SIZE` on this arm), mapped and
            // owned by this buffer until the arena is reset.
            unsafe {
                // Zero the data area so any stale bytes don't trip header
                // sanity checks on the next walk.
                std::ptr::write_bytes((aligned + HEADER_SIZE) as *mut u8, 0, data_bytes);
            }
        }
        // Family-A forensics (CRATONVM_DBG_A2, default-inert): record the
        // tail-filler header write so the sweep's desync forensics can tell
        // "corrupt header over a recycled filler slot" apart from "corrupt
        // header over a real object". Filler lengths (512, 751, 254, ...)
        // exactly match the alen values seen in the corrupt-header family.
        crate::a2dbg::record(
            aligned,
            class_id.as_u32(),
            ObjectKind::Array as u8,
            ArrayElementType::Int as u8,
            length,
            0,
            tail,
        );
        if tail_census_counts(self.accounting) {
            TLAB_FILLER_TAIL_BYTES.fetch_add(tail as u64, Ordering::Relaxed);
        }
        // Bump cursor past the filler so subsequent `remaining()` calls
        // report zero. The TLAB is now fully consumed (by the filler).
        self.cursor = self.end;
    }

    /// Bytes handed out of this TLAB since it was installed, **including the
    /// allocations the JIT's inline bump made without ever entering this
    /// file**.
    ///
    /// `cursor - start` is the only measure that sees those: the compiled
    /// fast path in `jit/src/x64.rs::emit_inline_tlab_new` reads `cursor` and
    /// `end` at their fixed offsets, bumps the cursor and commits it — it
    /// never touches [`TlabPressureTracker`]. Returns 0 for an empty
    /// (retired / never refilled) TLAB.
    pub fn consumed_bytes(&self) -> usize {
        if self.start.is_null() || self.cursor.is_null() {
            return 0;
        }
        (self.cursor as usize).saturating_sub(self.start as usize)
    }

    /// T5.5.1 — Recommended byte size for the next refill. Delegates to
    /// the per-thread [`TlabPressureTracker`] which applies the
    /// grow/shrink heuristic. Call this **before** retiring the current TLAB
    /// (the sizer reads the live cursor) and before requesting a fresh buffer
    /// from the arena.
    ///
    /// JIT BLINDNESS (perf/gc-allocation-fastpath, 2026-07-26). The tracker's
    /// `alloc_count` only counts allocations that went through
    /// [`Tlab::alloc_initialized`]. A thread running compiled code allocates
    /// through the JIT's inline bump instead, so its `alloc_count` is ~0 for
    /// the whole TLAB lifetime — and the raw heuristic reads "fewer than
    /// [`SLOW_REFILL_ALLOC_COUNT`] allocations" as *idle*. The result was a
    /// one-way ratchet: any JIT thread whose TLAB took longer than
    /// [`FAST_REFILL_THRESHOLD_MS`] to fill halved its next request, again
    /// and again, down to [`MIN_TLAB_SIZE`] — and could never climb back,
    /// because the shrink condition stayed permanently true no matter how
    /// furiously the thread was allocating. An 8 KiB TLAB refills 32x more
    /// often than the 256 KiB baseline, and every refill is a young-arena
    /// lock, a `write_bytes` of the whole chunk, a tail-filler write and two
    /// `Instant::now()` calls. Passing the observed consumption in lets the
    /// tracker distinguish "nobody allocated" from "the JIT allocated and did
    /// not tell you".
    pub fn next_refill_size(&mut self) -> usize {
        // TLAB AUDIT (tlab-and-card-audit.md): "size first, THEN retire"
        // is a prose contract with a silent failure mode. `consumed_bytes()` is
        // `cursor - start`, and `retire()` nulls both — so a caller that
        // retires first gets `consumed == 0`, which is exactly the input that
        // reintroduces the one-way shrink ratchet this signal exists to close
        // (see this method's doc comment). Nothing distinguishes that from a
        // genuinely idle thread, so the bug is invisible: the TLAB just gets
        // smaller every refill, forever.
        //
        // `retired_since_refill` is set by `retire` and cleared by
        // `begin_refill`, so the assertion fires precisely on the misordering
        // and never on the legitimate "fresh thread, never refilled" call.
        debug_assert!(
            !self.pressure.retired_since_refill,
            "Tlab::next_refill_size called on a RETIRED TLAB — the adaptive sizer \
             reads the live `cursor - start` span, which retire() has already \
             zeroed, so a JIT-drained TLAB reads as idle and ratchets down to \
             MIN_TLAB_SIZE forever. Size the outgoing TLAB BEFORE retiring it.",
        );
        let consumed = self.consumed_bytes();
        self.pressure.next_refill_size_with_consumed(consumed)
    }

    /// The size the NEXT refill of this thread's buffer should request, in
    /// whatever state the buffer is — the one call a refill site needs.
    ///
    /// * never handed a chunk ([`Tlab::empty`]): [`initial_refill_size`], the
    ///   "start big" baseline;
    /// * live (still holding its chunk — a drain): exactly
    ///   [`Self::next_refill_size`];
    /// * retired (a park, a blocking native, a forced GC, thread handoff):
    ///   the same heuristic, fed the consumption and fill time the retire
    ///   BANKED, so the answer is the one a size-before-retire would have got
    ///   — except that a buffer retired below the heuristic's 75 % "well
    ///   used" line is never GROWN (see the body: a short window on a retire
    ///   is a quick park, not a quick fill).
    ///
    /// gen r4/alloc (2026-09-23), the `tlab.rs` half of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    /// The refill site used `is_empty()` to mean "no history", which is also
    /// true of every retired buffer, so the adaptive sizer only ever saw
    /// drains. gce e2/o: the opt-in arm that called this
    /// (`CRATONVM_TLAB_SIZE_RETIRED`) was removed -- superseded by the share
    /// sizer, which samples an early retire's consumption like a drain's (and,
    /// since gcg g1/j, caps the refill after an under-used one and does not
    /// count it toward its raise: [`Self::share_refill_request`]) -- so this
    /// method has no production caller; it is kept with its tests as the
    /// ladder's reference answer. Delete it with the ladder.
    ///
    /// Why this does not reopen the ratchet [`Self::next_refill_size`]'s
    /// debug assertion guards: that ratchet is "sized after retire, so
    /// `consumed == 0`, so every JIT-drained buffer reads as idle". Here the
    /// consumption is banked by the retire itself (`cursor - start`, JIT
    /// bumps included), so a retired buffer is sized on what it really handed
    /// out. That assertion stays on `next_refill_size`, whose contract is
    /// still "live buffers only".
    ///
    /// Always in `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]` except for the first arm,
    /// which is [`INITIAL_REFILL_SIZE`] (inside the same range).
    pub fn refill_request_size(&self) -> usize {
        if !self.pressure.has_history {
            return initial_refill_size();
        }
        let p = &self.pressure;
        if p.retired_since_refill {
            let next = p.next_refill_size_at(p.banked_consumed, p.banked_elapsed_ms);
            // A retired window's short life is not a fast FILL. The heuristic's
            // `elapsed < FAST_REFILL_THRESHOLD_MS` grow arm was written for
            // drains, where a short window means the buffer filled quickly; on
            // a retire it only means the thread parked quickly, and a thread
            // that parks every half-millisecond having used a third of its
            // buffer would otherwise be DOUBLED on every refill — growing the
            // very tail waste this path exists to stop re-arming. So a buffer
            // that was not well used (the heuristic's own 75 % line) may keep
            // or shrink its size, never grow it. Drains are unaffected.
            let current = p.last_refill_size.max(MIN_TLAB_SIZE);
            let well_used = p.banked_consumed.saturating_mul(4) >= current.saturating_mul(3);
            // gce e2/o: the opt-in waste clause (`CRATONVM_TLAB_WASTE_SHRINK`,
            // halve after a retire that gave up half its buffer) was removed
            // with `CRATONVM_TLAB_SIZE_RETIRED`, its only way in.
            if well_used {
                next
            } else {
                next.min(current)
            }
        } else {
            p.next_refill_size_with_consumed(self.consumed_bytes())
        }
    }

    /// The HotSpot-shaped sizer's answer for the NEXT refill of this thread's
    /// buffer (`CRATONVM_TLAB_SHARE_SIZER`, gen r4w4/alloc4) — see
    /// [`TlabShareSizer`] for the model. Call it on the OUTGOING buffer, in
    /// any state (never refilled, live, retired), before it is replaced;
    /// `epoch` is the heap's collection count (`VmHeap::collection_count`).
    /// The refill site then carries the state across with
    /// [`Self::share_sizer`] / [`Self::adopt_share_sizer`].
    ///
    /// Unlike the ladder ([`Self::refill_request_size`]) it reads no clock and
    /// no allocation COUNT, so it has neither the JIT-blindness ratchet nor the
    /// quick-park-read-as-quick-fill hazard: its only input is
    /// [`Self::thread_allocated_bytes`], the cursor-based total, sampled once
    /// per collection. An early retire is SAMPLED exactly like a drain — the
    /// bytes it consumed count, the tail it gave up does not — so parking
    /// threads converge on their real allocation rate. The refill that follows
    /// an UNDER-USED early retire is not sized like a drain's (gcg g1/j): it is
    /// capped at twice what the retired buffer used and is not counted toward
    /// the underpredicted raise (`early_retire_refill_cap`,
    /// [`Self::note_share_refill`]), so the tail waste a thread that parks or
    /// calls natives often can produce is bounded by its use, not by `desired`.
    ///
    /// Always in `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]`, 8-aligned (the first refill
    /// excepted only in that it is [`initial_refill_size`]).
    ///
    /// Asking changes only the WINDOW (it may close one and resize); a refill
    /// is counted by [`Self::note_share_refill`] once it has been granted, so
    /// a refill gate that refuses a thousand times in a row (the fragmentation
    /// wedge) cannot read as a thousand refills and inflate the request —
    /// which would only make the gate refuse harder.
    pub fn share_refill_request(&mut self, epoch: u64) -> usize {
        self.share_refill_request_sized(epoch, &TlabSizing::DEFAULT)
    }

    /// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`):
    /// [`Self::share_refill_request`] under the VM's TLAB flags
    /// ([`TlabSizing`], `HeapRealm::tlab_sizing`): the first window opens at
    /// [`TlabSizing::initial_size`] (a typed `TLABSize`); a closed window
    /// resizes to `clamp(avg / target_refills, min_size, max)` with the VM's
    /// `TLABWasteTargetPercent` and `MinTLABSize`, sampled at its
    /// `TLABAllocationWeight` -- or, under `-XX:-ResizeTLAB`, is sampled and
    /// keeps its size (HotSpot still samples the allocation fraction, and
    /// skips `resize`); the refill-waste limit is `desired /
    /// TLABRefillWasteFraction`. The flags are copied onto the carried sizer
    /// state so [`Self::note_share_refill`] and [`Self::keep_on_miss`] read
    /// the same values. `TlabSizing::DEFAULT` is the old answer byte for byte.
    pub fn share_refill_request_sized(&mut self, epoch: u64, sizing: &TlabSizing) -> usize {
        let now = self.thread_allocated_bytes();
        // gcg g1/j: read before `self.share` is borrowed (see
        // `early_retire_refill_cap`).
        // gcj j3/yg3 R4: on the VM's typed `MinTLABSize` floor (the 8 KiB
        // constant untyped, as before).
        let early_cap = self.early_retire_refill_cap(sizing.min_size);
        let s = &mut self.share;
        s.adopt_flags(sizing);
        let first_window = s.window_epoch == u64::MAX || s.desired == 0;
        if first_window {
            // The thread's first sized refill: open the first window at the
            // "start big" baseline, as the ladder does (gci i45/sc: a typed
            // `-XX:TLABSize`, HotSpot's `initial_desired_size`).
            s.window_epoch = epoch;
            s.window_start_bytes = now;
            s.refills_in_window = 0;
            s.set_desired(sizing.initial_size.min(sizing.max_size));
        } else if epoch != s.window_epoch {
            // A collection (or several) closed the window: sample, resize.
            let cycles = epoch.wrapping_sub(s.window_epoch);
            // A collection count that went BACKWARDS (a different heap, in a
            // test) is one fresh interval, not four billion.
            let cycles = if cycles == 0 || cycles > (1 << 32) {
                1
            } else {
                cycles
            };
            let bytes = now.saturating_sub(s.window_start_bytes);
            s.sample(bytes as f64 / cycles as f64);
            s.window_epoch = epoch;
            s.window_start_bytes = now;
            s.refills_in_window = 0;
            if sizing.resize_tlab {
                s.set_desired(TlabShareSizer::desired_from_average_with(
                    s.avg_bytes_per_cycle,
                    sizing.target_refills(),
                    sizing.min_size,
                ).min(sizing.max_size));
            } else {
                // gci i45/sc: `-XX:-ResizeTLAB` -- the size stays; only the
                // limit is re-armed, as at every window boundary.
                let keep = s.desired;
                s.set_desired(keep.min(sizing.max_size));
            }
            TLAB_SIZER_SAMPLES.fetch_add(1, Ordering::Relaxed);
        }
        // gcj j2/yg2 (`CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`): HotSpot's `fill()`
        // re-arms the limit at EVERY refill (`initial_refill_waste_limit()`),
        // not only when the window closes or the size changes.
        if sizing.hotspot_waste_limit {
            s.refill_waste_limit = s.desired / (s.refill_waste_fraction.max(1) as usize);
        }
        if let (false, Some(cap)) = (first_window, early_cap) {
            // gcg g1/j (`gcf-f5j-jni-package-retires-the-tlab-on-every-native-call`):
            // the outgoing buffer was given up early and under-used. Two
            // changes, both for this one refill:
            //  * it is not counted toward the underpredicted raise
            //    (`note_share_refill` reads the mark) -- a thread that calls a
            //    native or blocks 100 times per young cycle used to climb to
            //    1 MiB carves (gcf f7: `sizer_max_desired=1048576` on Gen
            //    `PS_nochurn`, 209.6 GB carved for 6.95 MB consumed);
            //  * the carve is capped at twice what the outgoing buffer used
            //    (never below `MIN_TLAB_SIZE`, never above `desired`), so a
            //    thread that allocates 32 bytes between two native calls
            //    re-carves (and zeroes) 8 KiB, not `desired`. A cap the thread
            //    outgrows ends in a DRAIN, and a drain's refill is `desired`
            //    again: at most one extra refill per early retire.
            // g1b (vg1 j1_*_repeat_PS_full): a refill that also CLOSES a
            // window (the arm above) is capped too. g1 exempted it ("a pause
            // retires every buffer"), and on G1 `full` -- a young pause every
            // ten rounds -- those ~20 000 window-closing refills carved
            // `desired` (up to 512 KiB) for a thread that then used ~100 bytes
            // before its next native call: 70 % of the 5.2 GB carved. A thread
            // that really allocates fast pays one extra refill per pause
            // (its cap drains, the drain's refill is `desired`).
            s.refill_after_early_retire = true;
            return s.desired.min(cap);
        }
        s.refill_after_early_retire = false;
        s.desired
    }

    /// gcg g1/j — `Some(cap)` when this buffer was RETIRED before it was well
    /// used (the ladder's own 75 % line, `refill_request_size`), i.e. an early
    /// retire rather than a drain: `cap` is twice the bytes the retire banked,
    /// on the 8-byte grid, in `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]`. `None` for a
    /// live buffer (a fast-path miss: the refill site retires only AFTER it
    /// asks), for a buffer that never held a chunk ([`Tlab::empty`]), and for a
    /// well-used retire. Reads only state the retire banked
    /// (`TlabPressureTracker::bank_retire`), so it is exact after `retire` has
    /// nulled the pointers.
    ///
    /// gcj j3/yg3 R4: the floor is `min_size`, the VM's `MinTLABSize`
    /// ([`TlabSizing::min_size`]: a typed value, else [`MIN_TLAB_SIZE`]), as
    /// HotSpot's `MinTLABSize` bounds every TLAB it carves. With
    /// `-XX:MinTLABSize=2048` a buffer retired after 512 bytes re-carves
    /// 2 KiB, not the 8 KiB constant.
    fn early_retire_refill_cap(&self, min_size: usize) -> Option<usize> {
        let p = &self.pressure;
        if !(p.has_history && p.retired_since_refill && self.is_retired()) {
            return None;
        }
        let floor = min_size.min(MAX_TLAB_SIZE);
        let carved = p.last_refill_size.max(floor);
        let used = p.banked_consumed;
        if used.saturating_mul(4) >= carved.saturating_mul(3) {
            return None;
        }
        Some((used.saturating_mul(2).saturating_add(7) & !7).clamp(floor, MAX_TLAB_SIZE))
    }

    /// Count one GRANTED refill against the share sizer's window — call it on
    /// the NEW buffer, after [`Self::adopt_share_sizer`], with the size the
    /// heap actually granted. A window that reaches `2 × target_refills`
    /// refills is underpredicted, and its desired size doubles in place for
    /// the rest of the window (see [`TlabShareSizer`]'s deviation note).
    pub fn note_share_refill(&mut self, granted: usize) {
        self.note_share_refill_sized(granted, &TlabSizing::DEFAULT);
    }

    /// [`Self::note_share_refill`] under the VM's TLAB flags: with
    /// [`TlabSizing::raise_by_granted_bytes`] (gcj j3/yg3,
    /// `CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`, opt-in, Generational only)
    /// the window counts granted KiB and raises when they reach
    /// `2 x target_refills x desired`, the same threshold in bytes, so a window
    /// of full grants raises exactly as the refill count does, and a window of
    /// short grants (the smallest-whole refill's whole blocks below the
    /// request, the fragmentation fallback's mini-TLABs) no longer doubles
    /// `desired` toward 1 MiB on 1/10 of the bytes. The field
    /// `refills_in_window` then holds KiB; the rule is fixed per VM, so a
    /// thread's window never mixes the two units. Without the flag: the
    /// refill count, byte for byte.
    pub fn note_share_refill_sized(&mut self, granted: usize, sizing: &TlabSizing) {
        let s = &mut self.share;
        if s.desired == 0 {
            // Not sized by the share sizer (the switch was off when asked).
            return;
        }
        TLAB_SIZER_REFILLS.fetch_add(1, Ordering::Relaxed);
        TLAB_SIZER_GRANTED_BYTES.fetch_add(granted as u64, Ordering::Relaxed);
        TLAB_SIZER_MIN_DESIRED.fetch_min(s.desired as u64, Ordering::Relaxed);
        TLAB_SIZER_MAX_DESIRED.fetch_max(s.desired as u64, Ordering::Relaxed);
        // gcg g1/j: a refill after an under-used early retire is not
        // underprediction (see `share_refill_request`). One-shot: the mark
        // belongs to the request this grant answers.
        if std::mem::take(&mut s.refill_after_early_retire) {
            return;
        }
        // gcj j3/yg3: one unit per refill, or the granted KiB (at least one)
        // under the byte rule; the threshold in the same unit.
        let (step, threshold) = share_raise_step_and_threshold(
            sizing.raise_by_granted_bytes(),
            granted,
            s.desired,
            s.target_refills(),
        );
        s.refills_in_window = s.refills_in_window.saturating_add(step);
        // gci i45/sc: `2 x target_refills` at the VM's `TLABWasteTargetPercent`;
        // never under `-XX:-ResizeTLAB` (the size is frozen).
        if s.resize
            && s.refills_in_window as usize >= threshold
            && s.desired < share_sizer_max().min(sizing.max_size)
        {
            s.set_desired(s.desired.saturating_mul(2).min(sizing.max_size));
            s.refills_in_window = 0;
            TLAB_SIZER_RAISES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The HotSpot refill-waste limit, on a fast-path MISS of `size` bytes:
    /// should the caller KEEP this buffer and allocate the object outside it,
    /// instead of retiring the buffer's tail as a filler?
    ///
    /// `true` iff the share sizer is on for this buffer, the thread has been
    /// sized, and the remaining tail exceeds the refill-waste limit — and then
    /// the limit is raised by `desired / 64`, so repeated misses retire soon
    /// (see [`TlabShareSizer`]). `false` leaves the retire-and-refill path
    /// exactly as it was. `size` is only counted.
    ///
    /// The caller's fallback must allocate in the YOUNG generation; a caller
    /// whose `None` means "young is exhausted, spill to old" must not ask — see
    /// `tlab_alloc_shaped_inner`. The JIT helpers' slow paths are young-FIRST,
    /// not young-only, so since gen r5w5/sizer9 the refill site asks only when
    /// young's bump tail can hold the object (`VmHeap::young_bump_headroom`,
    /// after [`Self::keep_may_apply`]); before, a keep on a nearly full young
    /// generation spilled the object to the old one.
    pub fn keep_on_miss(&mut self, size: usize) -> bool {
        self.keep_on_miss_sized(size, &TlabSizing::DEFAULT)
    }

    /// [`Self::keep_on_miss`] under the VM's TLAB flags: with
    /// [`TlabSizing::hotspot_waste_limit`] (gcj j2/yg2,
    /// `CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`) the limit rises by HotSpot's
    /// `TLABWasteIncrement` words (`refill_waste_limit_increment()`; `0` never
    /// raises it), otherwise by `desired / TLABRefillWasteFraction` as before.
    /// `TlabSizing::DEFAULT` is [`Self::keep_on_miss`] byte for byte.
    pub fn keep_on_miss_sized(&mut self, size: usize, sizing: &TlabSizing) -> bool {
        if !self.keep_may_apply() {
            return false;
        }
        let s = &mut self.share;
        // gci i45/sc: at the VM's `TLABRefillWasteFraction` (64 unless typed).
        let raise = if sizing.hotspot_waste_limit {
            (sizing.waste_increment_words as usize).saturating_mul(8)
        } else {
            (s.desired / s.refill_waste_fraction.max(1) as usize).max(8)
        };
        s.refill_waste_limit = s.refill_waste_limit.saturating_add(raise);
        TLAB_SIZER_KEEPS.fetch_add(1, Ordering::Relaxed);
        TLAB_SIZER_KEEP_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        // gci i28/tl: HotSpot's `record_slow_allocation` is this event. gcj
        // j2/yg2: 24 bits wide, the high byte in the share sizer.
        let slow = self.cycle_slow_allocations().wrapping_add(1) & TLAB_CYCLE_COUNTER_MASK;
        self.cycle.slow_allocations = slow as u16;
        self.share.cycle_slow_hi = (slow >> 16) as u8;
        true
    }

    /// gci i28/tl: this thread's TLAB cycle counters, read from the outgoing
    /// buffer by the refill site (carried with
    /// [`Self::adopt_cycle_mark_for_refill`]) and by the pause (the `TLAB
    /// totals` line, [`TlabTotalsState::publish`]).
    pub fn cycle_mark(&self) -> TlabCycleMark {
        TlabCycleMark {
            refills: self.cycle_refills(),
            slow_allocations: self.cycle_slow_allocations(),
            refill_waste_words: self.cycle.refill_waste_words,
            gc_waste_words: self.gc_waste_words,
        }
    }

    /// gcj j2/yg2: the 24-bit refill counter (low 16 bits in
    /// [`TlabCycleCounters`], the high byte in the share sizer).
    fn cycle_refills(&self) -> u32 {
        u32::from(self.cycle.refills) | (u32::from(self.share.cycle_refills_hi) << 16)
    }

    /// gcj j2/yg2: the 24-bit slow-allocation counter (low 16 bits in
    /// [`TlabCycleCounters`], the high byte in the share sizer).
    fn cycle_slow_allocations(&self) -> u32 {
        u32::from(self.cycle.slow_allocations) | (u32::from(self.share.cycle_slow_hi) << 16)
    }

    /// gci i28/tl: install the outgoing buffer's counters on its replacement
    /// and count the refill that installed it. The refill site calls this
    /// beside [`Self::adopt_share_sizer`], only for a granted refill -- AFTER
    /// it (gcj j2/yg2: the counters' high bytes live in the share sizer, so an
    /// adopt of the sizer after this one would put back the outgoing values).
    pub fn adopt_cycle_mark_for_refill(&mut self, carried: TlabCycleMark) {
        let refills = carried.refills.wrapping_add(1) & TLAB_CYCLE_COUNTER_MASK;
        let slow = carried.slow_allocations & TLAB_CYCLE_COUNTER_MASK;
        self.cycle = TlabCycleCounters {
            refills: refills as u16,
            slow_allocations: slow as u16,
            refill_waste_words: carried.refill_waste_words,
        };
        self.share.cycle_refills_hi = (refills >> 16) as u8;
        self.share.cycle_slow_hi = (slow >> 16) as u8;
        self.gc_waste_words = carried.gc_waste_words;
    }

    /// gci i28/tl: [`Self::retire`] for the refill site's retire of the
    /// outgoing buffer (`tlab_alloc_shaped_inner`): the same retire, with the
    /// buried tail counted as REFILL waste (HotSpot's `_refill_waste`, the
    /// `slow:` figure) instead of collection waste (`gc:`).
    pub fn retire_for_refill(&mut self) {
        let before = self.gc_waste_words;
        self.retire();
        let tail = self.gc_waste_words.wrapping_sub(before);
        self.gc_waste_words = before;
        self.cycle.refill_waste_words = self.cycle.refill_waste_words.wrapping_add(tail);
    }

    /// Would [`Self::keep_on_miss`] keep this buffer right now? The same test,
    /// with no side effect: no limit raise, no count (gen r5w5/sizer9).
    ///
    /// The refill site asks this FIRST and only then pays for the second
    /// condition a keep needs — that young can serve the missed object from
    /// its bump tail (`VmHeap::young_bump_headroom`), so the caller's fallback
    /// allocates it YOUNG, as this method's contract requires. Without that
    /// check a keep on a nearly full young generation sent the object down the
    /// JIT helpers' old-generation spill (`try_alloc_object_full`,
    /// `try_alloc_array_full`) while the kept buffer still held up to a
    /// megabyte of young: a keep that tenures what it was meant to keep young.
    pub fn keep_may_apply(&self) -> bool {
        self.tuning.share_sizer
            && !self.is_retired()
            && self.share.desired != 0
            && self.remaining() > self.share.refill_waste_limit
    }

    /// This thread's sizer state, to carry across a refill (`Tlab::new` builds
    /// a fresh struct) — read it from the outgoing buffer.
    pub fn share_sizer(&self) -> TlabShareSizer {
        self.share
    }

    /// Install the outgoing buffer's sizer state on its replacement. The
    /// refill site calls this beside `adopt_allocation_total`.
    pub fn adopt_share_sizer(&mut self, share: TlabShareSizer) {
        self.share = share;
    }

    /// Arm (or disarm) the refill-waste limit ([`Self::keep_on_miss`]) on
    /// this buffer with the share sizer's setting for the heap that carved it
    /// — gen r5w4/defaults8. `Tlab::new` latches the heap-less reading (see
    /// `TlabTuning::share_sizer`); the refill site, which knows the heap
    /// (`VmHeap::tlab_share_sizer_enabled`), calls this beside
    /// [`Self::adopt_share_sizer`] with the same answer that decided whether
    /// the share sizer sized the request, so the two halves of the policy
    /// cannot disagree on one buffer.
    pub fn arm_share_sizer(&mut self, on: bool) {
        self.tuning.share_sizer = on;
    }

    /// T5.5.1 — Notify the tracker that a new TLAB of `size` bytes has
    /// been installed. Resets internal counters and starts the
    /// fill-time clock for the new window.
    pub fn begin_refill(&mut self, size: usize) {
        self.pressure.begin_refill(size);
    }

    /// T5.5.1 — Immutable access to the adaptive-sizing tracker.
    pub fn pressure_tracker(&self) -> &TlabPressureTracker {
        &self.pressure
    }
}

/// The default TLAB chunk size to request from the arena.
pub fn default_tlab_size() -> usize {
    DEFAULT_TLAB_SIZE
}

/// Maximum object size that uses the TLAB fast path.
pub fn tlab_max_alloc() -> usize {
    TLAB_MAX_ALLOC
}

/// T19.3.G1 — the refill size for a thread with no pressure history.
///
/// Call sites that request a TLAB without consulting a
/// [`TlabPressureTracker`] (e.g. the very first refill on a fresh
/// thread, or test harnesses that don't model per-thread state)
/// should use this instead of [`default_tlab_size`] — it is the
/// documented "start big, then let the adaptive sizer decide"
/// entry point. Currently identical to [`default_tlab_size`] but
/// exposed as a distinct symbol so future tuning can change one
/// without affecting the other.
pub fn initial_refill_size() -> usize {
    INITIAL_REFILL_SIZE
}

/// T19.3.G1 — floor on TLAB refill sizes (bytes).
pub fn min_tlab_size() -> usize {
    MIN_TLAB_SIZE
}

/// Fragmentation-mode TLAB floor — see [`FRAG_TLAB_FLOOR`].
pub fn frag_tlab_floor() -> usize {
    FRAG_TLAB_FLOOR
}

/// T19.3.G1 — cap on TLAB refill sizes (bytes).
pub fn max_tlab_size() -> usize {
    MAX_TLAB_SIZE
}

/// `CRATONVM_DBG_WATCH_ADDR=<hex>`: one young-heap address to narrate.
///
/// The refusal census can say an address is not an object start on the cycle it
/// fails, and nothing in the tree could say how it got that way — every
/// candidate mechanism (a double-issued chunk, a filler over live data, a
/// bump past a retired cursor) had to be tested by its own tripwire, and all of
/// them can be silent while the address is still wrong.
///
/// The addresses this workload fails on repeat exactly across runs, because a
/// semispace is reset and re-served from the same base every cycle. That makes
/// one absolute address a usable handle: arm it, and the allocator, the
/// retire path and the object-start walk each say what they did to it, in
/// order, with the collection number.
fn watch_addr() -> usize {
    static A: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *A.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| {
                usize::from_str_radix(
                    v.trim().trim_start_matches("0x").trim_start_matches("0X"),
                    16,
                )
                .ok()
            })
            .unwrap_or(0)
    })
}

/// Does `[lo, hi)` cover the watched address? `false` when unarmed.
pub fn watch_covers(lo: usize, hi: usize) -> bool {
    let w = watch_addr();
    w != 0 && w >= lo && w < hi
}

/// The watched address, or 0.
pub fn watched() -> usize {
    watch_addr()
}

/// The collection count, published by the generational heap each cycle so the
/// TLAB paths — which hold no heap handle — can date their own reports.
pub static WATCH_COLLECTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Emit one line of the watched address's life story.
pub fn watch_note(what: std::fmt::Arguments<'_>) {
    eprintln!(
        "[watch 0x{:x}] collection={} {what}",
        watch_addr(),
        WATCH_COLLECTION.load(std::sync::atomic::Ordering::Relaxed),
    );
}

/// Times [`Tlab::install_tail_filler`] found a live object at the cursor it was
/// about to fill from. See the tripwire there; a non-zero count is a
/// use-after-free in waiting, not a diagnostic curiosity.
///
/// gen r5w5/sizer9: also counts a retire whose tail read zero at its head but
/// not at its last word, which the tail sink's gate refuses
/// (`Tlab::tail_is_clean_for_sink`) — the same finding (the tail is not the
/// unused span it claims to be), caught one word further along.
pub static FILLER_OVER_OBJECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Times a TLAB refill handed out a chunk that already held an object. See the
/// tripwire in `GenerationalHeap::refill_tlab`.
pub static REFILL_OVER_OBJECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Round-5 #9 / round-7 #9 — synthetic class id used for `int[]` TLAB
/// fillers installed at TLAB retire time. Picked from the high-bit
/// "synthetic VM" class-id range (the same kind of reserved sentinel as
/// `AUTOBOX_CLASS_ID`, now `u32::MAX`) so it cannot collide with a
/// classloader-issued id.
pub const TLAB_FILLER_CLASS_ID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0xF111_E700);

/// Bug-D fix (2026-06-12) — synthetic class id stamped into a
/// **sub-`HEADER_SIZE`** TLAB tail that is too small to hold a walkable
/// `int[]` filler. A standard filler needs a full `ObjectHeader`, i.e.
/// `HEADER_SIZE` bytes; a shorter tail cannot carry one, and zeroing it is
/// unsafe because a zeroed sub-`HEADER_SIZE` region is byte-identical to a
/// live `new Object()` (class_id 0, num_slots 0 — its only non-zero header
/// word, identity_hash at offset 8, lies past an 8-byte gap). The non-moving
/// young sweep's linear walk then cannot distinguish the gap from a live
/// object and desyncs.
///
/// gengc-round2-alloc2, 2026-09-20: this doc used to say "8/16/24/32 bytes"
/// and "needs >= 40 bytes", both written when `HEADER_SIZE` was 32 and the
/// header was 40. `cratonvm_types::HEADER_SIZE` is **16** today, so the only
/// tail this sentinel is ever stamped into is exactly **8 bytes** — the
/// sentinel fills it. The arithmetic matters: gengc-round1-alloc sized a test
/// on the stale range and wrote one for a 24-byte tail, which takes the
/// `int[]` arm instead. Read the constant, not this sentence.
///
/// The sentinel occupies only the first 8 bytes of the gap — `class_id` at
/// offset 0, the exact gap length (bytes) as a `u32` at offset 4 — both of
/// which fit in the smallest (8-byte) gap. The sweep walker recognises this
/// class id, reads the length, and reclaims the span in O(1) with no
/// heuristic re-sync. Distinct from `TLAB_FILLER_CLASS_ID` so the two filler
/// kinds never alias. Sits in the same synthetic high-bit range.
pub const GAP_FILLER_CLASS_ID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0xF111_E701);

/// Round-5 #9 / round-7 #9 — class id every TLAB tail filler should use.
///
/// Heap walkers that see an array with this class id can treat the bytes
/// as dead-on-arrival filler, but the layout is a perfectly valid
/// `int[]` so even walkers that do not recognise the sentinel will skip
/// past it correctly using the normal array-size formula.
#[inline]
pub fn tlab_filler_class_id() -> cratonvm_types::ClassId {
    TLAB_FILLER_CLASS_ID
}

// ---------------------------------------------------------------------------
// T5.5.1 — TlabPressureTracker: per-thread, event-driven adaptive sizing
// ---------------------------------------------------------------------------

/// T5.5.1 — Per-thread allocation-pressure tracker for TLAB sizing.
///
/// This tracker reacts to each TLAB refill. It records the wall-clock
/// time the current TLAB has been in use and the number/size classes
/// of allocations made against it. When the TLAB is retired the tracker
/// decides whether the next refill should grow, shrink, or stay the
/// same size based on a simple heuristic:
///
/// - **Grow (double, cap at [`MAX_TLAB_SIZE`])** — TLAB filled in
///   under [`FAST_REFILL_THRESHOLD_MS`] ms of wall clock OR after
///   fewer than [`FAST_REFILL_ALLOC_COUNT`] allocations when most of
///   them are > [`LARGE_ALLOC_THRESHOLD`] bytes (indicating the thread
///   is pushing bulk throughput).
/// - **Shrink (halve, floor at [`MIN_TLAB_SIZE`])** — TLAB took more
///   than [`SLOW_REFILL_THRESHOLD_MS`] ms OR fewer than
///   [`SLOW_REFILL_ALLOC_COUNT`] allocations fired before the refill
///   window closed (the TLAB is oversized for this thread).
/// - **Keep** — neither trigger hit.
pub struct TlabPressureTracker {
    /// Total bytes allocated against the current TLAB since the last refill.
    ///
    /// CRIT-1 fix: this was an `AtomicUsize` but the `Tlab` that owns
    /// the tracker is per-thread (`unsafe impl Send for Tlab`) and is
    /// only ever borrowed `&mut` from its owning thread. The atomic
    /// served no purpose and roughly doubled the cost of `Tlab::alloc`.
    pub allocations_since_last_refill: u64,
    /// Size of the most recent TLAB refill (bytes). Starts at
    /// [`DEFAULT_TLAB_SIZE`] so the first refill uses a sane default.
    pub last_refill_size: usize,
    /// Number of allocation calls against the current TLAB.
    pub alloc_count: u64,
    /// Number of allocations > [`LARGE_ALLOC_THRESHOLD`] bytes against
    /// the current TLAB.
    pub large_alloc_count: u64,
    /// Instant the current TLAB was handed to the thread. Used to
    /// compute wall-clock fill time.
    ///
    /// CRIT-1 fix: was `parking_lot::Mutex<Instant>`. There is no
    /// second writer — the field is exclusive to the owning thread.
    refill_started_at: Instant,
    /// TLAB AUDIT — has the owning [`Tlab`] been retired since the last
    /// [`Self::begin_refill`]?
    ///
    /// Diagnostic only: it exists so [`Tlab::next_refill_size`] can
    /// `debug_assert!` the "size the outgoing TLAB BEFORE retiring it" ordering
    /// that the refill protocol depends on and that nothing else can detect
    /// (the misordering degrades silently into the JIT-blind shrink ratchet).
    /// A plain `bool` rather than a `#[cfg(debug_assertions)]` field so the
    /// struct has one shape in every build; it is cold and the `Tlab` layout the
    /// JIT depends on (`cursor`/`end` at offsets 0 and 8) is unaffected, since
    /// the tracker sits after `start`.
    retired_since_refill: bool,
    /// Has this tracker ever started a refill window? `false` only for the
    /// tracker of a [`Tlab::empty`] buffer — a thread that has never been
    /// handed a chunk. See [`Tlab::refill_request_size`].
    ///
    /// gen r4/alloc (2026-09-23). The refill site told "never sized" apart
    /// from "sized, then retired" with `Tlab::is_empty()`, which is true of
    /// BOTH, so every refill after an early retire (a park, a blocking native,
    /// a forced GC) re-armed at the flat 256 KiB baseline and this tracker was
    /// never consulted (`gengc-alloc2-adaptive-tlab-sizer-is-bypassed-after-every-early-retire-DONE-20260929.md`).
    has_history: bool,
    /// The outgoing buffer's consumption and fill time, BANKED by the first
    /// retire of this refill window, so the window can still be sized after
    /// `retire` has nulled the cursor it would otherwise read. `(0, 0)` until
    /// then. Written once per window: an idempotent second retire has no live
    /// `start` and does not overwrite it.
    banked_consumed: usize,
    banked_elapsed_ms: u128,
}

impl TlabPressureTracker {
    /// Create a new tracker, seeded with the default TLAB size.
    pub fn new() -> Self {
        Self {
            allocations_since_last_refill: 0,
            last_refill_size: DEFAULT_TLAB_SIZE,
            alloc_count: 0,
            large_alloc_count: 0,
            refill_started_at: Instant::now(),
            retired_since_refill: false,
            has_history: false,
            banked_consumed: 0,
            banked_elapsed_ms: 0,
        }
    }

    /// Record one allocation of `size` bytes against the current TLAB.
    ///
    /// Note: `Tlab::alloc` inlines these field updates directly rather
    /// than calling through this helper, so that the bump fast path
    /// avoids any function-call boundary. This method remains for
    /// callers that want to record allocations outside `Tlab::alloc`
    /// (e.g. tests, slow-path bookkeeping).
    #[inline]
    pub fn record_allocation(&mut self, size: usize) {
        self.allocations_since_last_refill += size as u64;
        self.alloc_count += 1;
        if size > LARGE_ALLOC_THRESHOLD {
            self.large_alloc_count += 1;
        }
    }

    /// Mark the beginning of a new TLAB lifetime. Call this after a
    /// refill so that the next call to [`next_refill_size`] can measure
    /// how long the just-retired TLAB was in use.
    pub fn begin_refill(&mut self, refill_size: usize) {
        self.last_refill_size = refill_size;
        self.allocations_since_last_refill = 0;
        self.alloc_count = 0;
        self.large_alloc_count = 0;
        self.refill_started_at = Instant::now();
        // A fresh TLAB lifetime: the outgoing one has been sized and replaced,
        // so the "sized before retired" tripwire is disarmed again.
        self.retired_since_refill = false;
        self.has_history = true;
        self.banked_consumed = 0;
        self.banked_elapsed_ms = 0;
    }

    /// Bank the closing window's consumption and fill time. Called by
    /// [`Tlab`]'s retire tail for a buffer that still held a chunk, i.e. once
    /// per window. See [`Self::banked_consumed`].
    fn bank_retire(&mut self, consumed: usize) {
        self.banked_consumed = consumed;
        self.banked_elapsed_ms = self.refill_started_at.elapsed().as_millis();
    }

    /// Compute the next TLAB refill size based on the heuristic.
    ///
    /// The current TLAB must be retired (fully exhausted) by the
    /// caller before invoking this — the tracker examines counters
    /// collected during the just-finished TLAB lifetime and decides
    /// whether to grow, shrink, or keep the size.
    ///
    /// The returned value is guaranteed to satisfy
    /// `MIN_TLAB_SIZE <= size <= MAX_TLAB_SIZE`.
    pub fn next_refill_size(&self) -> usize {
        // No external consumption measure — the tracker's own byte tally is
        // the whole truth for a caller that does not own a live `Tlab`.
        self.next_refill_size_with_consumed(0)
    }

    /// [`Self::next_refill_size`], told how many bytes the TLAB *actually*
    /// handed out.
    ///
    /// `consumed_bytes` comes from [`Tlab::consumed_bytes`] (the live
    /// `cursor - start` span) and is the only signal that sees the JIT's
    /// inline bump — see the note on [`Tlab::next_refill_size`] for the
    /// one-way shrink ratchet this closes. The tracker's own tally is still
    /// used when it is the larger of the two, so nothing about the
    /// interpreter path's behaviour changes.
    pub fn next_refill_size_with_consumed(&self, consumed_bytes: usize) -> usize {
        let elapsed_ms = self.refill_started_at.elapsed().as_millis();
        self.next_refill_size_at(consumed_bytes, elapsed_ms)
    }

    /// The sizing heuristic itself, against an explicit fill time. Both
    /// entry points reduce to this: [`Self::next_refill_size_with_consumed`]
    /// with the window's elapsed time NOW (a live buffer), and
    /// [`Tlab::refill_request_size`] with the time BANKED at retire — for a
    /// retired buffer "now" includes however long the thread then parked, and
    /// a thread that drained its buffer in 2 ms and parked for a second must
    /// not be read as a slow filler.
    fn next_refill_size_at(&self, consumed_bytes: usize, elapsed_ms: u128) -> usize {
        let alloc_count = self.alloc_count as usize;
        let large_allocs = self.large_alloc_count as usize;
        let current = self.last_refill_size;

        // Did the thread actually drain this TLAB? `alloc_count` cannot
        // answer that for compiled code, but the byte high-water mark can.
        // 75% is deliberately below 100%: the last object that did not fit is
        // what ends a TLAB's life, so a fully-drained TLAB still reports a
        // short tail, and a partially-filled one that was retired early
        // (refill for an oversized object) should not read as pressure.
        let bytes = (self.allocations_since_last_refill as usize).max(consumed_bytes);
        let well_used = current > 0 && bytes.saturating_mul(4) >= current.saturating_mul(3);

        // Grow when: fast fill OR few-but-large allocations dominated OR the
        // TLAB was genuinely drained within a sane window (the JIT-visible
        // arm — `alloc_count` is blind to compiled allocation).
        let grow = elapsed_ms <= FAST_REFILL_THRESHOLD_MS
            || (alloc_count < FAST_REFILL_ALLOC_COUNT && large_allocs > 0)
            || (well_used && elapsed_ms < SLOW_REFILL_THRESHOLD_MS);

        // Shrink when: very slow fill OR the TLAB was barely touched. "Barely
        // touched" now requires the BYTE evidence to agree — a drained TLAB
        // is never idle, however few allocations this tracker saw.
        let shrink = elapsed_ms > SLOW_REFILL_THRESHOLD_MS
            || (alloc_count < SLOW_REFILL_ALLOC_COUNT && !well_used);

        let next = if grow && !shrink {
            if well_used && elapsed_ms <= FAST_REFILL_THRESHOLD_MS && current >= DEFAULT_TLAB_SIZE {
                current.saturating_mul(4).min(MAX_TLAB_SIZE)
            } else {
                current.saturating_mul(2).min(MAX_TLAB_SIZE)
            }
        } else if shrink && !grow {
            // `current` is only guaranteed to be a multiple of 8 (it can be
            // seeded from a truncated, arena-nearly-full grant — see
            // `refill_tlab`), not a multiple of 16+, so halving it can drop
            // below the 8-byte alignment `Tlab::new` requires of the final
            // refill size. Re-mask after the divide so every value this
            // heuristic ever produces stays 8-aligned.
            ((current / 2) & !7).max(MIN_TLAB_SIZE)
        } else {
            current
        };
        next.clamp(MIN_TLAB_SIZE, MAX_TLAB_SIZE)
    }
}

impl Default for TlabPressureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TlabPressureTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlabPressureTracker")
            .field(
                "allocations_since_last_refill",
                &self.allocations_since_last_refill,
            )
            .field("last_refill_size", &self.last_refill_size)
            .field("alloc_count", &self.alloc_count)
            .field("large_alloc_count", &self.large_alloc_count)
            .finish()
    }
}



/// gci i28/tl (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 9, the `TLAB
/// totals` half): the per-thread cycle counters, their carry across a refill,
/// and HotSpot's `ThreadLocalAllocStats::publish` line.
#[cfg(test)]
mod gci_i28tl_tlab_totals_tests;

/// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
/// the TLAB consumer half): the VM's TLAB flags reach the sizer.
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i45_sc_`.
#[cfg(test)]
mod gci_i45_sc_tlab_flags_tests;

static OWNED_VM_TLAB_CHUNK_EPOCH_HOOK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub fn register_owned_vm_tlab_chunk_epoch_hook(f: fn(usize, usize) -> Option<u64>) {
    OWNED_VM_TLAB_CHUNK_EPOCH_HOOK.store(f as usize, std::sync::atomic::Ordering::Release);
}
pub fn call_owned_vm_tlab_chunk_epoch(lo: usize, hi: usize) -> Option<u64> {
    let p = OWNED_VM_TLAB_CHUNK_EPOCH_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if p == 0 { return None; }
    let f: fn(usize, usize) -> Option<u64> = unsafe { std::mem::transmute(p) };
    f(lo, hi)
}
