// SPDX-License-Identifier: Apache-2.0
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
/// |  40..  | pressure | rest |
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
    /// Whether this buffer's consumption is the PROGRAM's allocation, and so
    /// belongs in the VM's allocation total. See [`TlabAccounting`].
    accounting: TlabAccounting,
    /// Latched when the buffer is carved. The dead-reference-store tripwire
    /// is a process-start diagnostic, but consulting `flags()` here used to
    /// make every bump allocation read the process-wide `FLAGS` cell.
    dbg_deadref_store: bool,
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
    /// gen r4/alloc (2026-09-23), the same fix `dbg_deadref_store` above
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
    /// Opt-in sizing / retire policies, latched when the buffer is carved.
    /// See [`TlabTuning`]. gen r4w3/alloc3 (2026-09-23); after `watch_addr`,
    /// so the JIT's fixed offsets are untouched.
    tuning: TlabTuning,
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
    /// Samples folded into the average (drives the warm-up weight).
    samples: u32,
    /// What every refill in the current window requests; 0 before the first.
    desired: usize,
    /// HotSpot's `_refill_waste_limit`: the largest tail a miss may retire.
    refill_waste_limit: usize,
    /// Refills sized in the current window.
    refills_in_window: u32,
}

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
        }
    }
}

/// `CRATONVM_TLAB_SHARE_SIZER_MAX_KIB` — the share sizer's largest desired
/// size, in KiB, clamped to `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]` and to the 8-byte
/// grid; unset or unparsable = `MAX_TLAB_SIZE` (1 MiB), the cap it always had.
/// gen r5w5/sizer9: a bisection lever for
/// `docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`
/// mechanism A, which the orchestrator measured only once the sizer grew
/// buffers to 700 KiB–1 MiB (the ladder re-arms at 256 KiB after every
/// retire). Read once per process, like `CRATONVM_GEN_CONC_MARK_SLICE`: a
/// tuning knob, not per-VM state.
pub fn share_sizer_max() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_TLAB_SHARE_SIZER_MAX_KIB")
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
/// HotSpot's `TLABRefillWasteFraction`: the refill-waste limit is
/// `desired / 64`, and so is each raise of it (see [`TlabShareSizer`]).
const TLAB_REFILL_WASTE_FRACTION: usize = 64;

impl TlabShareSizer {
    /// Fold one sample (bytes allocated per collection interval) into the
    /// average. HotSpot's `AdaptiveWeightedAverage`: the weight is
    /// `max(TLABAllocationWeight, 100 / samples)` percent, so the first sample
    /// IS the average and the next few move it quickly.
    fn sample(&mut self, bytes_per_cycle: f64) {
        self.samples = self.samples.saturating_add(1);
        let weight = TLAB_ALLOCATION_WEIGHT_PERCENT.max(100 / self.samples) as f64 / 100.0;
        self.avg_bytes_per_cycle =
            (1.0 - weight) * self.avg_bytes_per_cycle + weight * bytes_per_cycle;
    }

    /// `avg / target_refills`, clamped to `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]` and
    /// kept on the 8-byte grid `Tlab::new` requires.
    fn desired_from_average(avg_bytes_per_cycle: f64) -> usize {
        let raw = avg_bytes_per_cycle / TLAB_TARGET_REFILLS as f64;
        // `as usize` saturates for a huge or non-finite value and maps NaN to 0.
        let raw = if raw.is_finite() && raw > 0.0 {
            raw as usize
        } else {
            0
        };
        raw.clamp(MIN_TLAB_SIZE, MAX_TLAB_SIZE) & !7
    }

    /// Install a new desired size and re-arm the refill-waste limit to it.
    ///
    /// gen r5w5/sizer9: capped at [`share_sizer_max`] (1 MiB unless
    /// `CRATONVM_TLAB_SHARE_SIZER_MAX_KIB` lowers it), on the 8-byte grid.
    fn set_desired(&mut self, desired: usize) {
        let desired = if desired > share_sizer_max() {
            share_sizer_max()
        } else {
            desired
        };
        self.desired = desired;
        self.refill_waste_limit = desired / TLAB_REFILL_WASTE_FRACTION;
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
    /// `docs/internal/gc/gengc-r4-alloc-tlab-memory-is-zeroed-twice-FIXED-20260929.md`.
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
pub(crate) fn tlab_tripwire_counts() -> (u64, u64) {
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
/// `docs/internal/gc/gengc-alloc-tlab-instruments-are-write-only-FIXED-20260924.md`
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
    let s = tlab_sizer_stats();
    let on = cratonvm_types::flags().gc.tlab_share_sizer_for(generational);
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
    retire_is_wasteful(carved, consumed)
        && carved - consumed.min(carved) >= TLAB_MAX_ALLOC as u64
}

/// May [`Tlab::install_tail_filler`] skip the memset of its `int[]` filler's
/// DATA area? gen r4w3/alloc3 (2026-09-23), write (3) of
/// `docs/internal/gc/gengc-r4-alloc-tlab-memory-is-zeroed-twice-FIXED-20260929.md`.
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
}

/// The one [`JitZgcAnnounceTable`].
pub static JIT_ZGC_ANNOUNCE: JitZgcAnnounceTable = JitZgcAnnounceTable {
    words: AtomicUsize::new(0),
    base: AtomicUsize::new(0),
    span: AtomicUsize::new(0),
    blocked: AtomicUsize::new(0),
    epoch: AtomicU64::new(1),
    owner: AtomicUsize::new(0),
};

/// Address of [`JIT_ZGC_ANNOUNCE`], as stored in `Tlab::zgc_announce`.
pub fn jit_zgc_announce_table_addr() -> usize {
    &JIT_ZGC_ANNOUNCE as *const JitZgcAnnounceTable as usize
}

/// `(zgc_owned_epoch, zgc_announce)` for a buffer over `[lo, hi)`.
#[cfg(feature = "zgc")]
fn zgc_inline_announce_for_chunk(lo: usize, hi: usize) -> (u64, usize) {
    if JIT_ZGC_ANNOUNCE.words.load(Ordering::Acquire) == 0 {
        return (0, 0);
    }
    // Only a chunk of the PUBLISHING heap: the JIT re-checks each object
    // against `[base, base + span)` anyway, so this is not what makes it
    // sound -- it keeps a second heap's buffers from paying for a check that
    // can never pass.
    let base = JIT_ZGC_ANNOUNCE.base.load(Ordering::Acquire);
    let span = JIT_ZGC_ANNOUNCE.span.load(Ordering::Acquire);
    if lo < base || hi > base.saturating_add(span) {
        return (0, 0);
    }
    match crate::zgc::owned_vm_tlab_chunk_epoch(lo, hi) {
        Some(epoch) => (epoch, jit_zgc_announce_table_addr()),
        None => (0, 0),
    }
}

/// Without the ZGC backend there is nothing to announce to.
#[cfg(not(feature = "zgc"))]
fn zgc_inline_announce_for_chunk(_lo: usize, _hi: usize) -> (u64, usize) {
    (0, 0)
}

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
            vm_buffer_noted: 0,
            // A thread that has never refilled has no sizing history.
            share: TlabShareSizer::default(),
            vm_thread_total: None,
            tail_sink: None,
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
        let (zgc_owned_epoch, zgc_announce) =
            zgc_inline_announce_for_chunk(ptr as usize, aligned_end_addr);
        Self {
            start: ptr,
            cursor: ptr,
            end: aligned_end_addr as *mut u8,
            zgc_owned_epoch,
            zgc_announce,
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
    /// `docs/internal/gc/gengc-r4w5-thrash5-blocking-retire-buries-tlab-tail-FIXED-20260927.md`.
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
    /// `docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`.
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
    /// `docs/internal/gc/gengc-r4-plumbing-alloc-denominators-are-not-allocation-FIXED-20260928.md`
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
                        ptr as usize,
                        self.start as usize,
                        cursor,
                        self.end as usize,
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
            TLAB_GAP_TAIL_BYTES.fetch_add(
                end_addr.saturating_sub(cursor_addr) as u64,
                Ordering::Relaxed,
            );
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
            TLAB_GAP_TAIL_BYTES.fetch_add(tail as u64, Ordering::Relaxed);
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
        TLAB_FILLER_TAIL_BYTES.fetch_add(tail as u64, Ordering::Relaxed);
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
    /// `docs/internal/gc/gengc-alloc2-adaptive-tlab-sizer-is-bypassed-after-every-early-retire-DONE-20260929.md`.
    /// The refill site used `is_empty()` to mean "no history", which is also
    /// true of every retired buffer, so the adaptive sizer only ever saw
    /// drains. gce e2/o: the opt-in arm that called this
    /// (`CRATONVM_TLAB_SIZE_RETIRED`) was removed -- superseded by the share
    /// sizer, which accounts an early retire like a drain -- so this method has
    /// no production caller; it is kept with its tests as the ladder's
    /// reference answer. Delete it with the ladder.
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
    /// per collection. An early retire is accounted exactly like a drain — the
    /// bytes it consumed count, the tail it gave up does not — so parking
    /// threads converge on their real allocation rate, and the tail waste they
    /// can produce is bounded by `desired`, which the rate sets.
    ///
    /// Always in `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]`, 8-aligned.
    ///
    /// Asking changes only the WINDOW (it may close one and resize); a refill
    /// is counted by [`Self::note_share_refill`] once it has been granted, so
    /// a refill gate that refuses a thousand times in a row (the fragmentation
    /// wedge) cannot read as a thousand refills and inflate the request —
    /// which would only make the gate refuse harder.
    pub fn share_refill_request(&mut self, epoch: u64) -> usize {
        let now = self.thread_allocated_bytes();
        let s = &mut self.share;
        if s.window_epoch == u64::MAX || s.desired == 0 {
            // The thread's first sized refill: open the first window at the
            // "start big" baseline, as the ladder does.
            s.window_epoch = epoch;
            s.window_start_bytes = now;
            s.refills_in_window = 0;
            s.set_desired(initial_refill_size());
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
            s.set_desired(TlabShareSizer::desired_from_average(s.avg_bytes_per_cycle));
            TLAB_SIZER_SAMPLES.fetch_add(1, Ordering::Relaxed);
        }
        s.desired
    }

    /// Count one GRANTED refill against the share sizer's window — call it on
    /// the NEW buffer, after [`Self::adopt_share_sizer`], with the size the
    /// heap actually granted. A window that reaches `2 × target_refills`
    /// refills is underpredicted, and its desired size doubles in place for
    /// the rest of the window (see [`TlabShareSizer`]'s deviation note).
    pub fn note_share_refill(&mut self, granted: usize) {
        let s = &mut self.share;
        if s.desired == 0 {
            // Not sized by the share sizer (the switch was off when asked).
            return;
        }
        TLAB_SIZER_REFILLS.fetch_add(1, Ordering::Relaxed);
        TLAB_SIZER_GRANTED_BYTES.fetch_add(granted as u64, Ordering::Relaxed);
        TLAB_SIZER_MIN_DESIRED.fetch_min(s.desired as u64, Ordering::Relaxed);
        TLAB_SIZER_MAX_DESIRED.fetch_max(s.desired as u64, Ordering::Relaxed);
        s.refills_in_window = s.refills_in_window.saturating_add(1);
        if s.refills_in_window as usize >= 2 * TLAB_TARGET_REFILLS && s.desired < share_sizer_max() {
            s.set_desired(s.desired.saturating_mul(2).min(MAX_TLAB_SIZE));
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
        if !self.keep_may_apply() {
            return false;
        }
        let s = &mut self.share;
        s.refill_waste_limit = s
            .refill_waste_limit
            .saturating_add((s.desired / TLAB_REFILL_WASTE_FRACTION).max(8));
        TLAB_SIZER_KEEPS.fetch_add(1, Ordering::Relaxed);
        TLAB_SIZER_KEEP_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        true
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
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_WATCH_ADDR")
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
        let grow = elapsed_ms < FAST_REFILL_THRESHOLD_MS
            || (alloc_count < FAST_REFILL_ALLOC_COUNT && large_allocs > 0)
            || (well_used && elapsed_ms < SLOW_REFILL_THRESHOLD_MS);

        // Shrink when: very slow fill OR the TLAB was barely touched. "Barely
        // touched" now requires the BYTE evidence to agree — a drained TLAB
        // is never idle, however few allocations this tracker saw.
        let shrink = elapsed_ms > SLOW_REFILL_THRESHOLD_MS
            || (alloc_count < SLOW_REFILL_ALLOC_COUNT && !well_used);

        let next = if grow && !shrink {
            current.saturating_mul(2).min(MAX_TLAB_SIZE)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// JIT contract — `Tlab` must keep `cursor` at offset 0 and `end`
    /// at offset 8. The JIT-emitted inline bump in `jit/src/x64.rs`
    /// reads these directly from a thread pointer. If a future edit
    /// reorders the struct, this test fails and the editor must
    /// update both [`Tlab::CURSOR_OFFSET`] and [`Tlab::END_OFFSET`].
    #[test]
    fn test_tlab_offsets() {
        let tlab = Tlab::empty();
        let base = &tlab as *const _ as usize;
        let cursor_addr = &tlab.cursor as *const _ as usize;
        let end_addr = &tlab.end as *const _ as usize;
        assert_eq!(
            cursor_addr - base,
            Tlab::CURSOR_OFFSET,
            "Tlab::cursor moved away from offset 0 — update JIT plumbing in lockstep"
        );
        assert_eq!(
            end_addr - base,
            Tlab::END_OFFSET,
            "Tlab::end moved away from offset 8 — update JIT plumbing in lockstep"
        );
        assert_eq!(Tlab::CURSOR_OFFSET, 0);
        assert_eq!(Tlab::END_OFFSET, 8);
    }

    /// JIT contract (round 9 wave 8, `zgc8`): the inline ZGC start-bit store
    /// reads `start`, `zgc_owned_epoch` and `zgc_announce` at fixed offsets,
    /// and the announce table's words at fixed offsets. `jit/src/x64/objects.rs`
    /// has its own copies and pins them to these constants.
    #[test]
    fn the_inline_announce_offsets_are_where_the_jit_reads_them() {
        let tlab = Tlab::empty();
        let base = &tlab as *const _ as usize;
        assert_eq!(&tlab.start as *const _ as usize - base, Tlab::START_OFFSET);
        assert_eq!(
            &tlab.zgc_owned_epoch as *const _ as usize - base,
            Tlab::ZGC_OWNED_EPOCH_OFFSET
        );
        assert_eq!(
            &tlab.zgc_announce as *const _ as usize - base,
            Tlab::ZGC_ANNOUNCE_OFFSET
        );
        assert_eq!(tlab.zgc_inline_announce_state(), (0, 0));

        let t = &JIT_ZGC_ANNOUNCE;
        let tb = jit_zgc_announce_table_addr();
        assert_eq!(tb, t as *const JitZgcAnnounceTable as usize);
        assert_eq!(
            &t.words as *const _ as usize - tb,
            JitZgcAnnounceTable::WORDS_OFFSET
        );
        assert_eq!(
            &t.base as *const _ as usize - tb,
            JitZgcAnnounceTable::BASE_OFFSET
        );
        assert_eq!(
            &t.span as *const _ as usize - tb,
            JitZgcAnnounceTable::SPAN_OFFSET
        );
        assert_eq!(
            &t.blocked as *const _ as usize - tb,
            JitZgcAnnounceTable::BLOCKED_OFFSET
        );
        assert_eq!(
            &t.epoch as *const _ as usize - tb,
            JitZgcAnnounceTable::EPOCH_OFFSET
        );
    }

    /// A buffer over memory no ZGC heap carved is never armed, and a retire
    /// disarms. (The armed case needs a heap: `zgc::vm_tlab`'s tests.)
    #[test]
    fn a_plain_buffer_is_never_armed_for_the_inline_announce() {
        let mut buf = vec![0u64; 128];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr() as *mut u8, 1024) };
        assert_eq!(tlab.zgc_inline_announce_state(), (0, 0));
        let staging = unsafe { Tlab::new_heap_staging(buf.as_mut_ptr() as *mut u8, 1024) };
        assert_eq!(staging.zgc_inline_announce_state(), (0, 0));
        let _ = staging;
        tlab.retire();
        assert_eq!(tlab.zgc_inline_announce_state(), (0, 0));
    }

    #[test]
    fn tlab_basic_alloc() {
        let mut buf = vec![0u8; 1024];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 1024) };
        assert_eq!(tlab.remaining(), 1024);

        let ptr = tlab.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(tlab.remaining(), 1024 - 64);
    }

    #[test]
    fn tlab_initialized_alloc_installs_object_before_publishing_tail() {
        let mut buf = vec![0u8; 128];
        let base = buf.as_mut_ptr() as usize;
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 128) };

        let object = tlab
            .alloc_initialized(40, 8, |ptr| unsafe {
                std::ptr::write(ptr as *mut u32, 0xC0DE_CAFE);
            })
            .expect("TLAB has room for one header-sized object");

        assert_eq!(unsafe { std::ptr::read(object as *const u32) }, 0xC0DE_CAFE);
        assert_eq!(tlab.reserved_tail(), Some((base + 40, base + 128)));
    }

    #[test]
    fn tlab_alignment() {
        let mut buf = vec![0u8; 256];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 256) };

        // Allocate 3 bytes (unaligned)
        tlab.alloc(3, 1).unwrap();
        // Next alloc with align=8 should skip to alignment boundary
        let p2 = tlab.alloc(8, 8).unwrap();
        assert_eq!((p2 as usize) % 8, 0);
    }

    #[test]
    fn tlab_alignment_reserves_compact_object_padding() {
        let mut buf = vec![0u8; 128];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 128) };
        let first = tlab.alloc(44, 8).unwrap();
        let second = tlab.alloc(8, 8).unwrap();

        assert_eq!(second as usize - first as usize, 48);
        assert_eq!(tlab.remaining(), 72);
    }

    #[test]
    fn tlab_exhaustion() {
        let mut buf = vec![0u8; 64];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 64) };
        assert!(tlab.alloc(64, 8).is_some());
        assert!(tlab.alloc(1, 1).is_none());
    }

    #[test]
    fn tlab_empty() {
        let mut tlab = Tlab::empty();
        assert!(tlab.is_empty());
        assert!(tlab.alloc(1, 1).is_none());
    }

    #[test]
    fn tlab_retire() {
        let mut buf = vec![0u8; 128];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 128) };
        assert!(!tlab.is_empty());
        tlab.retire();
        assert!(tlab.is_empty());
        assert!(tlab.alloc(1, 1).is_none());
    }

    // ---------------------------------------------------------------
    // T5.5.1 — TlabPressureTracker tests
    // ---------------------------------------------------------------

    #[test]
    fn pressure_tracker_defaults_to_default_size() {
        let t = TlabPressureTracker::new();
        assert_eq!(t.last_refill_size, DEFAULT_TLAB_SIZE);
        assert_eq!(t.allocations_since_last_refill, 0);
    }

    #[test]
    fn pressure_tracker_records_allocations() {
        let mut t = TlabPressureTracker::new();
        t.record_allocation(64);
        t.record_allocation(2048); // > LARGE_ALLOC_THRESHOLD
        assert_eq!(t.allocations_since_last_refill, 64 + 2048);
        assert_eq!(t.alloc_count, 2);
        assert_eq!(t.large_alloc_count, 1);
    }

    #[test]
    fn pressure_tracker_doubles_on_few_large_allocs() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024); // start at 64 KB
                                   // A handful of large allocations → high pressure.
        for _ in 0..4 {
            t.record_allocation(512); // > LARGE_ALLOC_THRESHOLD
        }
        // alloc_count=4 < FAST(16); large_allocs=4>0 → grow.
        // But alloc_count=4 <= SLOW(4-1) is false (4 < 4 is false) so no shrink.
        let next = t.next_refill_size();
        assert_eq!(next, 128 * 1024);
    }

    #[test]
    fn pressure_tracker_doubles_on_fast_refill() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(32 * 1024);
        // Simulate many allocations but start-time stays recent → elapsed < 1ms.
        for _ in 0..200 {
            t.record_allocation(128);
        }
        let next = t.next_refill_size();
        // Fast-fill path: elapsed_ms < 1 → grow.
        assert_eq!(next, 64 * 1024);
    }

    #[test]
    fn pressure_tracker_halves_on_few_allocations() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024);
        // Only 2 allocations over the window → shrink.
        t.record_allocation(64);
        t.record_allocation(64);
        // Sleep just over the fast threshold so we don't trigger grow.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert_eq!(next, 32 * 1024);
    }

    /// Regression: `next_refill_size`'s shrink branch used to compute
    /// `current / 2` without re-masking to 8-byte alignment. `current`
    /// (`last_refill_size`) can be seeded from a truncated `refill_tlab`
    /// grant (see `gen_heap.rs` / `g1.rs` — the arena/region need only
    /// guarantee multiples of 8, not 16+), so a value like 16392
    /// (multiple of 8, not of 16) halves to 8196 — still inside
    /// `[MIN_TLAB_SIZE, MAX_TLAB_SIZE]` so the final clamp doesn't catch
    /// it, but not 8-aligned. That poisoned "requested size" would flow
    /// straight into `Tlab::new`, tripping its `end`-pointer alignment
    /// contract. Every value this heuristic can ever produce must stay
    /// 8-aligned regardless of what `last_refill_size` was seeded with.
    #[test]
    fn pressure_tracker_shrink_stays_8_aligned_even_from_odd_seed() {
        let mut t = TlabPressureTracker::new();
        // 16392 = 16384 + 8: a multiple of 8, deliberately not of 16.
        t.begin_refill(16392);
        t.record_allocation(64);
        t.record_allocation(64);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert_eq!(
            next % 8,
            0,
            "next_refill_size must stay 8-aligned, got {next}"
        );
    }

    #[test]
    fn pressure_tracker_respects_max_cap() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(MAX_TLAB_SIZE);
        for _ in 0..2 {
            t.record_allocation(512);
        }
        // Would double, but MAX_TLAB_SIZE already at cap.
        let next = t.next_refill_size();
        assert_eq!(next, MAX_TLAB_SIZE);
    }

    // ---------------------------------------------------------------
    // T19.3.G1 — allocation-storm regression tests
    // ---------------------------------------------------------------

    #[test]
    fn t19_default_tlab_size_is_256kb() {
        // The default TLAB baseline is 256 KB — large enough to
        // amortize `refill_tlab` across a Quarkus-style 25 MB/s
        // static-init allocation storm without the 64 KB refill
        // cascade we fixed in T19.3.G1.
        assert_eq!(default_tlab_size(), 256 * 1024);
    }

    #[test]
    fn t19_max_tlab_size_is_1mb() {
        // Adaptive sizer can grow a thread's TLAB up to 1 MB under
        // sustained pressure.
        assert_eq!(max_tlab_size(), 1024 * 1024);
    }

    #[test]
    fn t19_initial_refill_matches_default() {
        // Initial refill on a fresh thread starts at the baseline;
        // tuning one without the other should be an intentional opt-in.
        assert_eq!(initial_refill_size(), default_tlab_size());
    }

    #[test]
    fn t19_min_cap_floor_respected() {
        // Pressure tracker must not undercut MIN_TLAB_SIZE.
        assert_eq!(min_tlab_size(), 8 * 1024);
        let mut t = TlabPressureTracker::new();
        t.begin_refill(MIN_TLAB_SIZE);
        t.record_allocation(16);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(t.next_refill_size(), MIN_TLAB_SIZE);
    }

    #[test]
    fn t19_adaptive_sizer_grows_on_fast_fill() {
        // Simulate the allocation-storm pattern: 2 tiny objects
        // per iteration, 10 k iterations, nearly zero wall clock.
        // The pressure tracker should grow the TLAB at least twice.
        let mut t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024);
        // Fast-fill: elapsed_ms < 1 → grow to 128 KB.
        for _ in 0..500 {
            t.record_allocation(24);
        }
        let step1 = t.next_refill_size();
        assert!(step1 >= 128 * 1024, "first growth step: {step1}");
        // Second round at 128 KB: fast-fill again → 256 KB.
        t.begin_refill(step1);
        for _ in 0..500 {
            t.record_allocation(24);
        }
        let step2 = t.next_refill_size();
        assert!(step2 >= step1, "second growth step: {step2} < {step1}");
    }

    #[test]
    fn t19_adaptive_sizer_grows_from_8kb_to_64kb_under_1m_allocs() {
        // Starts at MIN_TLAB_SIZE (8 KB), ramps to at least 64 KB
        // after repeated fast refills. This is the adaptive target
        // the T19.3.G1 fix requires so KC26 static-init doesn't
        // refill every ~3 ms.
        let mut t = TlabPressureTracker::new();
        let mut size = MIN_TLAB_SIZE;
        t.begin_refill(size);
        // Each loop iteration models filling a TLAB and asking for
        // the next size. We stop once we reach 64 KB or the loop
        // is clearly diverging.
        for _ in 0..16 {
            // Simulate >= 1 allocation so alloc_count doesn't shrink.
            for _ in 0..200 {
                t.record_allocation(24);
            }
            let next = t.next_refill_size();
            if next <= size {
                break;
            }
            size = next;
            t.begin_refill(size);
            if size >= 64 * 1024 {
                break;
            }
        }
        assert!(size >= 64 * 1024, "adaptive sizer stuck at {size} bytes");
    }

    #[test]
    fn t19_tlab_max_alloc_independent_of_default() {
        // Raising the default TLAB to 256 KB does not widen the cap
        // on what is eligible for the TLAB fast path — 32 KB stays.
        assert_eq!(tlab_max_alloc(), 32 * 1024);
    }

    #[test]
    fn t19_allocation_storm_tlab_survives_2m_bumps() {
        // A TLAB sized at the 256 KB default can service about
        // 10k 24-byte objects before a refill. Verify that a raw
        // TLAB actually fulfils ~10k bump requests without a panic
        // and without claiming more than the documented size.
        let mut buf = vec![0u8; 256 * 1024];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 256 * 1024) };
        let mut bumps = 0;
        while tlab.alloc(24, 8).is_some() {
            bumps += 1;
            if bumps > 20_000 {
                panic!("TLAB delivered too many objects — size unbounded?");
            }
        }
        // 256 KB / 24 B = ~10_922 objects with 8-byte alignment.
        assert!(
            bumps >= 10_000,
            "only {bumps} allocations fit in 256 KB TLAB"
        );
    }

    #[test]
    fn t19_adaptive_sizer_shrinks_on_idle() {
        // If a thread goes idle (alloc_count = 1) the TLAB must
        // shrink toward MIN so we don't hold 1 MB of arena per
        // sleeping thread.
        let mut t = TlabPressureTracker::new();
        t.begin_refill(MAX_TLAB_SIZE);
        t.record_allocation(16);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert!(
            next < MAX_TLAB_SIZE,
            "expected shrink from {MAX_TLAB_SIZE}, got {next}"
        );
        assert!(next >= MIN_TLAB_SIZE);
    }

    #[test]
    fn t19_tlab_retire_clears_buffer() {
        // Retiring the TLAB must not leave a dangling pointer that a
        // subsequent `alloc` could dereference.
        let mut buf = vec![0u8; 4096];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 4096) };
        tlab.alloc(64, 8).unwrap();
        tlab.retire();
        assert!(tlab.is_empty());
        assert!(tlab.alloc(1, 1).is_none());
    }

    #[test]
    fn t19_begin_refill_resets_timer() {
        // begin_refill must reset the fill-time clock: the timer
        // recorded before the second begin_refill must not leak into
        // the window the tracker measures after it.
        let mut t = TlabPressureTracker::new();
        t.begin_refill(MIN_TLAB_SIZE);
        std::thread::sleep(std::time::Duration::from_millis(2));
        // The second begin_refill snapshot must capture a fresh
        // Instant; fill-time elapsed is measured from that snapshot
        // and the counters reset to zero.
        t.begin_refill(64 * 1024);
        assert_eq!(t.alloc_count, 0);
        assert_eq!(t.large_alloc_count, 0);
        // A second begin_refill must also reset last_refill_size.
        assert_eq!(t.last_refill_size, 64 * 1024);
    }

    #[test]
    fn pressure_tracker_respects_min_floor() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(MIN_TLAB_SIZE);
        t.record_allocation(16); // one small alloc → shrink trigger
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert_eq!(next, MIN_TLAB_SIZE);
    }

    // ---------------------------------------------------------------
    // Round-5 #9 / round-7 #9 — TLAB tail-filler tests
    // ---------------------------------------------------------------

    #[test]
    fn tlab_filler_consumes_remaining_tail() {
        use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        // Use a buffer well above HEADER_SIZE so the filler has room.
        let mut buf = vec![0u8; 4096];
        // Align buffer pointer up to 8.
        let raw = buf.as_mut_ptr();
        let aligned = ((raw as usize + 7) & !7) as *mut u8;
        let usable = 4096 - (aligned as usize - raw as usize);
        let usable = usable & !7;
        let mut tlab = unsafe { Tlab::new(aligned, usable) };
        // Use part of the TLAB so a meaningful tail remains.
        let _ = tlab.alloc(64, 8).unwrap();
        let tail_before = tlab.remaining();
        assert!(tail_before > HEADER_SIZE);

        let cursor_before = tlab.cursor;
        unsafe {
            tlab.install_tail_filler(TLAB_FILLER_CLASS_ID);
        }
        // After install, cursor is at end (TLAB consumed).
        assert_eq!(tlab.remaining(), 0);

        // The synthetic header at the old cursor describes an int[] that
        // covers the remaining tail exactly.
        let hdr = unsafe { &*(cursor_before as *const ObjectHeader) };
        assert_eq!(hdr.kind(), ObjectKind::Array);
        assert_eq!(hdr.element_type(), ArrayElementType::Int);
        assert_eq!(hdr.class_id, TLAB_FILLER_CLASS_ID);
        let total = HEADER_SIZE + (hdr.array_length() as usize) * 4;
        // total should equal tail_before (no padding waste because both ends 8-aligned).
        assert_eq!(total, tail_before);
    }

    #[test]
    fn tlab_filler_handles_tiny_tail() {
        // Fewer than HEADER_SIZE bytes left → filler routine zeroes the
        // tail and bumps the cursor without writing a header.
        let mut buf = vec![0u8; 64];
        let raw = buf.as_mut_ptr();
        let aligned = ((raw as usize + 7) & !7) as *mut u8;
        let usable = 64 - (aligned as usize - raw as usize);
        let usable = usable & !7;
        let mut tlab = unsafe { Tlab::new(aligned, usable) };
        // Allocate enough that less than HEADER_SIZE remains.
        let alloc_size = usable.saturating_sub(8);
        let _ = tlab.alloc(alloc_size, 8).unwrap();
        assert!(tlab.remaining() < crate::heap::HEADER_SIZE);
        unsafe {
            tlab.install_tail_filler(TLAB_FILLER_CLASS_ID);
        }
        assert_eq!(tlab.remaining(), 0);
    }

    /// gengc-round1-alloc, 2026-09-20 — the GAP sentinel's span is left
    /// wholly zero behind it, exactly as the `int[]` arm leaves its data area.
    ///
    /// The sentinel itself is two `u32`s in the first 8 bytes; any remainder
    /// behind it used to keep whatever the span held before. That asymmetry
    /// matters for a reader that is NOT a walker recognising the sentinel —
    /// `is_object_address` DEDUCES its answer from the bytes at the address,
    /// and stale bytes are what its own comment names as the false-positive
    /// source it cannot eliminate.
    /// ORCHESTRATOR CORRECTION, 2026-09-20 — the round-1 version of this test
    /// asserted something the code does not do, and its premise was arithmetic.
    ///
    /// It left a **24**-byte tail and called that "below `HEADER_SIZE`, so the
    /// sentinel arm runs". `HEADER_SIZE` is 16 (`types/src/heap_types.rs:19`),
    /// so 24 takes the `int[]` arm instead, which stamps the `class_id`
    /// ARGUMENT — `TLAB_FILLER_CLASS_ID` (0xF111_E700) — and the test then
    /// asserted it equalled `GAP_FILLER_CLASS_ID` (0xF111_E701). Off by one,
    /// in the constant rather than the index.
    ///
    /// Correcting the arithmetic also retires the fix it was written for. The
    /// sentinel arm is guarded by `tail < HEADER_SIZE` and asserts
    /// `tail >= 8 && tail % 8 == 0`, so while `end` is 8-aligned — which it is
    /// on every production path — **`tail` is exactly 8**, the sentinel is
    /// exactly those 8 bytes, and there is no remainder behind it to go stale.
    /// The `int[]` arm (`tail >= HEADER_SIZE`) already zeroes its own data
    /// area. So the asymmetry the round-1 fix targeted is unreachable, and the
    /// `if tail > 8` zeroing it added survives only as defence for a
    /// non-8-aligned `end`; it is deliberately NOT exercised here, because a
    /// test that forced `tail = 12` would trip that arm's own `debug_assert`
    /// under `cargo test` without `--release`.
    ///
    /// What is worth fencing is the real contract: the sentinel arm stamps
    /// `GAP_FILLER_CLASS_ID` (not the caller's id), records the exact length,
    /// and consumes the tail.
    #[test]
    fn the_gap_filler_sentinel_arm_stamps_an_exact_eight_byte_tail() {
        let mut buf = vec![0u8; 128];
        let raw = buf.as_mut_ptr();
        let aligned = ((raw as usize + 7) & !7) as *mut u8;
        let usable = (128 - (aligned as usize - raw as usize)) & !7;
        let mut tlab = unsafe { Tlab::new(aligned, usable) };
        // Exactly 8 bytes left: the only tail the sentinel arm can ever see,
        // given `tail < HEADER_SIZE` and the arm's own 8-multiple assertion.
        let head = usable - 8;
        let p = tlab.alloc(head, 8).expect("head must fit");
        assert_eq!(tlab.remaining(), 8);
        let gap = p as usize + head;

        unsafe { tlab.install_tail_filler(TLAB_FILLER_CLASS_ID) };
        assert_eq!(tlab.remaining(), 0, "the filler must consume the tail");

        // SAFETY: `[gap, gap + 8)` is this buffer's own (still-owned) tail.
        let (cid, len) = unsafe {
            let q = gap as *const u32;
            (std::ptr::read(q), std::ptr::read(q.add(1)))
        };
        assert_eq!(
            cid,
            GAP_FILLER_CLASS_ID.as_u32(),
            "the sentinel arm stamps GAP_FILLER, not the caller's class id"
        );
        assert_eq!(len, 8, "the sentinel records the exact gap length");
    }

    /// gengc-round1-alloc, 2026-09-20 — the TLAB waste census moves, and the
    /// `int[]` filler's bytes land in the filler bucket.
    ///
    /// Asserted as LOWER BOUNDS on purpose: these are process-wide counters
    /// and every other test in this binary retires TLABs into them
    /// concurrently, so only monotone growth is a stable claim (the reason the
    /// allocation-total tests assert a per-buffer mark or a local counter).
    #[test]
    fn the_waste_census_counts_a_retire_and_its_filler_tail() {
        let before = tlab_waste_stats();
        let mut buf = vec![0u8; 4096];
        let raw = buf.as_mut_ptr();
        let aligned = ((raw as usize + 7) & !7) as *mut u8;
        let usable = (4096 - (aligned as usize - raw as usize)) & !7;
        let mut tlab = unsafe { Tlab::new(aligned, usable) };
        tlab.alloc(64, 8).expect("room");
        let tail = tlab.remaining();
        assert!(tail >= crate::heap::HEADER_SIZE);
        tlab.retire();

        let after = tlab_waste_stats();
        assert!(after.retires >= before.retires + 1);
        assert!(after.carved_bytes >= before.carved_bytes + usable as u64);
        assert!(after.consumed_bytes >= before.consumed_bytes + 64);
        // Filler OR sink: a `TlabTailSink` registered by some other test in
        // this binary declines a span outside its own arena, so the filler arm
        // is what runs here — but asserting the sum keeps the claim true
        // either way rather than depending on which tests ran first.
        assert!(
            after.filler_tail_bytes + after.sink_tail_bytes
                >= before.filler_tail_bytes + before.sink_tail_bytes + tail as u64,
            "the tail has to be accounted for in exactly one of the two buckets",
        );

        // A second retire is a no-op: the census is gated on a live `start`,
        // and `finish_retire` has already nulled it. The COUNT cannot be
        // asserted here — these are process-wide counters and the rest of the
        // binary is retiring into them concurrently — so the gate's own
        // precondition is what this pins.
        tlab.retire();
        assert!(
            tlab.is_retired(),
            "a second retire must leave the buffer retired and count nothing",
        );
    }

    /// The derived figures are pure arithmetic over the census, and are what a
    /// reader actually wants ("what fraction of what the arena gave up did the
    /// program ask for"). Kept as a separate, race-free test.
    #[test]
    fn waste_percentages_are_derived_without_dividing_by_zero() {
        let empty = TlabWasteStats::default();
        assert_eq!(empty.unused_bytes(), 0);
        assert_eq!(empty.unused_percent(), 0.0);

        let s = TlabWasteStats {
            retires: 4,
            carved_bytes: 1000,
            consumed_bytes: 750,
            filler_tail_bytes: 240,
            gap_tail_bytes: 10,
            sink_tail_bytes: 0,
            wasteful_retires: 1,
        };
        assert_eq!(s.unused_bytes(), 250);
        assert!((s.unused_percent() - 25.0).abs() < 1e-9);

        // Consumed can never exceed carved (`finish_retire` clamps), so the
        // saturating subtraction is a belt on a brace rather than a branch
        // anyone reaches.
        let odd = TlabWasteStats {
            carved_bytes: 10,
            consumed_bytes: 20,
            ..TlabWasteStats::default()
        };
        assert_eq!(odd.unused_bytes(), 0);
    }

    /// gengc-round2-alloc2, 2026-09-20 — a retire that gave up most of its
    /// buffer is counted as such, and a drained one is not.
    ///
    /// This is the shape number the aggregate cannot give: `carved - consumed`
    /// is identical for "a thin tail on every retire" and "a fat one on a few",
    /// and only the second is what
    /// `gengc-alloc-tlab-sizer-is-blind-to-waste-DONE-20260929.md` proposes to size
    /// against. Both directions are asserted — a counter that fires on
    /// everything would be as useless as one that fires on nothing.
    ///
    /// The threshold is asserted on [`retire_is_wasteful`] and not on the
    /// counter, because the counter is process-wide and every other test in
    /// this binary retires into it in parallel; only the ONE-WAY claim (an
    /// early retire moves it) is safe to make against the static.
    #[test]
    fn the_census_tells_an_early_retire_apart_from_a_drained_one() {
        // The threshold, race-free. Half, not the sizer's 75%: a drain cannot
        // reach half because `TLAB_MAX_ALLOC` (32 KiB) bounds the object that
        // ends a buffer's life well below half of the 256 KiB baseline.
        assert!(retire_is_wasteful(256 * 1024, 64), "the park-heavy shape");
        assert!(retire_is_wasteful(1000, 500), "exactly half is wasteful");
        assert!(!retire_is_wasteful(1000, 501));
        assert!(!retire_is_wasteful(256 * 1024, 256 * 1024 - 32 * 1024));
        assert!(
            !retire_is_wasteful(0, 0),
            "a buffer with no chunk is not a retire"
        );
        assert!(
            !retire_is_wasteful(1000, 5000),
            "consumed is clamped to carved before the compare, so an \
             over-reported span cannot read as a drain of something else",
        );

        // ...and a genuinely early retire does move the counter. Asserted one
        // way (`>=`) for the reason in the doc comment above. gce e1/o: the
        // buffer is 80 KiB so its tail clears `TLAB_MAX_ALLOC`, the census's
        // drain discriminator (a 4 KiB buffer no longer counts).
        let before = tlab_waste_stats();
        let len = 80 * 1024;
        let mut buf = vec![0u8; len];
        let raw = buf.as_mut_ptr();
        let aligned = ((raw as usize + 7) & !7) as *mut u8;
        let usable = (len - (aligned as usize - raw as usize)) & !7;
        let mut tlab = unsafe { Tlab::new(aligned, usable) };
        tlab.alloc(64, 8).expect("room");
        tlab.retire();
        let after = tlab_waste_stats();
        assert!(
            after.wasteful_retires >= before.wasteful_retires + 1,
            "64 bytes out of {usable} is an early retire, not a drain",
        );
    }

    /// gce e1/o — `gcd-d5q-wasteful-retire-census-counts-small-drains-FIXED-20260929.md`.
    /// A drain's tail is below the object that missed (at most
    /// `TLAB_MAX_ALLOC`), so on a buffer under 64 KiB a drain can leave half;
    /// the census must not count it. An early retire of a big buffer still
    /// counts.
    #[test]
    fn gce_e1o_a_small_drain_is_not_a_wasteful_retire() {
        // 8 KiB buffer, drained by a 5 KiB miss with a 4.5 KiB tail.
        let small = 8 * 1024u64;
        let consumed = small - 4608;
        assert!(retire_is_wasteful(small, consumed), "half or more is left");
        assert!(
            !retire_counts_as_wasteful(small, consumed),
            "a tail under TLAB_MAX_ALLOC is a drain's, not an early retire's",
        );
        // 256 KiB buffer retired after 20 KiB: early, counted.
        assert!(retire_counts_as_wasteful(256 * 1024, 20 * 1024));
        // A 64 KiB buffer left exactly half is counted.
        assert!(retire_counts_as_wasteful(64 * 1024, 32 * 1024));
        // The drain's worst case: the tail one word below TLAB_MAX_ALLOC.
        let tail = TLAB_MAX_ALLOC as u64 - 8;
        assert!(!retire_counts_as_wasteful(48 * 1024, 48 * 1024 - tail));
        assert!(!retire_counts_as_wasteful(0, 0));
    }

    /// gengc-round2-alloc2, 2026-09-20 — the reporting surface exists and says
    /// something.
    ///
    /// `gengc-alloc-tlab-instruments-are-write-only-FIXED-20260924.md`'s verification
    /// step 1 is `rg 'tlab_tripwire_counts|tlab_waste_stats'` returning CALL
    /// SITES rather than definitions. This is one of them, and the only one
    /// this reviewer can add: printing belongs to `VmHeap::print_gc_summary`,
    /// which is another agent's file. What is pinned here is that the lines
    /// carry the field names a log grep would be written against — renaming
    /// one silently is the way a shutdown line stops being greppable.
    #[test]
    fn the_census_lines_carry_the_field_names_a_log_grep_needs() {
        let [waste, guard] = tlab_census_lines();
        for key in [
            "[GC] tlab-waste:",
            "retires=",
            "wasteful_retires=",
            "carved=",
            "consumed=",
            "unused=",
            "filler_tail=",
            "gap_tail=",
            "sink_tail=",
        ] {
            assert!(waste.contains(key), "{key:?} missing from {waste:?}");
        }
        for key in [
            "[GC] tlab-guard:",
            "filler_over_object=",
            "refill_over_object=",
        ] {
            assert!(guard.contains(key), "{key:?} missing from {guard:?}");
        }
        // The urgency predicate must agree with the counters it summarises,
        // whatever the rest of this binary has done to them.
        let (f, r) = tlab_tripwire_counts();
        assert_eq!(tlab_guard_line_is_urgent(), f != 0 || r != 0);
    }

    #[test]
    fn tlab_filler_noop_on_empty_tlab() {
        let mut tlab = Tlab::empty();
        // Must not panic on null cursor/end.
        unsafe {
            tlab.install_tail_filler(TLAB_FILLER_CLASS_ID);
        }
        assert!(tlab.is_empty());
    }

    // ---------------------------------------------------------------
    // JIT-blind sizing (perf/gc-allocation-fastpath, 2026-07-26)
    // ---------------------------------------------------------------

    /// `consumed_bytes` is the ONLY signal that sees the JIT's inline bump,
    /// so it must track the raw cursor span — including a bump this file
    /// never performed. Simulate the compiled fast path exactly: read the
    /// cursor, add the object size, store it back.
    #[test]
    fn consumed_bytes_sees_a_raw_cursor_bump() {
        let mut buf = vec![0u8; 4096];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 4096) };
        assert_eq!(tlab.consumed_bytes(), 0);

        tlab.alloc(64, 8).unwrap();
        assert_eq!(tlab.consumed_bytes(), 64);

        // The JIT's `emit_inline_tlab_new`: bump `cursor` in place, touching
        // nothing else on the struct.
        let bumped = unsafe { tlab.cursor.add(128) };
        tlab.cursor = bumped;
        assert_eq!(tlab.consumed_bytes(), 64 + 128);
        // The tracker itself saw only the one Rust-side allocation.
        assert_eq!(tlab.pressure_tracker().alloc_count, 1);

        // An empty TLAB contributes nothing (no null-pointer arithmetic).
        let empty = Tlab::empty();
        assert_eq!(empty.consumed_bytes(), 0);
    }

    /// The regression this fix exists for: a thread allocating entirely
    /// through compiled code reports `alloc_count == 0`, which the raw
    /// heuristic read as "idle" and halved the TLAB for — every refill,
    /// forever, down to the floor and never back up. With the consumption
    /// signal the drained TLAB is recognised as pressure instead.
    #[test]
    fn jit_drained_tlab_does_not_ratchet_down() {
        let size = 256 * 1024;
        let mut t = TlabPressureTracker::new();
        t.begin_refill(size);
        // Slow enough that the sub-millisecond "fast fill" arm cannot rescue
        // it — this is the case that used to shrink.
        std::thread::sleep(std::time::Duration::from_millis(2));

        // Without the consumption signal: read as idle, halved.
        assert_eq!(t.next_refill_size_with_consumed(0), size / 2);
        // With it: the TLAB was drained, so it is not idle.
        assert!(
            t.next_refill_size_with_consumed(size) >= size,
            "a drained TLAB must never shrink"
        );
    }

    /// The ratchet was one-way — once at the floor, the shrink condition
    /// stayed true regardless of allocation rate, so the thread was stuck
    /// with 8 KiB TLABs for the rest of the process. Walking the sizer the
    /// way the refill path does must climb back out.
    #[test]
    fn jit_drained_tlab_climbs_back_from_the_floor() {
        let mut t = TlabPressureTracker::new();
        let mut size = MIN_TLAB_SIZE;
        t.begin_refill(size);
        for _ in 0..16 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            // Whole TLAB consumed by compiled code: zero tracked allocations.
            let next = t.next_refill_size_with_consumed(size);
            assert!(next >= size, "sizer went backwards: {next} < {size}");
            if next == size {
                break;
            }
            size = next;
            t.begin_refill(size);
            if size >= DEFAULT_TLAB_SIZE {
                break;
            }
        }
        assert!(
            size >= DEFAULT_TLAB_SIZE,
            "JIT-drained thread stuck at {size} bytes"
        );
    }

    /// A TLAB that was barely touched must still shrink — the consumption
    /// signal must not blanket-disable the idle path (one sleeping thread per
    /// core holding a 1 MiB chunk is what the shrink arm exists to prevent).
    #[test]
    fn barely_used_tlab_still_shrinks_with_consumption_signal() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(MAX_TLAB_SIZE);
        std::thread::sleep(std::time::Duration::from_millis(2));
        // 4 KiB out of 1 MiB consumed — nowhere near drained.
        let next = t.next_refill_size_with_consumed(4096);
        assert!(
            next < MAX_TLAB_SIZE,
            "an idle thread must give its TLAB back, got {next}"
        );
        assert!(next >= MIN_TLAB_SIZE);
    }

    /// The whole-TLAB path: `Tlab::next_refill_size` must feed its own live
    /// cursor span in, so callers get the fix without threading anything.
    #[test]
    fn tlab_next_refill_size_feeds_its_own_consumption() {
        let bytes = 64 * 1024;
        let mut buf = vec![0u8; bytes];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), bytes) };
        tlab.begin_refill(bytes);
        // Drain it the way compiled code does: raw cursor bump only.
        tlab.cursor = unsafe { tlab.start.add(bytes) };
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(tlab.consumed_bytes(), bytes);
        assert!(
            tlab.next_refill_size() >= bytes,
            "a JIT-drained TLAB must not shrink through the Tlab wrapper"
        );
    }

    // ---------------------------------------------------------------
    // TLAB audit (tlab-and-card-audit.md) — retire / publish
    // ---------------------------------------------------------------

    /// Helper: an 8-aligned span of exactly `bytes` usable bytes, plus the
    /// `Vec` that owns the backing allocation (which the caller must keep
    /// alive — moving the `Vec` does not move its heap buffer, so the returned
    /// pointer stays valid).
    ///
    /// `Tlab::new` requires an 8-aligned `end`, so `bytes` must be a multiple
    /// of 8 and the base is rounded up inside a buffer with 8 bytes of slack.
    fn aligned_buffer(bytes: usize) -> (Vec<u8>, *mut u8, usize) {
        assert_eq!(bytes % 8, 0, "TLAB spans are 8-aligned at both ends");
        let mut buf = vec![0u8; bytes + 8];
        let raw = buf.as_mut_ptr();
        let base = ((raw as usize + 7) & !7) as *mut u8;
        (buf, base, bytes)
    }

    /// `retire` is idempotent, and every repeat leaves the same observable
    /// state. The transition graph really does retire twice with no refill in
    /// between: a thread parks at a safepoint (`safepoint_check` retires), is
    /// then chosen as the next GC initiator (`maybe_gc` retires) and finally
    /// terminates (`thread_start` retires). If the second retire re-ran the
    /// tail filler over a now-null cursor, or resurrected a publishable tail,
    /// the STW census's "a parked peer publishes no skip region" assumption
    /// would be false.
    #[test]
    fn retire_is_idempotent_and_publishes_no_tail() {
        let (_owner, base, usable) = aligned_buffer(4096);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(64, 8).unwrap();
        assert!(tlab.reserved_tail().is_some(), "a live TLAB has a tail");

        for round in 0..3 {
            tlab.retire();
            assert!(tlab.is_retired(), "retire round {round}");
            assert!(
                tlab.reserved_tail().is_none(),
                "a retired TLAB must publish no skip region (round {round})",
            );
            assert!(tlab.is_empty());
            assert_eq!(tlab.remaining(), 0);
            assert_eq!(tlab.consumed_bytes(), 0);
            assert!(tlab.alloc(1, 8).is_none(), "round {round}");
        }
    }

    /// Retiring an EMPTY TLAB — the state a thread is in between its park and
    /// its next allocation — must also be a no-op rather than a null
    /// dereference inside the tail filler. Every abrupt-transition path
    /// (`begin_blocking_region`, thread termination, the JIT allocation
    /// helpers) can reach a TLAB that a previous transition already retired.
    #[test]
    fn retire_on_an_already_empty_tlab_is_a_noop() {
        let mut tlab = Tlab::empty();
        assert!(tlab.is_retired());
        tlab.retire();
        tlab.retire();
        assert!(tlab.is_retired());
        assert!(tlab.reserved_tail().is_none());
    }

    /// The abrupt-transition case the whole retire protocol exists for: a
    /// thread is bump-allocating (including through the JIT's inline path,
    /// which moves `cursor` without telling this file) and is then forced
    /// through a transition — blocking native, termination, forced GC — at an
    /// arbitrary point. After the transition's `retire` there must be no
    /// unretired tail at ANY of those points, and the bytes between the last
    /// object and the TLAB end must be covered by a filler the heap walker can
    /// stride in O(1).
    #[test]
    fn abrupt_transition_at_any_cursor_leaves_no_unretired_tail() {
        use crate::heap::{ObjectHeader, ObjectKind, HEADER_SIZE};

        // Sweep the cursor across the whole TLAB in 8-byte steps, including the
        // three interesting boundaries: a full-size `int[]` filler still fits,
        // exactly `HEADER_SIZE` remains, and a sub-header gap remains.
        let size = 512;
        for consumed in (0..=size).step_by(8) {
            let (_owner, base, usable) = aligned_buffer(size);
            let mut tlab = unsafe { Tlab::new(base, usable) };
            // Simulate the JIT's inline bump: move the cursor directly, exactly
            // as `emit_inline_tlab_new` does, so nothing in this file has seen
            // the allocations.
            if consumed > 0 {
                tlab.alloc(consumed, 8).expect("cursor sweep fits");
            }
            let tail_before = tlab.remaining();

            tlab.retire();

            assert!(
                tlab.reserved_tail().is_none(),
                "consumed={consumed}: an abruptly-transitioned thread must leave \
                 no reserved tail for the collector to trip over",
            );
            assert!(tlab.is_retired(), "consumed={consumed}");

            // And the tail is walkable: either a full `int[]` filler covering
            // exactly the remaining bytes, or the sub-header GAP sentinel.
            if tail_before >= HEADER_SIZE {
                // SAFETY: `base + consumed` is inside the (still-owned) buffer,
                // 8-aligned, and holds the filler header retire just wrote.
                let hdr = unsafe { &*((base as usize + consumed) as *const ObjectHeader) };
                assert_eq!(hdr.class_id, TLAB_FILLER_CLASS_ID, "consumed={consumed}");
                assert_eq!(hdr.kind(), ObjectKind::Array, "consumed={consumed}");
                assert_eq!(
                    HEADER_SIZE + (hdr.array_length() as usize) * 4,
                    tail_before,
                    "consumed={consumed}: the filler must cover the tail EXACTLY, \
                     or the walker resumes off the object grid",
                );
            } else if tail_before > 0 {
                // SAFETY: as above; the gap sentinel is two `u32`s.
                let (cid, len) = unsafe {
                    let p = (base as usize + consumed) as *const u32;
                    (std::ptr::read(p), std::ptr::read(p.add(1)))
                };
                assert_eq!(cid, GAP_FILLER_CLASS_ID.as_u32(), "consumed={consumed}");
                assert_eq!(len as usize, tail_before, "consumed={consumed}");
            }
        }
    }

    /// `reserved_tail` is what the cross-thread STW scan publishes as a skip
    /// region, and `GenerationalHeap::jit_tlab_skip_offsets` silently DROPS a
    /// region whose start is not 8-aligned — after which the sweep walks the
    /// un-retired tail as objects. Production allocates with `align >= 8`, so
    /// the invariant holds; it held only by convention until this assertion.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "not 8-aligned")]
    fn reserved_tail_rejects_a_misaligned_cursor_in_debug_builds() {
        let (_owner, base, usable) = aligned_buffer(256);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        // `align = 1` is the only way to get here, and no production path uses
        // it — this is the tripwire firing, which is the point.
        tlab.alloc(3, 1).unwrap();
        let _ = tlab.reserved_tail();
    }

    /// The published tail is exactly `[cursor, end)` for an aligned cursor —
    /// the collector must skip the reserved bytes and *only* the reserved
    /// bytes, or it either walks into raw TLAB memory (too small) or hides live
    /// objects from the sweep (too large).
    #[test]
    fn reserved_tail_is_exactly_the_unallocated_span() {
        let (_owner, base, usable) = aligned_buffer(1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(128, 8).unwrap();
        assert_eq!(
            tlab.reserved_tail(),
            Some((base as usize + 128, base as usize + usable)),
        );

        // Consuming the TLAB exactly to its end publishes nothing.
        tlab.alloc(usable - 128, 8).unwrap();
        assert_eq!(tlab.remaining(), 0);
        assert!(tlab.reserved_tail().is_none());
    }

    /// Installing the tail filler must be TOTAL: every exit path leaves
    /// `cursor == end`, so `reserved_tail()` is `None` afterwards. The
    /// sub-8-byte-slack path used to return with `cursor < end` still true,
    /// which left a publishable span the filler had not covered.
    ///
    /// **The `#[test]` for this used to sit HERE, above the doc comment of the
    /// test below.** An insertion between an attribute and its `fn` moved the
    /// attribute onto the wrong item: `retire_charges_...` was registered twice
    /// and this test was registered NOT AT ALL. The only trace was a
    /// `duplicate_macro_attributes` warning. See the crate-level
    /// `deny(duplicate_macro_attributes)` in `lib.rs`, which now makes that a
    /// build failure -- the warning was there and was read past.
    /// `retire` must charge the thread for what it CONSUMED, not for the whole
    /// chunk. `install_tail_filler` sets `cursor = end`, so a `consumed_bytes()`
    /// read taken after it returns the TLAB size — which made
    /// `ThreadMXBean.getThreadAllocatedBytes` a function of the collector's TLAB
    /// sizing rather than of the program: the same Hibernate HQL parse reported
    /// 488 MB under ZGC and 49 MB under Generational.
    #[test]
    fn retire_charges_the_thread_for_consumed_bytes_not_the_whole_chunk() {
        for (chunk, consumed) in [(64usize, 8usize), (4096, 64), (65536, 1024)] {
            let (_owner, base, usable) = aligned_buffer(chunk);
            let mut tlab = unsafe { Tlab::new(base, usable) };
            tlab.alloc(consumed, 8).unwrap();
            let before = tlab.thread_allocated_bytes();
            assert_eq!(before, consumed as u64, "live span, chunk={chunk}");
            tlab.retire();
            assert_eq!(
                tlab.thread_allocated_bytes(),
                consumed as u64,
                "retire must not add the unused tail (chunk={chunk}, usable={usable})"
            );
        }
    }

    /// A second `retire` still adds nothing, now that the span is read before
    /// the filler rather than after it.
    #[test]
    fn retire_remains_idempotent_for_the_allocation_counter() {
        let (_owner, base, usable) = aligned_buffer(4096);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(128, 8).unwrap();
        tlab.retire();
        let once = tlab.thread_allocated_bytes();
        tlab.retire();
        assert_eq!(tlab.thread_allocated_bytes(), once);
        assert_eq!(once, 128);
    }

    #[test]
    fn install_tail_filler_always_consumes_the_tlab() {
        for consumed in [0usize, 8, 40, 56, 64] {
            let (_owner, base, usable) = aligned_buffer(64);
            let mut tlab = unsafe { Tlab::new(base, usable) };
            if consumed > 0 {
                tlab.alloc(consumed, 8).unwrap();
            }
            unsafe {
                tlab.install_tail_filler(TLAB_FILLER_CLASS_ID);
            }
            assert_eq!(tlab.remaining(), 0, "consumed={consumed}");
            assert!(tlab.reserved_tail().is_none(), "consumed={consumed}");
        }
    }

    /// The adaptive sizer must be consulted BEFORE the TLAB is retired: after
    /// `retire` the `cursor - start` span it reads is zero, which is exactly
    /// the input that reintroduces the JIT-blind one-way shrink ratchet. The
    /// refill path honours the ordering; nothing detected a violation until
    /// this tripwire.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "RETIRED TLAB")]
    fn sizing_a_retired_tlab_trips_the_ordering_assertion() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(1024, 8).unwrap();
        tlab.retire();
        let _ = tlab.next_refill_size();
    }

    /// ...and the legitimate ordering (size, then retire, then install a fresh
    /// TLAB) must not trip it, on either a fresh or a recycled TLAB.
    #[test]
    fn sizing_before_retiring_is_accepted() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        // Drain it the way compiled code does — raw cursor bump only.
        tlab.cursor = unsafe { tlab.start.add(usable) };
        let requested = tlab.next_refill_size();
        assert!(requested >= MIN_TLAB_SIZE);
        tlab.retire();

        // The replacement is a brand-new `Tlab`, whose tracker starts a fresh
        // refill window — so the tripwire is disarmed again.
        let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
        let mut next = unsafe { Tlab::new(base2, usable2) };
        next.begin_refill(usable2);
        next.alloc(64, 8).unwrap();
        let _ = next.next_refill_size();
    }

    /// A heap-internal staging TLAB must not publish to an allocation total:
    /// the bytes it hands out are counted a second time by the Java thread's
    /// own TLAB (or by `note_external_allocation`) one layer up. Publishing
    /// from both is why `getTotalThreadAllocatedBytes` read 3.00x of retained
    /// heap under ZGC against 2.00x under Gen/G1.
    #[test]
    fn heap_staging_tlab_does_not_publish_to_the_allocation_total() {
        let total = AtomicU64::new(0);
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut staging = unsafe { Tlab::new_heap_staging(base, usable) };
        assert_eq!(staging.accounting(), TlabAccounting::HeapStaging);
        staging.attach_vm_thread_allocation_total(&total, 0);
        staging.alloc(4096, 8).unwrap();
        staging.note_external_allocation(1024);
        staging.retire();
        assert_eq!(
            staging.vm_thread_published_bytes(),
            0,
            "a heap-internal staging buffer published to an allocation total; its span is re-counted one layer up, so this double-counts"
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
        // Its own per-buffer total still moves: the ZGC TLAB statistics read it.
        assert_eq!(staging.thread_allocated_bytes(), 4096 + 1024);
    }

    /// The Java-thread arm of the same contract, and the size of the
    /// publication: exactly the settled bytes, once. A sub-batch external note
    /// is held back (the reader adds it through
    /// `vm_thread_unpublished_bytes`); the retire publishes everything.
    #[test]
    fn java_thread_tlab_publishes_settled_bytes_exactly_once() {
        let total = AtomicU64::new(0);
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        assert_eq!(tlab.accounting(), TlabAccounting::JavaThread);
        tlab.attach_vm_thread_allocation_total(&total, 0);
        tlab.alloc(4096, 8).unwrap();
        tlab.note_external_allocation(1024);
        assert_eq!(
            tlab.vm_thread_published_bytes(),
            0,
            "a sub-batch external note stays thread-local; the LIVE TLAB span waits for retire, because the reader adds it as the live-span term"
        );
        assert_eq!(
            total.load(Ordering::Relaxed) + tlab.vm_thread_unpublished_bytes(),
            4096 + 1024,
            "the reader's sum is exact for the calling thread"
        );
        tlab.retire();
        assert_eq!(tlab.vm_thread_published_bytes(), 4096 + 1024);
        assert_eq!(total.load(Ordering::Relaxed), 4096 + 1024);
        // Idempotent: a second retire settles nothing, so it publishes nothing.
        tlab.retire();
        assert_eq!(tlab.vm_thread_published_bytes(), 4096 + 1024);
        assert_eq!(total.load(Ordering::Relaxed), 4096 + 1024);
    }

    /// The native path notes every object that misses the TLAB. Those notes
    /// reach the shared total once per [`EXTERNAL_PUBLISH_BATCH_BYTES`], not
    /// once per object (the w2-d addendum on
    /// `common-f-process-global-allocation-state`).
    #[test]
    fn external_notes_are_published_in_batches() {
        let total = AtomicU64::new(0);
        let mut tlab = Tlab::empty();
        tlab.attach_vm_thread_allocation_total(&total, 0);
        let per_object: u64 = 24;
        let below = EXTERNAL_PUBLISH_BATCH_BYTES / per_object;
        for _ in 0..below {
            tlab.note_external_allocation(per_object as usize);
        }
        assert_eq!(
            total.load(Ordering::Relaxed),
            0,
            "a sub-batch of notes must not touch the shared total"
        );
        tlab.note_external_allocation(per_object as usize);
        assert_eq!(
            total.load(Ordering::Relaxed),
            (below + 1) * per_object,
            "crossing the batch publishes the whole pending carry at once"
        );
        tlab.note_external_allocation(per_object as usize);
        tlab.retire();
        assert_eq!(
            total.load(Ordering::Relaxed),
            (below + 2) * per_object,
            "a retire publishes the sub-batch residue"
        );
        assert_eq!(tlab.vm_thread_unpublished_bytes(), 0);
    }

    /// A thread that allocates only outside its TLAB still reaches its VM's
    /// total: the VM's non-TLAB sites attach through
    /// `ensure_vm_thread_allocation_total`, which keeps the buffer's mark and
    /// is idempotent.
    #[test]
    fn ensure_attach_credits_pending_bytes_once_and_is_idempotent() {
        let total = AtomicU64::new(0);
        let other = AtomicU64::new(0);
        let mut tlab = Tlab::empty();
        tlab.note_external_allocation(1024);
        tlab.ensure_vm_thread_allocation_total(&total);
        assert_eq!(total.load(Ordering::Relaxed), 1024);
        tlab.ensure_vm_thread_allocation_total(&other);
        tlab.note_external_allocation(EXTERNAL_PUBLISH_BATCH_BYTES as usize);
        assert_eq!(
            total.load(Ordering::Relaxed),
            1024 + EXTERNAL_PUBLISH_BATCH_BYTES,
            "a second ensure must neither re-credit nor re-point the buffer"
        );
        assert_eq!(other.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn vm_tlab_observations_are_batched_and_the_residual_is_drained() {
        let mut tlab = Tlab::empty();
        let total = AtomicU64::new(0);
        tlab.attach_vm_allocation_counter(&total);
        assert_eq!(
            tlab.note_vm_tlab_allocation((VM_ALLOCATION_BATCH_BYTES - 8) as usize),
            None,
            "a sub-batch allocation must stay thread-local"
        );
        assert_eq!(
            tlab.note_vm_tlab_allocation(8),
            Some(TlabAllocationBatch {
                bytes: VM_ALLOCATION_BATCH_BYTES
            }),
            "crossing the byte threshold publishes exactly one bounded batch"
        );
        assert_eq!(
            total.load(Ordering::Relaxed),
            VM_ALLOCATION_BATCH_BYTES,
            "the filled batch must reach its attached VM-local counter"
        );
        assert_eq!(
            tlab.take_vm_allocation_batch(),
            TlabAllocationBatch::default()
        );

        assert_eq!(tlab.note_vm_tlab_allocation(24), None);
        tlab.retire();
        assert_eq!(
            tlab.take_vm_allocation_batch(),
            TlabAllocationBatch::default(),
            "retiring for a refill/GC must consume the final sub-batch"
        );
        assert_eq!(
            total.load(Ordering::Relaxed),
            VM_ALLOCATION_BATCH_BYTES + 24,
            "retiring for a refill/GC must publish the final sub-batch exactly once"
        );
    }

    /// A refill must not re-publish the thread's history. `Tlab::new` starts a
    /// fresh buffer, so without carrying the high-water mark across, the
    /// successor's first retire would send the WHOLE running total again --
    /// once per refill, which on a parse that refills thousands of times is
    /// not a 2x error but an unbounded one.
    #[test]
    fn refill_carries_the_published_mark_so_history_is_not_republished() {
        let total = AtomicU64::new(0);
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.attach_vm_thread_allocation_total(&total, 0);
        tlab.alloc(4096, 8).unwrap();
        let carried = {
            tlab.retire();
            tlab.thread_allocated_bytes()
        };
        assert_eq!(carried, 4096);
        let mark = tlab.vm_thread_published_bytes();
        assert_eq!(mark, carried, "an attached retire publishes everything");

        let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
        let mut next = unsafe { Tlab::new(base2, usable2) };
        next.adopt_allocation_total(carried);
        assert_eq!(
            next.vm_thread_published_bytes(),
            carried,
            "the successor must adopt the mark, not just the total"
        );
        next.attach_vm_thread_allocation_total(&total, mark);
        assert_eq!(total.load(Ordering::Relaxed), 4096);
        next.alloc(2048, 8).unwrap();
        next.retire();
        assert_eq!(next.thread_allocated_bytes(), 4096 + 2048);
        assert_eq!(
            next.vm_thread_published_bytes() - carried,
            2048,
            "the successor published only its OWN bytes"
        );
        assert_eq!(total.load(Ordering::Relaxed), 4096 + 2048);
    }

    /// gc-common w6-c: the per-VM total (`HeapRealm::thread_allocated_total`)
    /// that replaced the process static `PROCESS_ALLOCATED_BYTES` as the
    /// `getTotalThreadAllocatedBytes` source. Walks a JvmThread's life the way
    /// the one refill site in `gc_and_alloc.rs` drives it:
    /// * the first buffer is an unattached `Tlab::empty()`, and an external
    ///   allocation that did not attach (a JIT helper's) lands on it before
    ///   any refill;
    /// * the first refill attaches, and must credit exactly those bytes;
    /// * later refills attach again with the outgoing mark and credit nothing
    ///   twice;
    /// * a second VM's counter never sees this thread's bytes.
    #[test]
    fn vm_thread_total_credits_pre_attach_bytes_once_and_is_per_vm() {
        let vm_a = AtomicU64::new(0);
        let vm_b = AtomicU64::new(0);

        // JvmThread::new: unattached empty buffer; a native allocates 1 KiB.
        let mut tlab = Tlab::empty();
        tlab.note_external_allocation(1024);
        assert_eq!(tlab.vm_thread_published_bytes(), 0);
        assert_eq!(
            tlab.vm_thread_unpublished_bytes(),
            1024,
            "the reader must still see the calling thread's pre-attach bytes"
        );

        // First refill: retire, read carry + mark, build, adopt, attach.
        tlab.retire();
        let carried = tlab.thread_allocated_bytes();
        let mark = tlab.vm_thread_published_bytes();
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        tlab = unsafe { Tlab::new(base, usable) };
        tlab.adopt_allocation_total(carried);
        tlab.attach_vm_thread_allocation_total(&vm_a, mark);
        assert_eq!(
            vm_a.load(Ordering::Relaxed),
            1024,
            "the first attach credits the pre-first-refill external bytes"
        );

        tlab.alloc(4096, 8).unwrap();
        assert_eq!(
            vm_a.load(Ordering::Relaxed) + tlab.vm_thread_unpublished_bytes(),
            1024 + 4096,
            "VM total + unpublished is the thread's whole allocation"
        );
        tlab.note_external_allocation(512);
        assert_eq!(
            vm_a.load(Ordering::Relaxed),
            1024,
            "a sub-batch external note is held back"
        );
        assert_eq!(
            vm_a.load(Ordering::Relaxed) + tlab.vm_thread_unpublished_bytes(),
            1024 + 4096 + 512
        );

        // Second refill: the outgoing buffer was attached, so its retire
        // published everything and nothing is re-credited at the attach.
        tlab.retire();
        assert_eq!(vm_a.load(Ordering::Relaxed), 1024 + 512 + 4096);
        let carried = tlab.thread_allocated_bytes();
        let mark = tlab.vm_thread_published_bytes();
        assert_eq!(mark, carried);
        let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
        tlab = unsafe { Tlab::new(base2, usable2) };
        tlab.adopt_allocation_total(carried);
        tlab.attach_vm_thread_allocation_total(&vm_a, mark);
        assert_eq!(
            vm_a.load(Ordering::Relaxed),
            1024 + 512 + 4096,
            "an attach after an attached buffer must credit nothing twice"
        );
        tlab.alloc(2048, 8).unwrap();
        tlab.retire();
        assert_eq!(vm_a.load(Ordering::Relaxed), 1024 + 512 + 4096 + 2048);
        assert_eq!(
            vm_b.load(Ordering::Relaxed),
            0,
            "another VM's total never sees this thread's allocation"
        );
    }

    /// `getTotalThreadAllocatedBytes` is read as
    /// `<VM total> + vm_thread_unpublished_bytes()`, whose attached-buffer
    /// value is the live span. The live span, NOT `thread_allocated_bytes()`
    /// -- that is the span PLUS the thread's running total, which the total
    /// already holds. Adding it twice is what made the counter report exactly
    /// 2.00x on Generational and G1.
    ///
    /// This states the arithmetic the reader in
    /// `vm/src/vm/vm_exec.rs::total_allocated_bytes` implements, against a
    /// local stand-in for the VM's `HeapRealm::thread_allocated_total`.
    #[test]
    fn vm_total_plus_unpublished_tracks_actual_allocation() {
        let total = AtomicU64::new(0);
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.attach_vm_thread_allocation_total(&total, 0);
        tlab.alloc(4096, 8).unwrap();

        let vm = total.load(Ordering::Relaxed);
        assert_eq!(vm + tlab.consumed_bytes() as u64, 4096);
        assert_eq!(vm + tlab.vm_thread_unpublished_bytes(), 4096);
        assert_eq!(
            vm + tlab.thread_allocated_bytes(),
            4096,
            "before the first retire the two agree: nothing is settled yet"
        );

        tlab.retire();
        let carried = tlab.thread_allocated_bytes();
        let mark = tlab.vm_thread_published_bytes();
        let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base2, usable2) };
        tlab.adopt_allocation_total(carried);
        tlab.attach_vm_thread_allocation_total(&total, mark);
        tlab.alloc(2048, 8).unwrap();

        let vm = total.load(Ordering::Relaxed);
        assert_eq!(
            vm + tlab.vm_thread_unpublished_bytes(),
            4096 + 2048,
            "the VM total plus the unpublished term is the bytes allocated"
        );
        assert_eq!(
            tlab.vm_thread_unpublished_bytes(),
            tlab.consumed_bytes() as u64,
            "once attached and retired, the unpublished term is the live span"
        );
        assert_eq!(
            vm + tlab.thread_allocated_bytes(),
            (4096 + 2048) + 4096,
            "regression witness: the thread's running total is already inside the total, so adding it back over-reports by that total"
        );
    }

    /// The ZGC arena path used to open-code
    /// `install_tail_filler(); retire_taking_tail();`. The filler sets
    /// `cursor = end`, so the retire that followed read the WHOLE chunk as
    /// consumed and charged the unused tail as allocated -- the same defect
    /// `Tlab::retire` documents having fixed, reintroduced by splitting the
    /// two steps. `retire_with_filler_taking_tail` reads the span first.
    #[test]
    fn retire_with_filler_taking_tail_charges_only_the_consumed_span() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new_heap_staging(base, usable) };
        tlab.alloc(4096, 8).unwrap();
        let tail = unsafe { tlab.retire_with_filler_taking_tail(TLAB_FILLER_CLASS_ID) };
        assert_eq!(
            tail,
            Some((base as usize + 4096, base as usize + usable)),
            "the tail handed back must be the PRE-filler [cursor, end)"
        );
        assert_eq!(
            tlab.thread_allocated_bytes(),
            4096,
            "charged the unused tail -- the filler ran before the span was read"
        );
        assert!(tlab.is_retired() && tlab.reserved_tail().is_none());
    }

    #[test]
    fn pressure_tracker_begin_refill_resets_counters() {
        let mut t = TlabPressureTracker::new();
        t.record_allocation(128);
        t.record_allocation(128);
        t.begin_refill(16 * 1024);
        assert_eq!(t.allocations_since_last_refill, 0);
        assert_eq!(t.alloc_count, 0);
        assert_eq!(t.large_alloc_count, 0);
        assert_eq!(t.last_refill_size, 16 * 1024);
    }

    // ---------------------------------------------------------------
    // gen r4/alloc (2026-09-23) — `refill_request_size` and the watch latch
    // ---------------------------------------------------------------

    /// A thread that has never been handed a chunk starts at the documented
    /// baseline — and so does one whose EMPTY buffer was "retired" (the
    /// idempotent retire of `Tlab::empty()` arms `retired_since_refill`, and
    /// that must not be mistaken for sizing history).
    #[test]
    fn refill_request_size_starts_big_on_a_never_refilled_buffer() {
        let mut tlab = Tlab::empty();
        assert_eq!(tlab.refill_request_size(), initial_refill_size());
        tlab.retire();
        assert_eq!(tlab.refill_request_size(), initial_refill_size());
    }

    /// The defect the method exists for: after an early retire the refill
    /// site used to re-arm at the flat baseline. A buffer drained by compiled
    /// code (raw cursor bump, zero tracked allocations) and THEN retired must
    /// still be sized as pressure — i.e. on the consumption the retire banked,
    /// not on the `consumed == 0` a post-retire read of the cursor would give
    /// (the one-way shrink ratchet).
    #[test]
    fn refill_request_size_after_a_retire_uses_the_banked_consumption() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        // Drain it the way compiled code does.
        tlab.cursor = unsafe { tlab.start.add(usable) };
        tlab.retire();
        assert!(tlab.is_retired());
        assert_eq!(tlab.pressure.banked_consumed, usable);
        let next = tlab.refill_request_size();
        assert!(
            next >= usable,
            "a JIT-drained buffer retired at a safepoint must not shrink: {next} < {usable}"
        );
        assert!(next <= MAX_TLAB_SIZE);
    }

    /// A barely-used buffer that is retired must never be told to GROW: the
    /// banked consumption is tiny, so the answer is "keep" (sub-millisecond
    /// window) or "halve" — never more than the buffer it replaces.
    #[test]
    fn refill_request_size_after_an_idle_retire_never_grows() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(64, 8).unwrap();
        tlab.retire();
        let next = tlab.refill_request_size();
        assert!(next <= usable, "an idle retire grew the next buffer to {next}");
        assert!(next >= MIN_TLAB_SIZE);
    }

    /// The drain heuristic reads a short window as a fast FILL and grows. On a
    /// retire a short window is a quick PARK: five 4 KiB allocations (large,
    /// and enough of them that the "idle" shrink arm is off) then an
    /// immediate retire would be doubled by the raw heuristic while 70 % of
    /// the buffer went unused. The retired arm must hold it at its size,
    /// whatever the clock says.
    #[test]
    fn a_quick_park_on_a_part_used_buffer_is_not_read_as_a_fast_fill() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        for _ in 0..5 {
            tlab.alloc(4096, 8).unwrap();
        }
        tlab.retire();
        let p = &tlab.pressure;
        assert_eq!(p.banked_consumed, 5 * 4096);
        // What the drain heuristic says with a zero-length window: grow.
        assert_eq!(p.next_refill_size_at(p.banked_consumed, 0), 2 * usable);
        // What the retired arm says, at any fill time: hold.
        assert_eq!(tlab.refill_request_size(), usable);
    }

    /// The bank is written once per window. `retire` is idempotent and the
    /// transition graph really does retire twice (park, then GC initiator,
    /// then thread exit); a second retire has no live `start` and must not
    /// overwrite the first retire's reading with zeros.
    #[test]
    fn a_second_retire_does_not_overwrite_the_bank() {
        let (_owner, base, usable) = aligned_buffer(16 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(4096, 8).unwrap();
        tlab.retire();
        let first = (tlab.pressure.banked_consumed, tlab.pressure.banked_elapsed_ms);
        assert_eq!(first.0, 4096);
        tlab.retire();
        assert_eq!((tlab.pressure.banked_consumed, tlab.pressure.banked_elapsed_ms), first);
    }

    /// On a LIVE buffer the method is exactly `next_refill_size` — same
    /// input, same heuristic — so a refill site that switches to it changes
    /// nothing for a drain. Compared on the pure core with one frozen fill
    /// time, because two calls to the clock-reading wrappers can straddle a
    /// millisecond boundary.
    #[test]
    fn refill_request_size_on_a_live_buffer_is_the_live_heuristic() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.cursor = unsafe { tlab.start.add(usable / 2) };
        assert!(!tlab.pressure.retired_since_refill);
        for elapsed in [0u128, 50, 500] {
            let live = tlab.pressure.next_refill_size_at(tlab.consumed_bytes(), elapsed);
            let via_tracker = tlab.pressure.next_refill_size_at(usable / 2, elapsed);
            assert_eq!(live, via_tracker, "elapsed={elapsed}");
        }
        let answer = tlab.refill_request_size();
        assert!((MIN_TLAB_SIZE..=MAX_TLAB_SIZE).contains(&answer));
    }

    /// `next_refill_size_at` is the heuristic `next_refill_size_with_consumed`
    /// always ran, with the clock hoisted out: a slow window with little
    /// consumption halves, a fast drained one doubles, and the result is
    /// always 8-aligned and inside the clamp.
    #[test]
    fn the_sizing_core_is_a_pure_function_of_consumption_and_fill_time() {
        let mut t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024);
        assert_eq!(t.next_refill_size_at(1024, 500), 32 * 1024, "slow + idle halves");
        assert_eq!(t.next_refill_size_at(64 * 1024, 0), 128 * 1024, "fast + drained doubles");
        for (consumed, elapsed) in [(0usize, 0u128), (7, 3), (64 * 1024, 99), (1, 10_000)] {
            let n = t.next_refill_size_at(consumed, elapsed);
            assert_eq!(n % 8, 0);
            assert!((MIN_TLAB_SIZE..=MAX_TLAB_SIZE).contains(&n));
        }
    }

    /// The bump path reads the watch address from the buffer, latched at
    /// carve time, instead of the process-wide `OnceLock`. The latch must be
    /// the same value the global answers (so no report can change), and an
    /// empty buffer — which serves no bump — must carry none.
    #[test]
    fn the_watch_address_is_latched_when_the_buffer_is_carved() {
        assert_eq!(Tlab::empty().watch_addr, 0);
        assert_eq!(Tlab::empty_heap_staging().watch_addr, 0);
        let (_owner, base, usable) = aligned_buffer(4096);
        let tlab = unsafe { Tlab::new(base, usable) };
        assert_eq!(tlab.watch_addr, watched());
        let staging = unsafe { Tlab::new_heap_staging(base, usable) };
        assert_eq!(staging.watch_addr, watched());
    }

    // ---------------------------------------------------------------
    // gen r4w3/alloc3 (2026-09-23) — the filler-data memset elision and the
    // share sizer's latch (`TlabTuning`; the waste clause was removed by gce e2/o)
    // ---------------------------------------------------------------

    /// Both policies are latched from the flag snapshot when the buffer is
    /// carved, parse `=0` as OFF (a control arm written `=0` must not get the
    /// treatment — the `CRATONVM_G1_*` retraction), and are off by default and
    /// on an empty buffer.
    #[test]
    fn the_tuning_is_latched_from_the_flags_when_the_buffer_is_carved() {
        use cratonvm_types::flags::alloc_policy_defaults as defaults;
        const SKIP: &str = "CRATONVM_TLAB_FILLER_SKIP_ZERO";
        const SHARE: &str = "CRATONVM_TLAB_SHARE_SIZER";
        assert_eq!(Tlab::empty().tuning, TlabTuning::default());
        let (_owner, base, usable) = aligned_buffer(4096);
        let carve = |edits: &[(&str, Option<&str>)]| {
            cratonvm_types::flags::with_thread_overrides(edits, || {
                // SAFETY: `base..base+usable` is the test's own zeroed,
                // 8-aligned buffer, alive for the whole test.
                unsafe { Tlab::new(base, usable) }.tuning
            })
        };
        // Unset: each arm's ONE default (gen r4w4/alloc4 — a default flip is
        // the constant, and this test follows it). gen r5w4/defaults8: the
        // share sizer's default is per backend and a carve has no heap, so
        // the carve latches the NON-Generational default; the refill site
        // re-arms per heap (`Tlab::arm_share_sizer`, tested below).
        assert_eq!(
            carve(&[(SKIP, None), (SHARE, None)]),
            TlabTuning {
                filler_skip_zero: defaults::TLAB_FILLER_SKIP_ZERO,
                share_sizer: defaults::TLAB_SHARE_SIZER.other,
            }
        );
        assert_eq!(
            carve(&[(SKIP, Some("on")), (SHARE, Some("yes"))]),
            TlabTuning {
                filler_skip_zero: true,
                share_sizer: true,
            },
        );
        assert_eq!(
            carve(&[(SKIP, Some("0")), (SHARE, Some("0"))]),
            TlabTuning::default(),
            "`=0` must read as OFF",
        );
    }

    /// The pure predicate behind the filler-data elision: all three conditions
    /// are required, and a non-zero tripwire word always keeps the repair.
    #[test]
    fn the_filler_memset_is_skipped_only_for_a_clean_java_thread_tail_when_opted_in() {
        use TlabAccounting::{HeapStaging, JavaThread};
        assert!(filler_data_memset_is_redundant(true, JavaThread, 0));
        assert!(
            !filler_data_memset_is_redundant(false, JavaThread, 0),
            "default"
        );
        assert!(
            !filler_data_memset_is_redundant(true, HeapStaging, 0),
            "unaudited"
        );
        assert!(
            !filler_data_memset_is_redundant(true, JavaThread, 0xF111_E700),
            "repair"
        );
    }

    /// End to end on a real buffer: with the elision on, the filler HEADER is
    /// still written exactly (a walker strides the tail as before) and the data
    /// area is left as it was — made visibly non-zero here, beyond the header
    /// word the tripwire reads, so "not written" is observable. With it off the
    /// same bytes are zeroed. The deep-scan tripwire is switched off on the
    /// buffer so the planted bytes cannot arm the process-wide
    /// `FILLER_OVER_OBJECT` counter.
    ///
    /// gen r5w5/sizer9: the planted bytes stop one word short of the tail's
    /// end, because the skip now also reads the LAST word (a dirty one keeps
    /// the memset — the `dirty_end` arm below). Before that change this test
    /// planted the whole data area.
    #[test]
    fn a_skipped_filler_memset_still_writes_a_walkable_header() {
        use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        for (skip, dirty_end) in [(false, false), (true, false), (true, true)] {
            let (_owner, base, usable) = aligned_buffer(4096);
            let mut tlab = unsafe { Tlab::new(base, usable) };
            tlab.dbg_deadref_store = false;
            tlab.tuning.filler_skip_zero = skip;
            tlab.alloc(64, 8).unwrap();
            let filler_at = tlab.cursor as usize;
            let tail = tlab.remaining();
            let data_at = (filler_at + HEADER_SIZE) as *mut u8;
            let data_len = tail - HEADER_SIZE;
            let planted = if dirty_end { data_len } else { data_len - 8 };
            // SAFETY: `[data_at, data_at + data_len)` is inside the test's own
            // buffer; the header word the tripwire reads stays zero.
            unsafe { std::ptr::write_bytes(data_at, 0x5A, planted) };
            tlab.retire();
            // SAFETY: the retire just wrote an `ObjectHeader` at `filler_at`.
            let hdr = unsafe { &*(filler_at as *const ObjectHeader) };
            assert_eq!(hdr.class_id, TLAB_FILLER_CLASS_ID);
            assert_eq!(hdr.kind(), ObjectKind::Array);
            assert_eq!(hdr.element_type(), ArrayElementType::Int);
            assert_eq!(HEADER_SIZE + hdr.array_length() as usize * 4, tail);
            // SAFETY: same span as the write above.
            let data = unsafe { std::slice::from_raw_parts(data_at as *const u8, planted) };
            let expect = if skip && !dirty_end { 0x5A } else { 0 };
            assert!(
                data.iter().all(|&b| b == expect),
                "skip={skip} dirty_end={dirty_end}"
            );
        }
    }

    // ---------------------------------------------------------------
    // gen r4w4/alloc4 (2026-09-24) — the HotSpot-shaped share sizer, the
    // refill-waste limit and the per-VM counter's JIT top-up
    // ---------------------------------------------------------------

    const MIB: u64 = 1024 * 1024;

    /// The sizer's model end to end, on the thread's cursor-based total: the
    /// first sized refill opens a window at the baseline; the first refill
    /// after a collection samples the window (the first sample IS the average)
    /// and asks for `bytes per cycle / 50`; a window spanning several
    /// collections is averaged over them, so an idle thread shrinks.
    #[test]
    fn the_share_sizer_asks_for_a_fiftieth_of_the_threads_bytes_per_cycle() {
        // A staging buffer so the external notes below do not credit the
        // process-wide total other tests share; its own running total (the
        // sizer's input) still moves.
        let mut tlab = Tlab::empty_heap_staging();
        assert_eq!(
            tlab.share_refill_request(7),
            initial_refill_size(),
            "first window"
        );
        assert_eq!(
            tlab.share_refill_request(7),
            initial_refill_size(),
            "same window"
        );
        // 25 MiB allocated before the collection that ends the window.
        tlab.note_external_allocation((25 * MIB) as usize);
        let after_one = tlab.share_refill_request(8);
        assert_eq!(after_one, 512 * 1024, "25 MiB per cycle / 50 refills");
        let avg = tlab.share_sizer().average_bytes_per_cycle();
        assert!(
            (avg - (25 * MIB) as f64).abs() < 1.0,
            "the first sample is the average"
        );
        // Four collections pass while the thread allocates 4 MiB: 1 MiB per
        // cycle, folded in at the warm-up weight (1/2 for the second sample).
        tlab.note_external_allocation((4 * MIB) as usize);
        let after_idle = tlab.share_refill_request(12);
        let expected_avg = 0.5 * (25 * MIB) as f64 + 0.5 * MIB as f64;
        assert!((tlab.share_sizer().average_bytes_per_cycle() - expected_avg).abs() < 1.0);
        assert_eq!(
            after_idle,
            TlabShareSizer::desired_from_average(expected_avg),
            "averaged over the collections the window spanned"
        );
        assert!(after_idle < after_one, "an idler thread asks for less");
        assert_eq!(after_idle % 8, 0);
    }

    /// The pure size rule clamps to the TLAB bounds and stays on the grid,
    /// whatever the average (including nonsense ones).
    #[test]
    fn the_share_sizers_size_rule_clamps_and_aligns() {
        let f = TlabShareSizer::desired_from_average;
        assert_eq!(f(0.0), MIN_TLAB_SIZE);
        assert_eq!(f(-5.0), MIN_TLAB_SIZE);
        assert_eq!(f(f64::NAN), MIN_TLAB_SIZE);
        assert_eq!(f(f64::INFINITY), MIN_TLAB_SIZE);
        assert_eq!(f(1e30), MAX_TLAB_SIZE);
        assert_eq!(f((50 * 100_003) as f64) % 8, 0);
        assert_eq!(f((50 * 64 * 1024) as f64), 64 * 1024);
    }

    /// A thread predicted small that then allocates hard is doubled in place
    /// after `2 × target` GRANTED refills in one window, and never past the
    /// cap. Asking without being granted (a refusing refill gate) counts
    /// nothing.
    #[test]
    fn an_underpredicted_thread_is_raised_within_the_window() {
        let mut tlab = Tlab::empty_heap_staging();
        tlab.share_refill_request(1);
        // An idle window: sample ~0 → the floor.
        let small = tlab.share_refill_request(2);
        assert_eq!(small, MIN_TLAB_SIZE);
        for _ in 0..(10 * TLAB_TARGET_REFILLS) {
            assert_eq!(
                tlab.share_refill_request(2),
                small,
                "asking is not a refill"
            );
        }
        for _ in 0..(2 * TLAB_TARGET_REFILLS - 1) {
            tlab.note_share_refill(small);
            assert_eq!(tlab.share_refill_request(2), MIN_TLAB_SIZE, "not yet");
        }
        tlab.note_share_refill(small);
        assert_eq!(tlab.share_refill_request(2), 2 * MIN_TLAB_SIZE, "raised");
        let mut last = 0;
        for _ in 0..(40 * TLAB_TARGET_REFILLS) {
            tlab.note_share_refill(last.max(small));
            last = tlab.share_refill_request(2);
        }
        assert_eq!(last, MAX_TLAB_SIZE, "capped");
        // A collection replaces the raised size with the sample's.
        assert_eq!(tlab.share_refill_request(3), MIN_TLAB_SIZE);
    }

    /// The refill-waste limit: with the sizer on, a miss with a tail bigger
    /// than `desired / 64` KEEPS the buffer and raises the limit by the same
    /// step, so the same tail is retired after a bounded number of keeps; with
    /// the sizer off, or before the thread was sized, it never keeps.
    #[test]
    fn the_refill_waste_limit_keeps_a_big_tail_a_bounded_number_of_times() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.tuning.share_sizer = true;
        assert!(!tlab.keep_on_miss(8192), "never sized: no limit to apply");
        let mut sized = TlabShareSizer {
            window_epoch: 0,
            ..TlabShareSizer::default()
        };
        sized.set_desired(64 * 1024);
        tlab.adopt_share_sizer(sized);
        assert_eq!(tlab.share_sizer().refill_waste_limit(), 1024);
        tlab.alloc(usable - 4096, 8).unwrap();
        assert_eq!(tlab.remaining(), 4096);
        let mut keeps = 0;
        while tlab.keep_on_miss(8192) {
            keeps += 1;
            assert!(keeps < 64, "the limit must rise to the tail");
        }
        // 1 KiB, 2 KiB, 3 KiB are below the 4 KiB tail; 4 KiB is not.
        assert_eq!(keeps, 3);
        assert_eq!(tlab.remaining(), 4096, "a keep never touches the buffer");
        tlab.tuning.share_sizer = false;
        tlab.adopt_share_sizer(sized);
        assert!(
            !tlab.keep_on_miss(8192),
            "sizer off: the old retire-and-refill"
        );
        tlab.tuning.share_sizer = true;
        tlab.retire();
        assert!(
            !tlab.keep_on_miss(8192),
            "a retired buffer has nothing to keep"
        );
    }

    /// gen r5w4/defaults8: `arm_share_sizer` is what the refill site uses to
    /// give a freshly carved buffer its heap's answer. It overrides the
    /// heap-less latch both ways and touches nothing else — so an armed,
    /// sized buffer keeps, and a disarmed one retires as before.
    #[test]
    fn the_refill_site_arms_the_waste_limit_per_heap() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let fresh = || {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_TLAB_SHARE_SIZER", None)],
                // SAFETY: `base..base+usable` is the test's own zeroed,
                // 8-aligned buffer, alive for the whole test.
                || unsafe { Tlab::new(base, usable) },
            )
        };
        let mut sized = TlabShareSizer {
            window_epoch: 0,
            ..TlabShareSizer::default()
        };
        sized.set_desired(64 * 1024);

        let mut tlab = fresh();
        assert!(!tlab.tuning.share_sizer, "unset: the heap-less carve is off");
        let before = tlab.tuning;
        tlab.arm_share_sizer(true);
        assert_eq!(
            tlab.tuning,
            TlabTuning {
                share_sizer: true,
                ..before
            },
            "only the share-sizer bit moves"
        );
        tlab.adopt_share_sizer(sized);
        tlab.alloc(usable - 4096, 8).unwrap();
        assert!(tlab.keep_on_miss(8192), "armed and sized: the limit applies");

        let mut tlab = fresh();
        tlab.arm_share_sizer(true);
        tlab.arm_share_sizer(false);
        tlab.adopt_share_sizer(sized);
        tlab.alloc(usable - 4096, 8).unwrap();
        assert!(!tlab.keep_on_miss(8192), "disarmed: retire and refill");
    }

    /// gen r5w4/defaults8: the `sizer_on=` of the census follows the heap
    /// family the summary names; an explicit value wins on both. Unset it is
    /// OFF on every family (the Generational flip was reverted at the r5w4
    /// merge, see `alloc_policy_defaults::TLAB_SHARE_SIZER`).
    #[test]
    fn the_sizer_line_reports_the_setting_for_its_heap_family() {
        let on = |v: Option<&str>, generational: bool| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_TLAB_SHARE_SIZER", v)],
                || tlab_sizer_line(generational),
            )
            .contains("sizer_on=true ")
        };
        assert!(!on(None, true));
        assert!(!on(None, false));
        assert!(on(Some("1"), true));
        assert!(on(Some("1"), false));
        assert!(!on(Some("0"), true));
        let [waste, _] = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_TLAB_SHARE_SIZER", None)],
            tlab_census_lines,
        );
        assert!(waste.contains("[GC] tlab-sizer: sizer_on=false "), "{waste}");
    }

    /// The per-VM allocation counter is topped up at retire to the CONSUMED
    /// span, so bytes bumped without a note (the JIT's inline bump) reach it,
    /// and bytes the interpreter noted are not counted twice.
    #[test]
    fn the_vm_counter_is_topped_up_to_the_consumed_span_at_retire() {
        for (bumped, noted) in [(4096usize, 1024usize), (4096, 4096), (0, 0)] {
            let (_owner, base, usable) = aligned_buffer(64 * 1024);
            let mut tlab = unsafe { Tlab::new(base, usable) };
            let total = AtomicU64::new(0);
            tlab.attach_vm_allocation_counter(&total);
            if bumped > 0 {
                tlab.alloc(bumped, 8).unwrap();
            }
            if noted > 0 {
                let _ = tlab.note_vm_tlab_allocation(noted);
            }
            tlab.retire();
            assert_eq!(
                total.load(Ordering::Relaxed),
                bumped.max(noted) as u64,
                "bumped={bumped} noted={noted}"
            );
            tlab.retire();
            assert_eq!(
                total.load(Ordering::Relaxed),
                bumped.max(noted) as u64,
                "an idempotent second retire publishes nothing"
            );
        }
    }

    /// gen r4w6/tlab6: a test sink that records what it was offered and
    /// answers `take`.
    struct RecordingSink {
        take: bool,
        offered: std::sync::Mutex<Vec<(usize, usize)>>,
    }

    impl TlabTailSink for RecordingSink {
        fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool {
            self.offered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((start, end));
            self.take
        }
    }

    /// gen r4w6/tlab6: a retire offers the reserved tail to the ATTACHED sink
    /// first. Taken, no filler is written (the tail stays the zero bytes it
    /// was carved as) and the buffer still ends retired; declined, the filler
    /// is installed exactly as before. An unarmed buffer never asks.
    #[test]
    fn the_attached_sink_is_offered_the_tail_before_the_filler() {
        for take in [true, false] {
            let sink = RecordingSink {
                take,
                offered: std::sync::Mutex::new(Vec::new()),
            };
            let (_owner, base, usable) = aligned_buffer(4096);
            // SAFETY: `_owner` keeps `[base, base + usable)` alive, zeroed and
            // 8-aligned for the whole test, and nothing else touches it.
            let mut tlab = unsafe { Tlab::new(base, usable) };
            assert!(!tlab.has_tail_sink());
            // SAFETY: `sink` outlives every retire of `tlab` in this test.
            unsafe { tlab.attach_tail_sink(&sink) };
            assert!(tlab.has_tail_sink());
            tlab.alloc(64, 8).unwrap();
            let tail = tlab.reserved_tail().expect("a live TLAB has a tail");
            tlab.retire();
            assert!(tlab.is_retired() && tlab.reserved_tail().is_none(), "take={take}");
            let offered = sink.offered.lock().unwrap_or_else(|e| e.into_inner()).clone();
            assert_eq!(offered, vec![tail], "offered exactly the reserved tail once");
            // SAFETY: `base..base + usable` is `_owner`'s live allocation.
            let first_tail_word = unsafe { std::ptr::read(tail.0 as *const u32) };
            if take {
                assert_eq!(first_tail_word, 0, "a taken tail gets no filler header");
            } else {
                assert_eq!(
                    first_tail_word,
                    TLAB_FILLER_CLASS_ID.as_u32(),
                    "a declined tail is filled exactly as before"
                );
            }
            // Idempotent: a second retire has no tail to offer.
            tlab.retire();
            assert_eq!(sink.offered.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        }
        // A buffer nobody armed fills its tail: the process-wide path.
        let (_owner, base, usable) = aligned_buffer(4096);
        // SAFETY: as above.
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(64, 8).unwrap();
        let (tail, _) = tlab.reserved_tail().expect("a live TLAB has a tail");
        tlab.retire();
        // SAFETY: as above. (A ZGC sink another test registered declines an
        // address outside its arena, so the filler is certain here.)
        let first = unsafe { std::ptr::read(tail as *const u32) };
        assert_eq!(first, TLAB_FILLER_CLASS_ID.as_u32());
    }

    /// gen r5w2/alloc6: the attached sink's gate reads the tail's first word.
    /// A clean tail passes; a tail whose first word is not zero (the cursor
    /// sits on an object already handed out) is kept from the sink, whichever
    /// half of the word is set. End to end the declined tail then takes the
    /// filler path, whose tripwire counts it — not exercised here, because that
    /// counter is process-wide and other tests read it.
    #[test]
    fn the_attached_sink_gate_passes_only_a_zero_tail_head() {
        let (_owner, base, usable) = aligned_buffer(64);
        let at = base as usize;
        // SAFETY: `[base, base + usable)` is `_owner`'s live, 8-aligned buffer.
        assert!(unsafe { tail_head_reads_zero(at) }, "a fresh tail");
        for word in [1u64, 1 << 32, 0xF111_E700] {
            // SAFETY: as above; one 8-byte write at the 8-aligned start.
            unsafe { std::ptr::write(at as *mut u64, word) };
            // SAFETY: as above.
            assert!(!unsafe { tail_head_reads_zero(at) }, "{word:#x}");
        }
        // SAFETY: as above.
        unsafe { std::ptr::write(at as *mut u64, 0) };
        // SAFETY: as above.
        assert!(unsafe { tail_head_reads_zero(at + 8) } && unsafe { tail_head_reads_zero(at) });
        let _ = usable;
    }

    /// A retire of a buffer armed with `sink`, whose tail has `dirty` written
    /// at `dirty_at` (an offset from the tail start) before the retire, in a
    /// buffer carved with `CRATONVM_DBG_DEADREF_STORE` set to `deep`. Returns
    /// what the sink was offered and the tail's first word afterwards.
    fn retire_with_dirty_tail(deep: bool, dirty_at: usize) -> (Vec<(usize, usize)>, u32, usize) {
        let sink = RecordingSink {
            take: true,
            offered: std::sync::Mutex::new(Vec::new()),
        };
        let (_owner, base, usable) = aligned_buffer(4096);
        let mut tlab = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DBG_DEADREF_STORE", if deep { Some("1") } else { None })],
            // SAFETY: `_owner` keeps `[base, base + usable)` alive, zeroed and
            // 8-aligned for the whole test, and nothing else touches it.
            || unsafe { Tlab::new(base, usable) },
        );
        assert_eq!(tlab.dbg_deadref_store, deep, "the mode is latched at the carve");
        // SAFETY: `sink` outlives every retire of `tlab` below.
        unsafe { tlab.attach_tail_sink(&sink) };
        tlab.alloc(64, 8).unwrap();
        let (tail_start, tail_end) = tlab.reserved_tail().expect("a live TLAB has a tail");
        assert!(tail_start + dirty_at + 8 <= tail_end);
        // Something other than this buffer's bump wrote past its cursor.
        // SAFETY: an 8-aligned word inside the tail of `_owner`'s buffer.
        unsafe { std::ptr::write((tail_start + dirty_at) as *mut u64, 0x5EED_0B1E) };
        tlab.retire();
        assert!(tlab.is_retired() && tlab.reserved_tail().is_none());
        let offered = sink.offered.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // SAFETY: as above.
        let first = unsafe { std::ptr::read(tail_start as *const u32) };
        (offered, first, tail_end - tail_start)
    }

    /// gen r5w5/sizer9: the attached sink's gate reads the tail's LAST word
    /// as well as its first. A tail that is clean at its head but dirty at its
    /// end is never offered to the sink — the sink would hand those bytes to
    /// the next allocation as unused young — and takes the filler path
    /// instead. The dirty word is counted on `FILLER_OVER_OBJECT` (not
    /// asserted: the counter is process-wide and other tests move it).
    #[test]
    fn a_tail_dirty_at_its_last_word_is_kept_from_the_sink() {
        let (offered, first, tail_len) = retire_with_dirty_tail(false, 4096 - 64 - 8);
        assert_eq!(tail_len, 4096 - 64);
        assert!(offered.is_empty(), "offered {offered:?}");
        assert_eq!(first, TLAB_FILLER_CLASS_ID.as_u32(), "declined: the filler path");
    }

    /// gen r5w5/sizer9: the O(1) gate reads two words, so a dirty word in the
    /// MIDDLE of an otherwise clean tail still reaches the sink in a default
    /// run — which is why the deep mode exists. Under
    /// `CRATONVM_DBG_DEADREF_STORE` the whole tail is scanned and the same
    /// retire is refused.
    #[test]
    fn the_deadref_mode_scans_the_whole_tail_before_the_sink_takes_it() {
        let middle = 2048;
        let (offered, first, _) = retire_with_dirty_tail(false, middle);
        assert_eq!(offered.len(), 1, "the O(1) gate cannot see the middle");
        assert_eq!(first, 0, "taken: no filler written");
        let (offered, first, _) = retire_with_dirty_tail(true, middle);
        assert!(offered.is_empty(), "the deep gate refuses it: {offered:?}");
        assert_eq!(first, TLAB_FILLER_CLASS_ID.as_u32());
    }

    /// gen r5w5/sizer9: `CRATONVM_TLAB_SHARE_SIZER_MAX_KIB` caps every desired
    /// size (1 MiB when unset), on the grid and inside the TLAB bounds.
    #[test]
    fn the_share_sizer_cap_bounds_every_desired_size() {
        let cap = share_sizer_max();
        assert!((MIN_TLAB_SIZE..=MAX_TLAB_SIZE).contains(&cap) && cap % 8 == 0);
        let mut s = TlabShareSizer::default();
        s.set_desired(64 * MAX_TLAB_SIZE);
        assert_eq!(s.desired(), cap);
        assert_eq!(s.refill_waste_limit(), cap / TLAB_REFILL_WASTE_FRACTION);
    }

    /// gen r5w5/sizer9: `keep_may_apply` is `keep_on_miss`'s test without its
    /// side effects — it agrees with it in every state and moves neither the
    /// limit nor the census, so the refill site can ask it before paying for
    /// the young-headroom probe.
    #[test]
    fn keep_may_apply_is_keep_on_miss_without_the_raise() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        // SAFETY: `_owner` keeps the buffer alive, zeroed and 8-aligned.
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.arm_share_sizer(true);
        assert!(!tlab.keep_may_apply(), "never sized");
        let mut sized = TlabShareSizer {
            window_epoch: 0,
            ..TlabShareSizer::default()
        };
        sized.set_desired(64 * 1024);
        tlab.adopt_share_sizer(sized);
        tlab.alloc(usable - 4096, 8).unwrap();
        for _ in 0..8 {
            assert!(tlab.keep_may_apply());
        }
        assert_eq!(
            tlab.share_sizer().refill_waste_limit(),
            1024,
            "asking raised nothing"
        );
        let mut keeps = 0;
        while tlab.keep_may_apply() {
            assert!(tlab.keep_on_miss(8192), "the two agree");
            keeps += 1;
            assert!(keeps < 64);
        }
        assert!(!tlab.keep_on_miss(8192), "and agree on the refusal");
        assert_eq!(keeps, 3, "the same three keeps as the limit test");
        tlab.arm_share_sizer(false);
        tlab.adopt_share_sizer(sized);
        assert!(!tlab.keep_may_apply(), "disarmed");
        tlab.retire();
        tlab.arm_share_sizer(true);
        assert!(!tlab.keep_may_apply(), "retired");
    }

    /// The tuning element carries the sizer line after a newline, with the
    /// keys a log grep needs, and the waste line still comes first.
    #[test]
    fn the_sizer_line_rides_the_tuning_element() {
        let [waste, _guard] = tlab_census_lines();
        let mut lines = waste.lines();
        let first = lines.next().unwrap_or_default();
        assert!(first.starts_with("[GC] tlab-waste: "), "{first}");
        let sizer = lines.next().expect("the sizer line follows the waste line");
        for key in [
            "[GC] tlab-sizer: ",
            "sizer_on=",
            "sizer_target_refills=50 ",
            "sizer_refills=",
            "sizer_mean_granted=",
            "sizer_min_desired=",
            "sizer_max_desired=",
            "sizer_samples=",
            "sizer_raises=",
            "sizer_keeps=",
            "sizer_keep_bytes=",
        ] {
            assert!(sizer.contains(key), "{key:?} missing from {sizer:?}");
        }
        assert!(lines.next().is_none());
    }
}
