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
/// |  24..  | pressure | rest |
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
    start: *mut u8,
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
    /// [`PROCESS_ALLOCATED_BYTES`]. A high-water mark, not a counter: every
    /// publication sends `thread_alloc_carry - process_published` and then
    /// moves the mark up.
    ///
    /// Stated this way rather than as "add the bytes I just added" so the two
    /// counters cannot drift apart at all: a publication site that is
    /// forgotten is caught up by the next one, and a site that runs twice
    /// sends zero the second time. The process-wide counter has had both
    /// defects -- see [`TlabAccounting`] and the reader note on
    /// `PROCESS_ALLOCATED_BYTES`.
    ///
    /// Carried across refills by [`Self::adopt_allocation_total`], which
    /// requires the outgoing buffer to have been retired and therefore to have
    /// published everything it held.
    process_published: u64,
    /// Whether this buffer's consumption is the PROGRAM's allocation, and so
    /// belongs in [`PROCESS_ALLOCATED_BYTES`]. See [`TlabAccounting`].
    accounting: TlabAccounting,
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
    /// A Java thread's own buffer. Credits [`PROCESS_ALLOCATED_BYTES`].
    #[default]
    JavaThread,
    /// A heap-internal staging buffer whose span a Java thread's TLAB (or the
    /// VM's `note_external_allocation`) counts again. Does NOT credit
    /// [`PROCESS_ALLOCATED_BYTES`]; its own `thread_alloc_carry` still moves,
    /// because that field is read per-buffer by the ZGC TLAB statistics.
    HeapStaging,
}

/// Process-wide cumulative allocation, in the sense
/// `com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes` means it:
/// a total over the life of the PROCESS that never decreases.
///
/// It has to be its own counter. The obvious source, the heap's
/// `allocated_bytes()`, is an occupancy gauge derived from `used - free`, so it
/// FALLS at every collection — and a caller measuring a window that contains a
/// GC gets the difference of two occupancies rather than the bytes it
/// allocated. The same Hibernate HQL parse read 488 MB under ZGC, 49 MB under
/// Generational at a 2 GB heap, and 458 MB under Generational at 8 GB. Same
/// bytecode, three answers, none of them cumulative.
///
/// Fed from the two places a thread's own total is fed (`retire` rolling in the
/// consumed span, and `note_external_allocation`), so it is the sum of every
/// thread's retired total. Readers add the calling thread's **live TLAB span**
/// on top — the span, not the thread's running total, which is already in here;
/// other threads' in-flight spans are not visible cross-thread by design
/// (see `Tlab::thread_allocated_bytes`), which bounds the under-count by one
/// TLAB per running thread and keeps the value monotonic.
///
/// Only buffers marked [`TlabAccounting::JavaThread`] feed it. A heap-internal
/// staging buffer's span is re-counted one layer up, so crediting it here
/// double-counts every byte — see [`TlabAccounting`].
static PROCESS_ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Cumulative bytes retired into the process-wide total. See
/// [`PROCESS_ALLOCATED_BYTES`].
pub fn process_allocated_bytes() -> u64 {
    PROCESS_ALLOCATED_BYTES.load(Ordering::Relaxed)
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
            pressure: TlabPressureTracker::new(),
            thread_alloc_carry: 0,
            process_published: 0,
            accounting: TlabAccounting::JavaThread,
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
        Self {
            start: ptr,
            cursor: ptr,
            end: aligned_end_addr as *mut u8,
            pressure,
            // A fresh buffer knows nothing about what the thread allocated
            // before it. The refill site carries the running total across
            // with `adopt_allocation_total`.
            thread_alloc_carry: 0,
            process_published: 0,
            accounting: TlabAccounting::JavaThread,
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
            ..Self::new(ptr, size)
        }
    }

    /// Whose allocation this buffer's consumed span represents.
    pub fn accounting(&self) -> TlabAccounting {
        self.accounting
    }

    /// Send everything this buffer has SETTLED but not yet published into the
    /// process-wide total, and move the high-water mark up.
    ///
    /// A no-op for a heap-internal staging buffer, whose span is counted again
    /// one layer up -- see [`TlabAccounting`].
    ///
    /// "Settled" is [`Self::thread_alloc_carry`], NOT
    /// [`Self::thread_allocated_bytes`]: the LIVE TLAB span is deliberately
    /// left out, because the reader of the process-wide counter adds the
    /// calling thread's live span itself (it is the one in-flight span it may
    /// read). Publishing it here as well is a double-count.
    fn publish_to_process_total(&mut self) {
        if self.accounting != TlabAccounting::JavaThread {
            return;
        }
        let delta = self
            .thread_alloc_carry
            .saturating_sub(self.process_published);
        if delta != 0 {
            self.process_published = self.thread_alloc_carry;
            PROCESS_ALLOCATED_BYTES.fetch_add(delta, Ordering::Relaxed);
        }
    }

    /// Bytes this buffer has published to the process-wide total.
    ///
    /// Exposed because the accounting tests cannot assert against the global
    /// itself: every other test in the binary credits it concurrently.
    pub fn process_published_bytes(&self) -> u64 {
        self.process_published
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
        // reasons of its own), so it published its whole carry and
        // `prior_total` is already inside `PROCESS_ALLOCATED_BYTES`. Adopting
        // the high-water mark alongside the total is what stops the successor
        // from publishing the same bytes a second time: leaving it at 0 would
        // re-send the thread's ENTIRE history at every refill.
        self.process_published = prior_total;
    }

    /// Record bytes allocated by this thread that never passed through the
    /// TLAB — humongous objects and arrays, and every post-GC retry that goes
    /// straight to the heap arena.
    ///
    /// Without this the counter would see only TLAB-sized allocations, and a
    /// caller measuring a 1 MiB-per-iteration loop (netty's chunk-reuse
    /// assertion is exactly that) would be told it allocated nothing.
    #[inline]
    pub fn note_external_allocation(&mut self, bytes: usize) {
        self.thread_alloc_carry = self.thread_alloc_carry.saturating_add(bytes as u64);
        self.publish_to_process_total();
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
    /// in-flight object as a 40-byte object and later create a free-list hole
    /// in its body.  The initializer therefore runs while the reservation is
    /// still private; only a fully walker-coherent object is made visible.
    #[inline(always)]
    pub fn alloc_initialized<F>(&mut self, size: usize, align: usize, init: F) -> Option<*mut u8>
    where
        F: FnOnce(*mut u8),
    {
        debug_assert!(align.is_power_of_two());
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
        if watch_covers(ptr as usize, new_cursor) {
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
        if cratonvm_types::flags().gc.dbg_deadref_store {
            // SAFETY: `[ptr, ptr+footprint)` is inside `[cursor, end)`, memory
            // this TLAB owns and that is mapped.
            let w = unsafe { std::ptr::read_unaligned(ptr as *const u32) };
            if w != 0 {
                static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 8 {
                    eprintln!(
                        "[tlab-audit] BUMP-OVER-OCCUPIED #{n}: TLAB alloc at 0x{:x}                          (size={size} align={align}) is handing out memory whose first word is                          0x{w:x}, not zero. start=0x{:x} cursor=0x{:x} end=0x{:x}. The chunk                          this TLAB is bumping through is not free.",
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
    /// The unused tail space is wasted but will be reclaimed at GC time
    /// when the arena is reset.
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
            Some((tail_start, tail_end)) => reclaim_tail_via_sinks(tail_start, tail_end),
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

    /// Retire, handing the reserved tail to the CALLER instead of to a sink or
    /// a filler. For an allocator that owns both the buffer and the arena it
    /// was carved from (`zgc::arena_tlab`), which returns the tail to its own
    /// free list and must not have a process-wide sink do it a second time.
    ///
    /// Same accounting as [`Self::retire`]: the consumed span is credited to
    /// the thread and process totals, the three pointers are nulled, and the
    /// "sized before retired" tripwire is armed.
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
        self.thread_alloc_carry = self.thread_alloc_carry.saturating_add(consumed);
        self.publish_to_process_total();
        self.start = std::ptr::null_mut();
        self.cursor = std::ptr::null_mut();
        self.end = std::ptr::null_mut();
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
        if occupant == 0 && cratonvm_types::flags().gc.dbg_deadref_store {
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
                    "[tlab-audit] install_tail_filler is about to stamp a filler over an                      ALREADY-ALLOCATED object: the word at the TLAB cursor is a live class id,                      not the zero this chunk was carved with. Everything from here to the TLAB                      end is about to become one synthetic int[], so the next object-start walk                      will stride over it and the evacuator will refuse every reference into it.",
                );
            }
        }
        if tail < HEADER_SIZE {
            // Bug-D fix (2026-06-12): a sub-`HEADER_SIZE` tail cannot hold a
            // walkable `int[]` filler, and ZEROING it (the old behaviour) is
            // unsafe — a zeroed sub-40 region is byte-identical to a live
            // `new Object()` (class_id 0 / num_slots 0), so the non-moving
            // young sweep's linear walk strides a phantom 40-byte object off
            // the object grid (the `RemoteCIDRFilter` "implausible object
            // size" desync that a later moving GC turns into a SIGSEGV).
            //
            // Stamp the GAP-filler sentinel instead: class_id at offset 0 and
            // the exact gap length at offset 4 — both inside the smallest
            // (8-byte) gap. The sweep walker reclaims the span in O(1) on the
            // `class_id == GAP_FILLER_CLASS_ID` match. `aligned`/`end` are
            // 8-aligned so `tail` is a non-zero multiple of 8 here (8/16/24/32).
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
            // Zero the data area so any stale bytes don't trip header
            // sanity checks on the next walk.
            std::ptr::write_bytes((aligned + HEADER_SIZE) as *mut u8, 0, data_bytes);
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

/// Round-5 #9 / round-7 #9 — synthetic class id used for `int[]` TLAB
/// fillers installed at TLAB retire time. Picked from the high-bit
/// "synthetic VM" class-id range (the same kind of reserved sentinel as
/// `AUTOBOX_CLASS_ID`, now `u32::MAX`) so it cannot collide with a
/// classloader-issued id.
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

/// Times [`Tlab::install_tail_filler`] found a live object at the cursor it was/// Times [`Tlab::install_tail_filler`] found a live object at the cursor it was
/// about to fill from. See the tripwire there; a non-zero count is a
/// use-after-free in waiting, not a diagnostic curiosity.
pub static FILLER_OVER_OBJECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Times a TLAB refill handed out a chunk that already held an object. See the
/// tripwire in `GenerationalHeap::refill_tlab`.
pub static REFILL_OVER_OBJECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const TLAB_FILLER_CLASS_ID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0xF111_E700);

/// Bug-D fix (2026-06-12) — synthetic class id stamped into a
/// **sub-`HEADER_SIZE`** TLAB tail (8/16/24/32 bytes) that is too small to
/// hold a walkable `int[]` filler. A standard filler needs >= 40 bytes; a
/// shorter tail cannot carry a full `ObjectHeader`, and zeroing it is unsafe
/// because a zeroed sub-40 region is byte-identical to a live `new Object()`
/// (class_id 0, num_slots 0 — its only non-zero header word, identity_hash at
/// offset 8, lies past an 8-byte gap). The non-moving young sweep's linear
/// walk then cannot distinguish the gap from a live object and desyncs.
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

    /// A heap-internal staging TLAB must not credit the process-wide
    /// allocation total: the bytes it hands out are counted a second time by
    /// the Java thread's own TLAB (or by `note_external_allocation`) one layer
    /// up, and crediting both is exactly why `getTotalThreadAllocatedBytes`
    /// A heap-internal staging TLAB must not publish to the process-wide
    /// allocation total: the bytes it hands out are counted a second time by
    /// the Java thread's own TLAB (or by `note_external_allocation`) one layer
    /// up. Publishing from both is why `getTotalThreadAllocatedBytes` read
    /// 3.00x of retained heap under ZGC against 2.00x under Gen/G1.
    #[test]
    fn heap_staging_tlab_does_not_publish_to_the_process_total() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut staging = unsafe { Tlab::new_heap_staging(base, usable) };
        assert_eq!(staging.accounting(), TlabAccounting::HeapStaging);
        staging.alloc(4096, 8).unwrap();
        staging.note_external_allocation(1024);
        staging.retire();
        assert_eq!(
            staging.process_published_bytes(),
            0,
            "a heap-internal staging buffer published to the process-wide              total; its span is re-counted one layer up, so this double-counts"
        );
        // Its own per-buffer total still moves: the ZGC TLAB statistics read it.
        assert_eq!(staging.thread_allocated_bytes(), 4096 + 1024);
    }

    /// The Java-thread arm of the same contract, and the size of the
    /// publication: exactly the settled bytes, once.
    #[test]
    fn java_thread_tlab_publishes_settled_bytes_exactly_once() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        assert_eq!(tlab.accounting(), TlabAccounting::JavaThread);
        tlab.alloc(4096, 8).unwrap();
        tlab.note_external_allocation(1024);
        assert_eq!(
            tlab.process_published_bytes(),
            1024,
            "the external note settles immediately; the LIVE TLAB span must              wait for retire, because the reader adds it as the live-span term"
        );
        tlab.retire();
        assert_eq!(tlab.process_published_bytes(), 4096 + 1024);
        // Idempotent: a second retire settles nothing, so it publishes nothing.
        tlab.retire();
        assert_eq!(tlab.process_published_bytes(), 4096 + 1024);
    }

    /// A refill must not re-publish the thread's history. `Tlab::new` starts a
    /// fresh buffer, so without carrying the high-water mark across, the
    /// successor's first retire would send the WHOLE running total again --
    /// once per refill, which on a parse that refills thousands of times is
    /// not a 2x error but an unbounded one.
    #[test]
    fn refill_carries_the_published_mark_so_history_is_not_republished() {
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(4096, 8).unwrap();
        let carried = {
            tlab.retire();
            tlab.thread_allocated_bytes()
        };
        assert_eq!(carried, 4096);

        let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
        let mut next = unsafe { Tlab::new(base2, usable2) };
        next.adopt_allocation_total(carried);
        assert_eq!(
            next.process_published_bytes(),
            carried,
            "the successor must adopt the mark, not just the total"
        );
        next.alloc(2048, 8).unwrap();
        next.retire();
        assert_eq!(next.thread_allocated_bytes(), 4096 + 2048);
        assert_eq!(
            next.process_published_bytes() - carried,
            2048,
            "the successor published only its OWN bytes"
        );
    }

    /// `getTotalThreadAllocatedBytes` is read as
    /// `process_allocated_bytes() + <live span>`. The live span, NOT
    /// `thread_allocated_bytes()` -- that is the span PLUS the thread's running
    /// total, which the global already holds. Adding it twice is what made the
    /// counter report exactly 2.00x on Generational and G1.
    ///
    /// This states the arithmetic the reader in
    /// `vm/src/vm/vm_exec.rs::total_allocated_bytes` implements, against a
    /// local stand-in for the global (the real one is credited concurrently by
    /// every other test in this binary).
    #[test]
    fn process_total_plus_live_span_tracks_actual_allocation() {
        // Stand-in for PROCESS_ALLOCATED_BYTES, fed the same way: by each
        // buffer's published mark.
        let (_owner, base, usable) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base, usable) };
        tlab.alloc(4096, 8).unwrap();

        let global = tlab.process_published_bytes();
        assert_eq!(global + tlab.consumed_bytes() as u64, 4096);
        assert_eq!(
            global + tlab.thread_allocated_bytes(),
            4096,
            "before the first retire the two agree: nothing is settled yet"
        );

        tlab.retire();
        let carried = tlab.thread_allocated_bytes();
        let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
        let mut tlab = unsafe { Tlab::new(base2, usable2) };
        tlab.adopt_allocation_total(carried);
        tlab.alloc(2048, 8).unwrap();

        let global = tlab.process_published_bytes();
        assert_eq!(
            global + tlab.consumed_bytes() as u64,
            4096 + 2048,
            "the process total plus the LIVE SPAN is the bytes allocated"
        );
        assert_eq!(
            global + tlab.thread_allocated_bytes(),
            (4096 + 2048) + 4096,
            "regression witness: the thread's running total is already inside              the global, so adding it back over-reports by that total"
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
}
