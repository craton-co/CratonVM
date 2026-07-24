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

use std::time::Instant;

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
        Some((c, e))
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
    pub fn retire(&mut self) {
        // SAFETY: see method-level note — backing memory valid, single owner.
        unsafe {
            self.install_tail_filler(TLAB_FILLER_CLASS_ID);
        }
        self.start = std::ptr::null_mut();
        self.cursor = std::ptr::null_mut();
        self.end = std::ptr::null_mut();
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
            0,
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

    /// T5.5.1 — Recommended byte size for the next refill. Delegates to
    /// the per-thread [`TlabPressureTracker`] which applies the
    /// grow/shrink heuristic. Call this after retiring the current TLAB
    /// and before requesting a fresh buffer from the arena.
    pub fn next_refill_size(&mut self) -> usize {
        self.pressure.next_refill_size()
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
        let elapsed_ms = self.refill_started_at.elapsed().as_millis();
        let alloc_count = self.alloc_count as usize;
        let large_allocs = self.large_alloc_count as usize;
        let current = self.last_refill_size;

        // Grow when: fast fill OR few-but-large allocations dominated.
        let grow = elapsed_ms < FAST_REFILL_THRESHOLD_MS
            || (alloc_count < FAST_REFILL_ALLOC_COUNT && large_allocs > 0);

        // Shrink when: very slow fill OR the TLAB was barely touched.
        let shrink = elapsed_ms > SLOW_REFILL_THRESHOLD_MS || alloc_count < SLOW_REFILL_ALLOC_COUNT;

        let next = if grow && !shrink {
            current.saturating_mul(2).min(MAX_TLAB_SIZE)
        } else if shrink && !grow {
            (current / 2).max(MIN_TLAB_SIZE)
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
        assert_eq!(hdr.kind, ObjectKind::Array);
        assert_eq!(hdr.element_type, ArrayElementType::Int);
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
