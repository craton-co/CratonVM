// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java threads: registry, monitors, virtual-thread scheduling and the main ThreadGroup.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.threads.<field>`.

use crate::threading::monitor::MonitorTable;
use crate::threading::thread_registry::ThreadRegistry;
use crate::types::{ObjectRef, Value};
use crate::vm::vm_init::{MainThreadGroupInit, ThrowableStackTrace};
use cratonvm_types::ClassId;
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize};
use std::sync::{Arc, OnceLock, Weak};

/// Sentinel for a Throwable field slot that has not yet been resolved in this
/// VM. Field layouts are VM state: a process-global slot cache silently binds
/// a second VM to the first VM's bootstrap layout.
pub(crate) const UNRESOLVED_THROWABLE_FIELD_INDEX: usize = usize::MAX;
/// Sentinel for a StackTraceElement class id that has not yet been resolved in
/// this VM. Class id zero is valid (`java/lang/Object`), so it cannot be the
/// sentinel.
pub(crate) const UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID: u32 = u32::MAX;

/// Independent registry locks for retained Throwable traces. This is a power
/// of two so a captured throwable selects its shard with a cheap hash mask.
pub(crate) const THROWABLE_STACK_SHARDS: usize = 32;

/// The hasher of one retained-trace shard, keyed by the throwable's address.
///
/// Not Fx: the shard is chosen by address bits 3..8
/// (`vm_init::throwable_stack_shard`), so every key in one shard has the same
/// low eight bits, and Fx's `x * K` keeps them -- the product's low bits
/// depend only on the key's low bits. hashbrown indexes its buckets by the
/// hash's LOW bits, so every key of a shard started probing in the same
/// 1/256th of the table, and a throw-heavy run (one entry per live
/// throwable) spent ~10% of a plain `new RuntimeException` in the shard's
/// `insert` probe (`perf`, `R16ThrowpathLoop newfull`, 2026-10-07). This
/// folds the product's high half, which every key bit reaches, into the low
/// half. Nothing reads the hash but the map, so the registry's contents and
/// answers are unchanged.
#[derive(Default, Clone, Copy)]
pub(crate) struct ThrowableAddrHasher(u64);

impl std::hash::Hasher for ThrowableAddrHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(b));
        }
    }

    #[inline]
    fn write_u64(&mut self, key: u64) {
        let m = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.0 = m ^ (m >> 32);
    }

    #[inline]
    fn write_usize(&mut self, key: usize) {
        self.write_u64(key as u64);
    }
}

/// The indexed half of a [`ThrowableStackShard`].
pub(crate) type ThrowableStackMap = std::collections::HashMap<
    usize,
    ThrowableStackTrace,
    std::hash::BuildHasherDefault<ThrowableAddrHasher>,
>;

/// One retained-trace shard: throwable address -> its trace.
///
/// A capture APPENDS its trace to `pending` (a sequential push, no hashing);
/// a trace enters the indexed `map` only when somebody asks for one (a
/// reader flushes the shard's pending list first) or when it survives a
/// collection's sweep. Most throwables are caught and dropped without their
/// trace ever being read, and their rows now die in `pending` at the next
/// sweep without ever being hashed. The map insert -- a random probe into a
/// table holding one row per throwable allocated since the last collection,
/// and the cache misses that come with it -- was ~10% of a plain compiled
/// `new RuntimeException` (FP `perf`, `JkiThrowCpu new-only`, 2026-10-07).
///
/// The answers are the map's as if every store had gone straight into it:
/// `pending` is in store order and is always merged in that order, so the
/// LAST store of an address wins, as `HashMap::insert` made it win.
///
/// A refill stores the same address again, and refills allocate no Java heap,
/// so nothing may sweep for a long time: at `compact_at` rows the list drops
/// every row a later row of the same address replaces (a set sized to the
/// list, not the table), and the threshold doubles while the rows are mostly
/// distinct -- which bounds the list by the distinct throwables it names, as
/// the map was bounded.
#[derive(Default)]
pub(crate) struct ThrowableStackShard {
    map: ThrowableStackMap,
    pending: Vec<(usize, ThrowableStackTrace)>,
    /// Compact `pending` when it reaches this length; 0 = the minimum.
    compact_at: usize,
}

/// The first `pending` length at which a shard collapses duplicates.
const THROWABLE_PENDING_COMPACT_MIN: usize = 4096;

impl ThrowableStackShard {
    /// Record `trace` for the throwable at `address` (the newest store of an
    /// address wins).
    #[inline]
    pub(crate) fn push(&mut self, address: usize, trace: ThrowableStackTrace) {
        // A refill loop of one throwable: replace its row in place.
        if let Some(last) = self.pending.last_mut().filter(|(a, _)| *a == address) {
            last.1 = trace;
            return;
        }
        self.pending.push((address, trace));
        if self.pending.len() >= self.compact_at.max(THROWABLE_PENDING_COMPACT_MIN) {
            self.compact_pending();
        }
    }

    /// Drop every pending row a later pending row of the same address
    /// replaces; then compact again only once the list has doubled.
    fn compact_pending(&mut self) {
        let len = self.pending.len();
        let mut seen: std::collections::HashSet<
            usize,
            std::hash::BuildHasherDefault<ThrowableAddrHasher>,
        > = std::collections::HashSet::with_capacity_and_hasher(len, Default::default());
        let mut keep = vec![false; len];
        for (i, (address, _)) in self.pending.iter().enumerate().rev() {
            keep[i] = seen.insert(*address);
        }
        let mut index = 0usize;
        self.pending.retain(|_| {
            let k = keep[index];
            index += 1;
            k
        });
        self.compact_at = (self.pending.len() * 2).max(THROWABLE_PENDING_COMPACT_MIN);
    }

    /// Is anything waiting to be indexed? A reader holding only the read lock
    /// must take the write lock and [`Self::flush`] first when it is.
    #[inline]
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Index `trace` at `address` now, after every pending row (so it is the
    /// newest store of its address). For the sweep's relocated rows.
    pub(crate) fn insert_now(&mut self, address: usize, trace: ThrowableStackTrace) {
        self.flush();
        self.map.insert(address, trace);
    }

    /// Index every pending row, in store order (keeps the list's capacity).
    pub(crate) fn flush(&mut self) {
        let Self {
            map,
            pending,
            compact_at,
        } = self;
        for (address, trace) in pending.drain(..) {
            map.insert(address, trace);
        }
        *compact_at = 0;
    }

    /// The trace of the throwable at `address`. Exact only once nothing is
    /// pending ([`Self::flush`]); debug-asserted.
    #[inline]
    pub(crate) fn get(&self, address: &usize) -> Option<&ThrowableStackTrace> {
        debug_assert!(self.pending.is_empty(), "a shard read with unflushed rows");
        self.map.get(address)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty() && self.pending.is_empty()
    }

    /// Rows held, a pending row counted even when a later store of its
    /// address will replace it at the flush.
    pub(crate) fn len(&self) -> usize {
        self.map.len() + self.pending.len()
    }

    /// Keep the rows whose ADDRESS `keep` accepts: a pure test of the address,
    /// so pending rows are filtered in place without being indexed.
    pub(crate) fn retain_addresses(&mut self, mut keep: impl FnMut(usize) -> bool) {
        self.pending.retain(|(address, _)| keep(*address));
        self.map.retain(|&address, _| keep(address));
    }

    /// The start of a collection's sweep: drop every pending row `survives`
    /// refuses (a dead throwable's trace dies without being hashed) and index
    /// the rest in store order. Afterwards the shard is only `map`, which the
    /// sweep filters and remaps as it always did.
    pub(crate) fn settle_pending(
        &mut self,
        survives: impl Fn(usize) -> bool,
    ) -> &mut ThrowableStackMap {
        let Self {
            map,
            pending,
            compact_at,
        } = self;
        for (address, trace) in pending.drain(..) {
            if survives(address) {
                map.insert(address, trace);
            }
        }
        *compact_at = 0;
        map
    }
}

/// Hot Throwable metadata scoped to one VM. Values are immutable once
/// resolved, so atomics avoid turning the constructor path into lock traffic.
pub(crate) struct ThrowableLayoutCache {
    pub(crate) detail_message: AtomicUsize,
    pub(crate) cause: AtomicUsize,
    pub(crate) suppressed_exceptions: AtomicUsize,
    pub(crate) backtrace: AtomicUsize,
    pub(crate) depth: AtomicUsize,
    pub(crate) stack_trace: AtomicUsize,
    pub(crate) stack_trace_element_class_id: AtomicU32,
    pub(crate) throwable_class_id: AtomicU32,
    pub(crate) suppressed_sentinel_static_index: AtomicUsize,
    /// JIT round 16 (lane throwpath, TP16-2): `CRATONVM_THROWABLE_FILL_DIRECT_STORES`,
    /// read once per VM: 0 not read yet, 1 off, 2 on.
    fill_direct_stores: AtomicU8,
    /// JIT round 16 wave 7 (lane throwpath, TP16-6):
    /// `CRATONVM_THROWABLE_CAPTURE_MEMO`, read once per VM as above.
    capture_memo: AtomicU8,
    /// JIT round 20 wave 12 (lane qMisc, L4W15-2):
    /// `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES`, read once per VM as above.
    capture_memo_splices: AtomicU8,
    /// JIT round 16 wave 9 (lane throwpath, TP16-9):
    /// `CRATONVM_THROWABLE_SCREEN_ONCE`, read once per VM as above.
    screen_once: AtomicU8,
    /// JIT round 16 wave 11 (lane throw2, TP16-10):
    /// `CRATONVM_THROWABLE_STANDIN_SITE_MEMO`, read once per VM as above.
    standin_site_memo: AtomicU8,
    /// JIT round 16 wave 11 (lane throw2, TP16-12):
    /// `CRATONVM_THROWABLE_TRIM_IS_A_MEMO`, read once per VM as above.
    trim_is_a_memo: AtomicU8,
    /// JIT round 16 wave 12 (lane throw3, TP16-14):
    /// `CRATONVM_JIT_CALLEE_VERDICT_IDENTITY`, read once per VM as above.
    callee_verdict_identity: AtomicU8,
    /// JIT round 16 wave 15 (lane throwfill):
    /// `CRATONVM_THROWABLE_WALK_SCOPED_LOOKUPS`, read once per VM as above.
    walk_scoped_lookups: AtomicU8,
    /// JIT round 17 wave 1 (lane throwfill, TF17-1):
    /// `CRATONVM_THROWABLE_FUSED_CAPTURE`, read once per VM as above.
    fused_capture: AtomicU8,
    /// JIT round 17 wave 2 (lane walk, TF17-5):
    /// `CRATONVM_THROWABLE_BOUNDED_WALK`, read once per VM as above.
    bounded_walk: AtomicU8,
    /// JIT round 17 wave 1 (lane throwfill, TF17-0):
    /// `CRATONVM_DBG_THROWFILL_PHASES`, read once per VM as above, and the
    /// census it prints ([`Self::note_throwfill_phases`]): nanoseconds per
    /// phase, then captures, then retained frames.
    dbg_throwfill_phases: AtomicU8,
    throwfill_phase_counts: [AtomicU64; THROWFILL_PHASE_COUNTS],
    /// JIT round 18 wave 10 (lane throws): the walk-shape half of the same
    /// census ([`Self::note_throwfill_shape`]): compiled activations, inlined
    /// levels, then one column per reason the fused door handed the walk to
    /// the plain one. Counted only under `CRATONVM_DBG_THROWFILL_PHASES=1`.
    throwfill_shape_counts: [AtomicU64; THROWFILL_SHAPE_COUNTS],
    /// JIT round 19 wave 1 (lane throwfill, THROWS18-1):
    /// `CRATONVM_THROWABLE_FUSED_REWRITTEN`, read once per VM as above.
    fused_rewritten: AtomicU8,
    /// JIT round 16 wave 9 (lane throwpath): `CRATONVM_DBG_CAPTURE_MEMO`,
    /// read once per VM as above, and the census it prints
    /// ([`Self::note_capture_memo`]).
    dbg_capture_memo: AtomicU8,
    capture_memo_counts: [AtomicU64; CAPTURE_MEMO_COUNTS],
    /// JIT round 19 proposal wave (lane throws19, TP16-15):
    /// `CRATONVM_JIT_LOCAL_HANDLER_LEAN`, read once per VM as above.
    local_handler_lean: AtomicU8,
    /// TP16-15: `CRATONVM_DBG_LOCAL_HANDLER_LOOKUP`, latched per VM -- read
    /// this way only under `CRATONVM_JIT_LOCAL_HANDLER_LEAN=1`; the default
    /// arm keeps its per-catch read.
    dbg_local_handler_lookup: AtomicU8,
    /// Interpreter round i3 wave 73, lane L2:
    /// `CRATONVM_THROWABLE_LOADER_MISS_MEMO`, read once per VM as above.
    loader_miss_memo: AtomicU8,
    /// nio21 wave 16, lane L5: is `OmitStackTraceInFastThrow` in effect for
    /// this VM (`VmConfig::omit_stack_trace_in_fast_throw_resolved`), latched
    /// once per VM as above.
    fast_throw: AtomicU8,
    /// nio21 wave 16, lane L5: `CRATONVM_FAST_THROW_SHARED_INSTANCE`, read
    /// once per VM as above.
    fast_throw_shared_instance: AtomicU8,
    /// nio21 wave 16, lane L5: the JNI global-ref handle (0 = none yet) of
    /// this VM's preallocated fast-throw instance of each
    /// `exceptions::FastThrowKind`, in `FastThrowKind::slot` order -- HotSpot's
    /// `Universe::_null_ptr_exception` and its four siblings. The JNI global
    /// table roots and remaps them, so no collector step names them.
    pub(crate) fast_throw_handles: [AtomicU64; 5],
    /// JIT round 20 wave 11, lane pNat (L5W17-5):
    /// `CRATONVM_FAST_THROW_UNLOCKED_HANDOUT`, read once per VM as above.
    fast_throw_unlocked_handout: AtomicU8,
    /// L5W17-5's positive control: `CRATONVM_DBG_FAST_THROW_HANDOUT`, read
    /// once per VM as above, and the count of unlocked hand-outs it prints
    /// ([`Self::note_fast_throw_unlocked_handout`]).
    dbg_fast_throw_handout: AtomicU8,
    fast_throw_unlocked_handouts: AtomicU64,
    /// The `OmitStackTraceInFastThrow` hotness rule's half of the method
    /// profile: the (method, bci, kind) sites at which an implicit exception
    /// has been raised -- HotSpot's per-bci trap bits in the MDO
    /// (`runtime::exceptions::FastThrowTrapProfile`). Written only while fast
    /// throw is on for this VM. Its lock is a leaf.
    pub(crate) fast_throw_traps: crate::runtime::exceptions::FastThrowTrapProfile,
    /// `CRATONVM_FAST_THROW_COUNT_FALLBACK`, read once per VM as above.
    fast_throw_count_fallback: AtomicU8,
    /// nio21 wave 16, lane L5: `CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER`,
    /// read once per VM as above (it was read on every compiled catch).
    handler_sink_genuine_receiver: AtomicU8,
    /// Interpreter round i3 wave 82, lane L2:
    /// `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT`, read once per VM as above.
    backtrace_in_object: AtomicU8,
    /// The frame ids a throwable-owned backtrace holder names
    /// (`backtrace_ids`), per VM. The one member of this cache with a lock: a
    /// leaf, taken only by a capture or a read with
    /// [`Self::backtrace_in_object_enabled`] on.
    pub(crate) frame_ids: super::backtrace_ids::BacktraceFrameIds,
    /// JIT round 20 (lane throw): frames of freshly captured traces stored in
    /// the off-heap registry (`throwable_stacks`) since the last sweep or the
    /// last collection this count requested
    /// ([`Self::take_retained_trace_pressure`]). A registry row lives until a
    /// collection sweeps its dead throwable, and nothing else ever asked for
    /// one: a deep recursion catching an implicit exception per level stored
    /// ~1024 frames (~100 KB) per ~48-byte throwable, never filled young, and
    /// was OOM-killed at 16 GB RSS where HotSpot, whose backtrace lives in the
    /// Java heap, collects (`R20ThrowNestedTrapCapture`).
    retained_trace_frames: AtomicUsize,
    /// JIT round 20 wave 7, lane revVM: the served `StackWalker` walks' JDK
    /// frames, named once per VM and redefinition count, for the readers that
    /// may not take the class-manager lock (a published trace, the debugger's
    /// listing). See [`crate::runtime::stackwalker::StackWalkFramesNamed`].
    pub(crate) stack_walk_named: crate::runtime::stackwalker::StackWalkFramesNamed,
    /// JIT round 21 wave 2, lane throws (THROWS21-1):
    /// `CRATONVM_JIT_THROW_CENSUS_STATS_ONLY` without
    /// `CRATONVM_DBG_JIT_METHOD_STATS`, latched once per VM as above
    /// ([`Self::throw_census_skipped`]).
    throw_census_skipped: AtomicU8,
}

/// Registry frames (see [`ThrowableLayoutCache::retained_trace_frames`]) past
/// which a throwable-building door asks for a collection: about 100 MB of
/// `BacktraceFrame`s. A program that keeps that many traces alive pays one
/// young collection per this many frames stored, as an allocation of the
/// same size would cost it.
pub(crate) const RETAINED_TRACE_FRAMES_GC_BUDGET: usize = 1 << 20;

/// The [`ThrowableLayoutCache::note_capture_memo`] columns: a memo-eligible
/// capture the thread's memo answered.
pub(crate) const CAPTURE_MEMO_HIT: usize = 0;
/// A memo-eligible capture that did not match the thread's memo.
pub(crate) const CAPTURE_MEMO_MISS: usize = 1;
/// A miss that left a memo for the next capture.
pub(crate) const CAPTURE_MEMO_MADE: usize = 2;
/// A miss that left none (a screen engaged, or the stack is not memoisable).
pub(crate) const CAPTURE_MEMO_REFUSED: usize = 3;
/// Refusal reason lines printed so far (`capture_memo_refusal_report_due`).
const CAPTURE_MEMO_REFUSAL_REPORTS: usize = 4;
/// Hits answered before the walk (the raw walk key matched); also `HIT`s.
pub(crate) const CAPTURE_MEMO_RAW_HIT: usize = 5;
/// Hits on a capture under reflective splices (JIT round 20 wave 12, lane
/// qMisc, L4W15-2: the switch's positive control); also `HIT`s.
pub(crate) const CAPTURE_MEMO_SPLICED_HIT: usize = 6;
const CAPTURE_MEMO_COUNTS: usize = 7;

/// The timed phases of a throwable capture ([`ThrowableLayoutCache::note_throwfill_phases`]):
/// the compiled-frame walk, the layout (slots, fill-frame trim, retained
/// frames built), the screens and the rules they gate (middle `Thread.run`,
/// loader frames, hidden drop), the stand-in and `Thread.run` appends, the
/// registry store (the replaced trace dropped with it).
pub(crate) const THROWFILL_PHASES: usize = 5;
/// The phase columns, then the capture count, then the retained-frame count.
const THROWFILL_PHASE_COUNTS: usize = THROWFILL_PHASES + 2;
/// Names of the [`THROWFILL_PHASES`] columns, in order.
const THROWFILL_PHASE_NAMES: [&str; THROWFILL_PHASES] =
    ["walk", "layout", "screens-rules", "appends", "store"];
/// JIT round 18 wave 10 (lane throws): how many reasons
/// `stackwalker::fused_capture_refusal` can give (its discriminants are the
/// column offsets, in [`THROWFILL_SHAPE_REFUSAL_NAMES`] order).
pub(crate) const THROWFILL_SHAPE_FUSED_REFUSALS: usize = 4;
/// JIT round 19 wave 1 (lane throwfill, THROWS18-1): the shape column after
/// the refusal columns -- captures whose walk the OSR / call dedupe rewrote
/// and that the fused door laid out itself (`CRATONVM_THROWABLE_FUSED_REWRITTEN=1`).
const THROWFILL_SHAPE_FUSED_REWRITTEN: usize = 2 + THROWFILL_SHAPE_FUSED_REFUSALS;
/// Compiled activations, inlined levels, the refusal columns, then
/// [`THROWFILL_SHAPE_FUSED_REWRITTEN`].
const THROWFILL_SHAPE_COUNTS: usize = 2 + THROWFILL_SHAPE_FUSED_REFUSALS + 1;
/// Names of the refusal columns, in `stackwalker::FusedRefusal` order.
const THROWFILL_SHAPE_REFUSAL_NAMES: [&str; THROWFILL_SHAPE_FUSED_REFUSALS] =
    ["no-compiled", "splice", "compact-off", "rewritten"];

impl ThrowableLayoutCache {
    pub(crate) fn new() -> Self {
        Self {
            detail_message: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            cause: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            suppressed_exceptions: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            backtrace: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            depth: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            stack_trace: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            stack_trace_element_class_id: AtomicU32::new(UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID),
            throwable_class_id: AtomicU32::new(UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID),
            suppressed_sentinel_static_index: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            fill_direct_stores: AtomicU8::new(0),
            capture_memo: AtomicU8::new(0),
            capture_memo_splices: AtomicU8::new(0),
            screen_once: AtomicU8::new(0),
            standin_site_memo: AtomicU8::new(0),
            trim_is_a_memo: AtomicU8::new(0),
            callee_verdict_identity: AtomicU8::new(0),
            walk_scoped_lookups: AtomicU8::new(0),
            fused_capture: AtomicU8::new(0),
            bounded_walk: AtomicU8::new(0),
            dbg_throwfill_phases: AtomicU8::new(0),
            throwfill_phase_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            throwfill_shape_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            fused_rewritten: AtomicU8::new(0),
            dbg_capture_memo: AtomicU8::new(0),
            capture_memo_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            local_handler_lean: AtomicU8::new(0),
            dbg_local_handler_lookup: AtomicU8::new(0),
            loader_miss_memo: AtomicU8::new(0),
            fast_throw: AtomicU8::new(0),
            fast_throw_shared_instance: AtomicU8::new(0),
            fast_throw_handles: std::array::from_fn(|_| AtomicU64::new(0)),
            fast_throw_unlocked_handout: AtomicU8::new(0),
            dbg_fast_throw_handout: AtomicU8::new(0),
            fast_throw_unlocked_handouts: AtomicU64::new(0),
            fast_throw_traps: crate::runtime::exceptions::FastThrowTrapProfile::default(),
            fast_throw_count_fallback: AtomicU8::new(0),
            handler_sink_genuine_receiver: AtomicU8::new(0),
            backtrace_in_object: AtomicU8::new(0),
            frame_ids: super::backtrace_ids::BacktraceFrameIds::new(),
            retained_trace_frames: AtomicUsize::new(0),
            stack_walk_named: crate::runtime::stackwalker::StackWalkFramesNamed::new(),
            throw_census_skipped: AtomicU8::new(0),
        }
    }

    /// JIT round 21 wave 2, lane throws (THROWS21-1 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): may a compiled
    /// catch skip the process-wide local-handler census counters
    /// (`jit::helpers::note_local_handler_at_catch`)? Only with
    /// `CRATONVM_JIT_THROW_CENSUS_STATS_ONLY=1` (opt-in, default OFF) and
    /// without `CRATONVM_DBG_JIT_METHOD_STATS`, the census line's only reader.
    /// Read once per VM.
    pub(crate) fn throw_census_skipped(&self) -> bool {
        Self::latched_flag(&self.throw_census_skipped, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_THROW_CENSUS_STATS_ONLY")
                && !cratonvm_types::flags().jit.method_stats
        })
    }

    /// JIT round 20 (lane throw): count `frames` registry frames just stored.
    #[inline]
    pub(crate) fn note_retained_trace_frames(&self, frames: usize) {
        self.retained_trace_frames
            .fetch_add(frames, std::sync::atomic::Ordering::Relaxed);
    }

    /// JIT round 20 (lane throw): a collection swept the registry.
    pub(crate) fn reset_retained_trace_frames(&self) {
        self.retained_trace_frames
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// JIT round 20 (lane throw): have more than
    /// [`RETAINED_TRACE_FRAMES_GC_BUDGET`] registry frames been stored since
    /// the last sweep? Consumes the count when it answers `true`, so one
    /// budget's worth of stores asks for at most one collection even when no
    /// sweep follows (a pause another thread already runs, a backend whose
    /// cycle sweeps later): never a collection per throw.
    pub(crate) fn take_retained_trace_pressure(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.retained_trace_frames.load(Ordering::Relaxed) >= RETAINED_TRACE_FRAMES_GC_BUDGET
            && self.retained_trace_frames.swap(0, Ordering::Relaxed)
                >= RETAINED_TRACE_FRAMES_GC_BUDGET
    }

    /// nio21 wave 16, lane L5: `CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER`
    /// (default ON, `=0` kill switch), read once per VM. The compiled-handler
    /// sinks asked the environment on every caught throw (a flag-name set probe
    /// and an environment read per throw on the `Nio21ImplicitCatch` rows).
    pub(crate) fn handler_sink_genuine_receiver_enabled(&self) -> bool {
        Self::latched_flag(&self.handler_sink_genuine_receiver, || {
            cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER",
            )
        })
    }

    /// nio21 wave 16, lane L5: is HotSpot's `OmitStackTraceInFastThrow` in
    /// effect for this VM? `resolve` is
    /// `VmConfig::omit_stack_trace_in_fast_throw_resolved`, asked once.
    pub(crate) fn fast_throw_enabled(&self, resolve: impl FnOnce() -> bool) -> bool {
        Self::latched_flag(&self.fast_throw, resolve)
    }

    /// nio21 wave 16, lane L5: does a fast throw hand out this VM's ONE
    /// preallocated instance per kind, as HotSpot does
    /// (`CRATONVM_FAST_THROW_SHARED_INSTANCE`, default ON; `=0` restores the
    /// r9w9 fresh stackless object per throw), read once per VM.
    pub(crate) fn fast_throw_shared_instance_enabled(&self) -> bool {
        Self::latched_flag(&self.fast_throw_shared_instance, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FAST_THROW_SHARED_INSTANCE")
        })
    }

    /// L5W17-5 (`docs/feature-designs/jit-ir-optimizations.md`;
    /// JIT round 20 wave 11, lane pNat): does the shared fast-throw instance's
    /// hand-out read its JNI global-ref slot without the table's mutex
    /// (`exceptions::preallocated_fast_throw_instance`)?
    /// `CRATONVM_FAST_THROW_UNLOCKED_HANDOUT`, default ON, `=0` restores the
    /// locked `resolve`; read once per VM.
    pub(crate) fn fast_throw_unlocked_handout_enabled(&self) -> bool {
        Self::latched_flag(&self.fast_throw_unlocked_handout, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FAST_THROW_UNLOCKED_HANDOUT")
        })
    }

    /// L5W17-5's positive control (`CRATONVM_DBG_FAST_THROW_HANDOUT=1`, read
    /// once per VM): count one unlocked hand-out and print the running count
    /// at every power of two (`[fast-throw-handout] unlocked n=<count>`).
    /// Nothing is counted with the flag unset.
    pub(crate) fn note_fast_throw_unlocked_handout(&self) {
        use std::sync::atomic::Ordering;
        let on = Self::latched_flag(&self.dbg_fast_throw_handout, || {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FAST_THROW_HANDOUT").is_some()
        });
        if !on {
            return;
        }
        let n = self
            .fast_throw_unlocked_handouts
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if n.is_power_of_two() {
            eprintln!("[fast-throw-handout] unlocked n={n}");
        }
    }

    /// May a compiled site that the profile rule does not make hot still turn
    /// hot after `exceptions::FAST_THROW_HOT_THRESHOLD` full throws (the
    /// pre-profile rule, kept for a site the profile cannot name or a body
    /// compiled before its first recorded throw, which HotSpot would have
    /// recompiled)? `CRATONVM_FAST_THROW_COUNT_FALLBACK`, default ON, `=0`
    /// leaves the profile rule alone. Read once per VM.
    pub(crate) fn fast_throw_count_fallback_enabled(&self) -> bool {
        Self::latched_flag(&self.fast_throw_count_fallback, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FAST_THROW_COUNT_FALLBACK")
        })
    }

    /// Latch `flag` (0 unread, 1 off, 2 on) from `read` on first use.
    fn latched_flag(flag: &AtomicU8, read: impl FnOnce() -> bool) -> bool {
        use std::sync::atomic::Ordering;
        match flag.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = read();
                flag.store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// JIT round 16 wave 9 (lane throwpath, TP16-9): may a throwable capture
    /// decide its three per-frame pre-screens in one pass
    /// (`stackwalker::CaptureScreens`)? `CRATONVM_THROWABLE_SCREEN_ONCE`
    /// (default ON since round 16, `=0` kill switch; the flag row is
    /// authoritative), read once per VM.
    pub(crate) fn screen_once_enabled(&self) -> bool {
        Self::latched_flag(&self.screen_once, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_SCREEN_ONCE")
        })
    }

    /// JIT round 16 wave 11 (lane throw2, TP16-10): may a throwable capture
    /// answer a compiled innermost frame's "no stand-in frames" from the
    /// thread's `stackwalker::StandinSiteMemo`?
    /// `CRATONVM_THROWABLE_STANDIN_SITE_MEMO` (default ON since round 16, `=0`
    /// kill switch; the flag row is authoritative), read once per VM.
    pub(crate) fn standin_site_memo_enabled(&self) -> bool {
        Self::latched_flag(&self.standin_site_memo, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_STANDIN_SITE_MEMO")
        })
    }

    /// JIT round 16 wave 11 (lane throw2, TP16-12): may the fill-frame trim
    /// answer `throwable->is_a(holder)` from the thread's
    /// `stackwalker::ThrowableIsAMemo`? `CRATONVM_THROWABLE_TRIM_IS_A_MEMO=1`
    /// (default OFF until measured), read once per VM.
    pub(crate) fn trim_is_a_memo_enabled(&self) -> bool {
        Self::latched_flag(&self.trim_is_a_memo, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_THROWABLE_TRIM_IS_A_MEMO")
        })
    }

    /// JIT round 16 wave 15 (lane throwfill): may a throwable capture walk its
    /// compiled frames with walk-scoped lookups
    /// (`conservative_roots::active_compiled_frames_walk_scoped`: one
    /// code-range snapshot per walk, the recursion's repeated return address
    /// answered once, the safepoint-id index, one census flush)?
    /// `CRATONVM_THROWABLE_WALK_SCOPED_LOOKUPS=1` (default OFF until
    /// measured), read once per VM.
    pub(crate) fn walk_scoped_lookups_enabled(&self) -> bool {
        Self::latched_flag(&self.walk_scoped_lookups, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_THROWABLE_WALK_SCOPED_LOOKUPS")
        })
    }

    /// JIT round 17 wave 1 (lane throwfill, TF17-1): may a throwable capture
    /// take the fused layout (`stackwalker::capture_throwable_backtrace_fused`:
    /// the walk consumed into the retained frames, labels moved rather than
    /// cloned, the label screen and the one-pass screens once per distinct
    /// label) on a walk-scoped walk? The same trace.
    /// `CRATONVM_THROWABLE_FUSED_CAPTURE` (default ON since the round-17 close,
    /// `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md), read once
    /// per VM.
    pub(crate) fn fused_capture_enabled(&self) -> bool {
        Self::latched_flag(&self.fused_capture, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_FUSED_CAPTURE")
        })
    }

    /// JIT round 17 wave 2 (lane walk, TF17-5): may a throwable capture capped
    /// at `MaxJavaStackTraceDepth` walk only the innermost cap-plus-slack
    /// compiled activations of each chain entry, proving the cut lies outside
    /// the kept frames and redoing the walk whole when it cannot
    /// (`stackwalker::capture_throwable_backtrace_bounded`)? The same trace.
    /// `CRATONVM_THROWABLE_BOUNDED_WALK=1` (default OFF until measured), read
    /// once per VM.
    pub(crate) fn bounded_walk_enabled(&self) -> bool {
        Self::latched_flag(&self.bounded_walk, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_THROWABLE_BOUNDED_WALK")
        })
    }

    /// Interpreter round i3 wave 73, lane L2
    /// (`i73-L2-proposal-memoise-the-delegation-miss-capture-20261106.md`): may
    /// a class-loader delegation miss's `ClassNotFoundException` capture be
    /// memoised like any other throw loop's? Two arms, one per mode:
    /// `--jdk-only`, the loader-middle screen's pairs that the decoded
    /// `stackwalker::loader_middle_pairs_decided_quiet` proves get no frames
    /// no longer refuse the memo (nor run the rules); `--compatible`, a capture
    /// carrying a served loader's miss hint is memoised with the loader in its
    /// key (`ThrowableCaptureKey::loader_miss`). The same trace.
    /// `CRATONVM_THROWABLE_LOADER_MISS_MEMO=1` (default OFF until measured),
    /// read once per VM.
    pub(crate) fn loader_miss_memo_enabled(&self) -> bool {
        Self::latched_flag(&self.loader_miss_memo, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_THROWABLE_LOADER_MISS_MEMO")
        })
    }

    /// Interpreter round i3 wave 82, lane L2
    /// (`i73-L2-proposal-backtrace-owned-by-the-throwable-20261106.md`): does a
    /// capture keep its frames in a `long[]` holder stored in
    /// `Throwable.backtrace` (`backtrace_ids`), as HotSpot keeps its backtrace
    /// in the object, instead of the address-keyed registry the collector must
    /// sweep? Readers then decode the holder; a trace the holder cannot name
    /// keeps the registry. The same traces.
    /// `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1` (default OFF until
    /// measured), read once per VM.
    pub(crate) fn backtrace_in_object_enabled(&self) -> bool {
        Self::latched_flag(&self.backtrace_in_object, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT")
        })
    }

    /// Test seam: decide [`Self::backtrace_in_object_enabled`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_backtrace_in_object_for_test(&self, on: bool) {
        self.backtrace_in_object
            .store(if on { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
    }

    /// JIT round 19 wave 1 (lane throwfill, THROWS18-1): may the fused capture
    /// lay out a walk the OSR / call dedupe rewrites (a compiled frame naming
    /// the interpreter frame it was pushed from) itself -- the dedupe run first,
    /// its bci overrides laid out with the walk
    /// (`stackwalker::capture_throwable_backtrace_fused_with`) -- instead of
    /// handing it to the plain door? The same trace.
    /// `CRATONVM_THROWABLE_FUSED_REWRITTEN=1` (default OFF until measured),
    /// read once per VM.
    pub(crate) fn fused_rewritten_enabled(&self) -> bool {
        Self::latched_flag(&self.fused_rewritten, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_THROWABLE_FUSED_REWRITTEN")
        })
    }

    /// THROWS18-1: count one capture whose rewritten walk the fused door laid
    /// out itself, for the TF17-0 census line (`fused-rewritten=`). Counted
    /// only under `CRATONVM_DBG_THROWFILL_PHASES=1`.
    pub(crate) fn note_throwfill_fused_rewritten(&self) {
        use std::sync::atomic::Ordering;
        self.throwfill_shape_counts[THROWFILL_SHAPE_FUSED_REWRITTEN]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// JIT round 17 wave 1 (lane throwfill, TF17-0): is the throwable capture
    /// phase census on? `CRATONVM_DBG_THROWFILL_PHASES=1`, read once per VM.
    pub(crate) fn throwfill_phases_enabled(&self) -> bool {
        Self::latched_flag(&self.dbg_throwfill_phases, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_THROWFILL_PHASES")
        })
    }

    /// TF17-0: add one capture's phase times (nanoseconds, in
    /// `THROWFILL_PHASE_NAMES` order) and its retained depth to the census,
    /// and print the per-capture averages of each window of 1024 captures:
    /// `[throwfill-phases] captures=.. frames=.. walk=..ns layout=..ns ...`.
    /// The split the orchestrator reads against `R17ThrowfillDepth`'s rows
    /// (the intercept and the per-frame slope of each phase).
    pub(crate) fn note_throwfill_phases(&self, phase_ns: [u64; THROWFILL_PHASES], depth: usize) {
        use std::sync::atomic::Ordering;
        for (counter, ns) in self.throwfill_phase_counts.iter().zip(phase_ns) {
            counter.fetch_add(ns, Ordering::Relaxed);
        }
        self.throwfill_phase_counts[THROWFILL_PHASES + 1]
            .fetch_add(u64::try_from(depth).unwrap_or(u64::MAX), Ordering::Relaxed);
        let captures =
            self.throwfill_phase_counts[THROWFILL_PHASES].fetch_add(1, Ordering::Relaxed) + 1;
        if captures < 1024 {
            return;
        }
        // One window of (about) 1024 captures, then the counters restart, so
        // each line describes the probe row running at the time. Debug only:
        // a capture landing between two `swap`s is counted in either window.
        let take = |i: usize| self.throwfill_phase_counts[i].swap(0, Ordering::Relaxed);
        let captures = take(THROWFILL_PHASES).max(1);
        let mut line = format!(
            "[throwfill-phases] captures={captures} frames={}",
            take(THROWFILL_PHASES + 1) / captures
        );
        let mut total = 0u64;
        for (i, name) in THROWFILL_PHASE_NAMES.iter().enumerate() {
            let per = take(i) / captures;
            total = total.saturating_add(per);
            line.push_str(&format!(" {name}={per}ns"));
        }
        line.push_str(&format!(" total={total}ns"));
        // JIT round 18 wave 10 (lane throws): the walk's shape over the same
        // window -- compiled activations and inlined levels per capture, and
        // how many captures the fused door handed to the plain one, by reason.
        let shape = |i: usize| self.throwfill_shape_counts[i].swap(0, Ordering::Relaxed);
        line.push_str(&format!(
            " compiled={} inlined={} fused-plain:",
            shape(0) / captures,
            shape(1) / captures
        ));
        for (i, name) in THROWFILL_SHAPE_REFUSAL_NAMES.iter().enumerate() {
            line.push_str(&format!(" {name}={}", shape(2 + i)));
        }
        // JIT round 19 wave 1 (lane throwfill, THROWS18-1).
        line.push_str(&format!(
            " fused-rewritten={}",
            shape(THROWFILL_SHAPE_FUSED_REWRITTEN)
        ));
        eprintln!("{line}");
    }

    /// JIT round 18 wave 10 (lane throws): add one capture's walk shape to the
    /// TF17-0 census (printed with its phase line): `compiled` activations,
    /// `inlined` levels over them, and `fused_refusal`, the
    /// `stackwalker::FusedRefusal` column (`< THROWFILL_SHAPE_FUSED_REFUSALS`)
    /// when the fused door would hand this walk to the plain one. The question
    /// it answers: why `R17ThrowfillDepth`'s alternating rows did not move with
    /// `CRATONVM_THROWABLE_FUSED_CAPTURE` while the recursive ones fell by a
    /// third (a refused walk, or inlined levels the fused door does not save).
    pub(crate) fn note_throwfill_shape(
        &self,
        compiled: usize,
        inlined: usize,
        fused_refusal: Option<usize>,
    ) {
        use std::sync::atomic::Ordering;
        let as_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        self.throwfill_shape_counts[0].fetch_add(as_u64(compiled), Ordering::Relaxed);
        self.throwfill_shape_counts[1].fetch_add(as_u64(inlined), Ordering::Relaxed);
        if let Some(column) = fused_refusal.filter(|&c| c < THROWFILL_SHAPE_FUSED_REFUSALS) {
            self.throwfill_shape_counts[2 + column].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// JIT round 16 wave 12 (lane throw3, TP16-14): may an exception crossing
    /// a compiled caller answer the callee exception-table verdict
    /// (`jit::helpers::CalleeTableVerdict`) by the info's identity -- the JIT
    /// cache's retire epoch unchanged since the entry was proved -- before
    /// comparing its three names? `CRATONVM_JIT_CALLEE_VERDICT_IDENTITY=1`
    /// (default OFF until measured), read once per VM.
    pub(crate) fn callee_verdict_identity_enabled(&self) -> bool {
        Self::latched_flag(&self.callee_verdict_identity, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CALLEE_VERDICT_IDENTITY")
        })
    }

    /// JIT round 19 proposal wave (lane throws19, TP16-15): may
    /// `jit::helpers::jit_local_handler_lookup` answer a dispatched throwable
    /// from the site's monomorphic cache on the throwable's class id read
    /// WITHOUT the conservative header validation
    /// (`VmHeap::class_id_of_validated`), and read its debug switch once per
    /// VM? A cache miss still resolves on the validated read, so only a hit
    /// changes. `CRATONVM_JIT_LOCAL_HANDLER_LEAN` (default ON since JIT round
    /// 19, kill word `0`: R18ThrowsShapes pre-local 42 -> 25 ns), read once per VM.
    pub(crate) fn local_handler_lean_enabled(&self) -> bool {
        Self::latched_flag(&self.local_handler_lean, || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LOCAL_HANDLER_LEAN")
        })
    }

    /// TP16-15: `CRATONVM_DBG_LOCAL_HANDLER_LOOKUP`, latched per VM (asked only
    /// by the lean arm of `jit_local_handler_lookup`).
    pub(crate) fn dbg_local_handler_lookup_enabled(&self) -> bool {
        Self::latched_flag(&self.dbg_local_handler_lookup, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOCAL_HANDLER_LOOKUP")
        })
    }

    /// TP16-15 tests: set the lean switch's latch directly (`true` on, `false`
    /// off), as a first read of the variable would.
    #[cfg(test)]
    pub(crate) fn latch_local_handler_lean_for_test(&self, on: bool) {
        use std::sync::atomic::Ordering;
        self.local_handler_lean
            .store(if on { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// JIT round 16 wave 9 (lane throwpath): count one capture-memo outcome
    /// (`CAPTURE_MEMO_*`) under `CRATONVM_DBG_CAPTURE_MEMO=1`, printing the
    /// census every 4096 lookups -- the positive control wave 7's memo never
    /// had (it was never made for a compiled throw, and nothing said so).
    /// Off, one relaxed load.
    pub(crate) fn note_capture_memo(&self, column: usize) {
        use std::sync::atomic::Ordering;
        if !Self::latched_flag(&self.dbg_capture_memo, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CAPTURE_MEMO")
        }) {
            return;
        }
        let Some(counter) = self.capture_memo_counts.get(column) else {
            return;
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if column != CAPTURE_MEMO_HIT && column != CAPTURE_MEMO_MISS {
            return;
        }
        let read = |i: usize| self.capture_memo_counts[i].load(Ordering::Relaxed);
        let (hits, misses) = (read(CAPTURE_MEMO_HIT), read(CAPTURE_MEMO_MISS));
        if (hits + misses) % 4096 == 0 {
            eprintln!(
                "[capture-memo] hits={hits} misses={misses} made={} refused={} raw-hits={} \
                 spliced-hits={}",
                read(CAPTURE_MEMO_MADE),
                read(CAPTURE_MEMO_REFUSED),
                read(CAPTURE_MEMO_RAW_HIT),
                read(CAPTURE_MEMO_SPLICED_HIT)
            );
        }
    }

    /// JIT round 16 (lane throwpath, TP16-2): may `NativeContext::finish_throwable_fill`
    /// store a filled throwable's fields directly? `CRATONVM_THROWABLE_FILL_DIRECT_STORES`
    /// (default ON since round 16, `=0` kill switch; the flag row is
    /// authoritative), read once per VM.
    pub(crate) fn fill_direct_stores_enabled(&self) -> bool {
        use std::sync::atomic::Ordering;
        match self.fill_direct_stores.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_THROWABLE_FILL_DIRECT_STORES",
                );
                self.fill_direct_stores
                    .store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// JIT round 16 wave 7 (lane throwpath, TP16-6): may a throwable capture
    /// reuse one of the thread's recent captures on the same stack
    /// (`stackwalker::ThrowableCaptureMemos`)? `CRATONVM_THROWABLE_CAPTURE_MEMO`,
    /// default ON since 2026-10-07 (the memo keys the launcher's owned frames,
    /// skips the walk on a raw-key hit and backs off where captures do not
    /// repeat; `JkiThrowCpu` new-only 2.0 -> 1.1 us), `=0` off. Read once per VM.
    pub(crate) fn capture_memo_enabled(&self) -> bool {
        use std::sync::atomic::Ordering;
        match self.capture_memo.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_THROWABLE_CAPTURE_MEMO",
                );
                self.capture_memo
                    .store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// JIT round 20 wave 12 (lane qMisc, L4W15-2 of
    /// `jit-r21-nio-proposals-w15-L4.md`): may the capture memo be made for,
    /// and answer, a capture under reflective splices
    /// (`stackwalker::ThrowableCaptureMemo::with_splices`)?
    /// `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES`, default ON, `=0` refuses
    /// such a capture as before. Read once per VM.
    pub(crate) fn capture_memo_splices_enabled(&self) -> bool {
        Self::latched_flag(
            &self.capture_memo_splices,
            crate::runtime::stackwalker::capture_memo_splices_enabled,
        )
    }

    /// `CRATONVM_DBG_CAPTURE_MEMO=1`: should this refusal print its reasons?
    /// The first eight of a VM, so a census that reads `refused=` can be told
    /// why.
    pub(crate) fn capture_memo_refusal_report_due(&self) -> bool {
        use std::sync::atomic::Ordering;
        Self::latched_flag(&self.dbg_capture_memo, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CAPTURE_MEMO")
        }) && self.capture_memo_counts[CAPTURE_MEMO_REFUSAL_REPORTS].fetch_add(1, Ordering::Relaxed)
            < 8
    }

    pub(crate) fn field_slot(&self, field_name: &str) -> Option<&AtomicUsize> {
        match field_name {
            "detailMessage" => Some(&self.detail_message),
            "cause" => Some(&self.cause),
            "suppressedExceptions" => Some(&self.suppressed_exceptions),
            "backtrace" => Some(&self.backtrace),
            "depth" => Some(&self.depth),
            "stackTrace" => Some(&self.stack_trace),
            _ => None,
        }
    }
}

/// The `java.lang.Thread` fields [`ThreadLayoutCache`] keeps a slot for, in
/// slot-array order. Any other name is answered by the caller's own walk.
const THREAD_LAYOUT_FIELD_NAMES: [&str; 10] = [
    "holder",
    "target",
    "name",
    "tid",
    "eetop",
    "interrupted",
    "contextClassLoader",
    "inheritableThreadLocals",
    "daemon",
    // Round 15 wave 5 (lane threadvm2): the park path's blocker pin.
    "parkBlocker",
];
/// A [`ThreadLayoutCache`] slot not yet resolved in this VM.
const THREAD_LAYOUT_UNRESOLVED: usize = usize::MAX;
/// A [`ThreadLayoutCache`] slot `java.lang.Thread` does not declare (JDK 21+
/// has no `target`).
const THREAD_LAYOUT_ABSENT: usize = usize::MAX - 1;
/// An empty [`ThreadLayoutCache::above`] way (class id `u32::MAX` is never
/// allocated: it is the stores' "unresolved" sentinel).
const THREAD_LAYOUT_EMPTY_WAY: u64 = u64::MAX;
/// The high half of an [`ThreadLayoutCache::above`] way whose class is NOT a
/// proper `java.lang.Thread` subclass.
const THREAD_LAYOUT_NOT_BELOW: u32 = u32::MAX;
/// Ways in the [`ThreadLayoutCache::above`] memo (a power of two).
const THREAD_LAYOUT_WAYS: usize = 16;

/// Round 15 wave 4 (lane threadvm; TR4W3-1,
/// `jit-r15w3-trace4-proposals-RETIRED-20261003.md`): the per-VM `java.lang.Thread` layout
/// memo behind `NativeHeapAccess::thread_declared_field_slot`, so
/// `lang_misc::thread_field` (the `Thread` natives' "the field
/// `java.lang.Thread` declares" read) and the served `Thread.run` of a
/// subclass stop taking the class-manager read lock twice per superclass
/// level on every call.
///
/// Every answer here is immutable once resolved: a class's superclass chain
/// is fixed from the moment it has an instance (the memo is filled only from
/// a live instance's class; class ids are never reused), and a redefinition
/// cannot change `java.lang.Thread`'s instance-field layout (a schema change
/// is refused). So, as [`ThrowableLayoutCache`], relaxed atomics and no
/// invalidation. The slots belong to ONE `java.lang.Thread` class id (the
/// first seen); a chain reaching another class of that name answers "no
/// memo" and the caller walks.
pub(crate) struct ThreadLayoutCache {
    /// `CRATONVM_THREAD_LAYOUT_CACHE` (default on), latched per VM on first
    /// use: 0 unread, 1 off, 2 on.
    enabled: AtomicU8,
    /// `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS` (default on), latched
    /// the same way (a snapshot flag, so latching changes no answer).
    declared_fields: AtomicU8,
    /// `CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS` (default on), latched
    /// the same way (round 15 wave 7, lane drainC; `r15w3-trace4-review-findings`
    /// item 4: `vm_exec::thread_run_bottom_frame` read it per hop).
    nested_tasks: AtomicU8,
    /// Direct-mapped memo `class id -> java.lang.Thread's class id above
    /// it`: low half the class id, high half the answer
    /// ([`THREAD_LAYOUT_NOT_BELOW`] for "not a proper subclass"). One 64-bit
    /// word per way, so a reader never sees half of two entries.
    above: [AtomicU64; THREAD_LAYOUT_WAYS],
    /// The `java.lang.Thread` class id `slots` were resolved on.
    thread_class_id: AtomicU32,
    /// Per [`THREAD_LAYOUT_FIELD_NAMES`] entry: its slot,
    /// [`THREAD_LAYOUT_ABSENT`], or [`THREAD_LAYOUT_UNRESOLVED`].
    slots: [AtomicUsize; THREAD_LAYOUT_FIELD_NAMES.len()],
}

impl ThreadLayoutCache {
    pub(crate) fn new() -> Self {
        Self {
            enabled: AtomicU8::new(0),
            declared_fields: AtomicU8::new(0),
            nested_tasks: AtomicU8::new(0),
            above: std::array::from_fn(|_| AtomicU64::new(THREAD_LAYOUT_EMPTY_WAY)),
            thread_class_id: AtomicU32::new(UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID),
            slots: std::array::from_fn(|_| AtomicUsize::new(THREAD_LAYOUT_UNRESOLVED)),
        }
    }

    fn latched(flag: &AtomicU8, name: &str) -> bool {
        use std::sync::atomic::Ordering;
        match flag.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = cratonvm_types::flags::runtime_flag_default_on(name);
                flag.store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// `CRATONVM_THREAD_LAYOUT_CACHE` (default on): `0` answers "no memo"
    /// everywhere, which restores round 15 wave 3's walks exactly.
    pub(crate) fn enabled(&self) -> bool {
        Self::latched(&self.enabled, "CRATONVM_THREAD_LAYOUT_CACHE")
    }

    /// `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS` (default on), per VM.
    pub(crate) fn declared_fields_on(&self) -> bool {
        Self::latched(
            &self.declared_fields,
            "CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS",
        )
    }

    /// `CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS` (default on), per VM.
    pub(crate) fn nested_tasks_on(&self) -> bool {
        Self::latched(
            &self.nested_tasks,
            "CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS",
        )
    }

    /// Whether `class_id` is the `java.lang.Thread` the slots are bound to
    /// (round 15 wave 5, lane threadvm2): its by-name walk and
    /// [`Self::declared_slot`] answer the same slot.
    pub(crate) fn is_thread_class(&self, class_id: ClassId) -> bool {
        use std::sync::atomic::Ordering;
        self.thread_class_id.load(Ordering::Relaxed) == class_id.as_u32()
    }

    fn way(class_id: ClassId) -> usize {
        class_id.as_u32() as usize & (THREAD_LAYOUT_WAYS - 1)
    }

    /// The memoised "`java.lang.Thread` above `class_id`" answer: `None` on
    /// a miss, `Some(None)` for a class that is not a proper subclass.
    pub(crate) fn thread_class_above(&self, class_id: ClassId) -> Option<Option<ClassId>> {
        use std::sync::atomic::Ordering;
        let word = self.above[Self::way(class_id)].load(Ordering::Relaxed);
        if word == THREAD_LAYOUT_EMPTY_WAY || word as u32 != class_id.as_u32() {
            return None;
        }
        let answer = (word >> 32) as u32;
        Some((answer != THREAD_LAYOUT_NOT_BELOW).then_some(ClassId::new(answer)))
    }

    /// Remember [`Self::thread_class_above`]'s answer for `class_id`,
    /// replacing whatever shared its way.
    pub(crate) fn remember_thread_class_above(&self, class_id: ClassId, above: Option<ClassId>) {
        use std::sync::atomic::Ordering;
        if class_id.as_u32() == u32::MAX {
            return;
        }
        let high = above.map_or(THREAD_LAYOUT_NOT_BELOW, ClassId::as_u32);
        let word = (u64::from(high) << 32) | u64::from(class_id.as_u32());
        self.above[Self::way(class_id)].store(word, Ordering::Relaxed);
    }

    /// The slot of `field_name` on `thread_class` (`Some(None)`: declared
    /// nowhere on it), resolving it once with `resolve`. `None` ("no memo"):
    /// a name this cache does not keep, or a `thread_class` other than the
    /// one the slots belong to.
    pub(crate) fn declared_slot(
        &self,
        thread_class: ClassId,
        field_name: &str,
        resolve: impl FnOnce() -> Option<usize>,
    ) -> Option<Option<usize>> {
        use std::sync::atomic::Ordering;
        let index = THREAD_LAYOUT_FIELD_NAMES
            .iter()
            .position(|n| *n == field_name)?;
        let bound = self.thread_class_id.load(Ordering::Relaxed);
        if bound != thread_class.as_u32() {
            if bound != UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID {
                return None;
            }
            if let Err(other) = self.thread_class_id.compare_exchange(
                UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID,
                thread_class.as_u32(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                if other != thread_class.as_u32() {
                    return None;
                }
            }
        }
        let slot = &self.slots[index];
        match slot.load(Ordering::Relaxed) {
            THREAD_LAYOUT_UNRESOLVED => {
                let resolved = resolve();
                slot.store(resolved.unwrap_or(THREAD_LAYOUT_ABSENT), Ordering::Relaxed);
                Some(resolved)
            }
            THREAD_LAYOUT_ABSENT => Some(None),
            cached => Some(Some(cached)),
        }
    }
}

/// Java threads: registry, monitors, virtual-thread scheduling and the main ThreadGroup.
pub struct ThreadRealm {
    /// Captured Throwable backtraces, shared by every Java thread in this VM.
    ///
    /// Java exposes a Throwable's stack trace independently of the thread that
    /// filled it in. Keeping this in `SharedVm` also lets the registry survive
    /// the producer thread's teardown. Entries are non-owning and swept after
    /// each collection, so completed exception-heavy workloads do not retain
    /// stale frame vectors indefinitely.
    pub(crate) throwable_stacks: Vec<RwLock<ThrowableStackShard>>,

    /// Per-VM slots used by Throwable and StackTraceElement native helpers.
    pub(crate) throwable_layout_cache: ThrowableLayoutCache,

    /// Per-VM `java.lang.Thread` layout memo (round 15 wave 4, lane
    /// threadvm; TR4W3-1).
    pub(crate) thread_layout_cache: ThreadLayoutCache,

    /// Monitor table for JVM intrinsic locks (monitorenter/monitorexit).
    pub monitors: MonitorTable,

    /// Cached "main" `java.lang.ThreadGroup` object used by every
    /// VM-created `Thread` as `holder.group`.  Lazily created the first
    /// time `NativeContextImpl::current_thread_object` builds a Thread
    /// in real-JDK mode — the ThreadGroup constructor invokes bytecode,
    /// so it can't run during `SharedVm::new`.
    pub main_thread_group: RwLock<Option<ObjectRef>>,

    /// Claim/wait coordination for the lazy `main_thread_group` build
    /// (`NativeContextImpl::get_or_create_main_thread_group`,
    /// `vm/src/vm/vm_exec.rs`). Mirrors `class_init_waiters`'s JVMS §5.5
    /// claim/wait/notify shape: exactly one thread transitions
    /// `Idle -> InProgress` and performs the (allocating, bytecode-running)
    /// build; every other concurrent caller blocks on the `InProgress`
    /// waiter's condvar instead of redundantly building its own
    /// `ThreadGroup` pair. Fixes a confirmed TOCTOU race (see
    /// CRATONVM-SPRING-GENUINE-BUGLIST, "5.8
    /// follow-up #4") where several `cratonvm-aio-dispatch-N` threads
    /// lazily building their first `Thread` mirror at the same instant
    /// could each observe `main_thread_group == None` and independently
    /// race through the whole unguarded build, corrupting whichever
    /// build's objects lost the final write.
    pub main_thread_group_init: parking_lot::Mutex<MainThreadGroupInit>,

    /// Registry of all JVM threads (main + spawned).
    pub thread_registry: ThreadRegistry,

    /// Virtual thread scheduler — bounds concurrent virtual thread execution
    /// to a carrier-thread pool (JEP 444, Java 21).
    pub virtual_scheduler: Arc<crate::threading::VirtualThreadScheduler>,

    /// Continuation scheduler and heap-resident virtual-thread execution
    /// states. Unlike `virtual_scheduler`'s legacy permit semaphore, this
    /// manager owns a bounded set of carrier OS threads.
    pub virtual_thread_manager: Arc<crate::threading::VirtualThreadManager>,
}

#[cfg(test)]
mod tests {
    use super::{ThrowableLayoutCache, UNRESOLVED_THROWABLE_FIELD_INDEX};
    use std::sync::atomic::Ordering;

    #[test]
    fn throwable_layout_slots_are_not_shared_between_vms() {
        let first = ThrowableLayoutCache::new();
        let second = ThrowableLayoutCache::new();
        first.detail_message.store(7, Ordering::Relaxed);

        assert_eq!(first.detail_message.load(Ordering::Relaxed), 7);
        assert_eq!(
            second.detail_message.load(Ordering::Relaxed),
            UNRESOLVED_THROWABLE_FIELD_INDEX,
            "a second VM must not inherit the first VM's Throwable layout"
        );
    }

    /// Round 15 wave 4 (lane threadvm; TR4W3-1): the `Thread` layout memo
    /// answers what it was told, per class, per VM, and refuses what it
    /// cannot answer exactly.
    #[test]
    fn thread_layout_memo_answers_per_class_and_refuses_a_second_thread_class() {
        use super::ThreadLayoutCache;
        use cratonvm_types::ClassId;
        let cache = ThreadLayoutCache::new();
        let thread = ClassId::new(7);
        let sub = ClassId::new(40);
        // 40 + 16 shares 40's way: it evicts, never aliases.
        let rival = ClassId::new(56);
        assert_eq!(cache.thread_class_above(sub), None);
        cache.remember_thread_class_above(sub, Some(thread));
        assert_eq!(cache.thread_class_above(sub), Some(Some(thread)));
        assert_eq!(cache.thread_class_above(rival), None);
        cache.remember_thread_class_above(rival, None);
        assert_eq!(cache.thread_class_above(rival), Some(None));
        assert_eq!(cache.thread_class_above(sub), None);

        let mut resolved = 0;
        let mut resolve = |slot: Option<usize>| {
            resolved += 1;
            slot
        };
        assert_eq!(
            cache.declared_slot(thread, "holder", || resolve(Some(5))),
            Some(Some(5))
        );
        assert_eq!(
            cache.declared_slot(thread, "holder", || resolve(Some(99))),
            Some(Some(5))
        );
        assert_eq!(
            cache.declared_slot(thread, "target", || resolve(None)),
            Some(None)
        );
        assert_eq!(
            cache.declared_slot(thread, "target", || resolve(Some(1))),
            Some(None)
        );
        assert_eq!(resolved, 2);
        // Not a kept name, or another `java.lang.Thread` id: "no memo".
        assert_eq!(cache.declared_slot(thread, "priority", || Some(3)), None);
        assert_eq!(
            cache.declared_slot(ClassId::new(8), "holder", || Some(3)),
            None
        );
        // A second VM starts empty.
        let other = ThreadLayoutCache::new();
        assert_eq!(other.thread_class_above(sub), None);
        assert_eq!(
            other.declared_slot(ClassId::new(8), "holder", || Some(3)),
            Some(Some(3))
        );
    }
}

#[cfg(test)]
mod jr20_retained_trace_tests {
    use super::{ThrowableLayoutCache, RETAINED_TRACE_FRAMES_GC_BUDGET};

    /// JIT round 20 (lane throw): the registry's budget asks once per budget
    /// of stored frames, consumes the count when it asks, and a sweep resets it.
    #[test]
    fn retained_trace_pressure_asks_once_per_budget() {
        let cache = ThrowableLayoutCache::new();
        assert!(!cache.take_retained_trace_pressure());
        cache.note_retained_trace_frames(RETAINED_TRACE_FRAMES_GC_BUDGET - 1);
        assert!(!cache.take_retained_trace_pressure());
        cache.note_retained_trace_frames(1);
        assert!(cache.take_retained_trace_pressure());
        assert!(!cache.take_retained_trace_pressure(), "consumed");
        cache.note_retained_trace_frames(RETAINED_TRACE_FRAMES_GC_BUDGET - 1);
        cache.reset_retained_trace_frames();
        cache.note_retained_trace_frames(1);
        assert!(
            !cache.take_retained_trace_pressure(),
            "a sweep reset the count"
        );
    }
}

#[cfg(test)]
mod r21_throws_census_tests {
    use super::ThrowableLayoutCache;

    fn skipped_with(switch: Option<&str>, stats: Option<&str>) -> bool {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_THROW_CENSUS_STATS_ONLY", switch),
                ("CRATONVM_DBG_JIT_METHOD_STATS", stats),
            ],
            || ThrowableLayoutCache::new().throw_census_skipped(),
        )
    }

    /// JIT round 21 wave 2 (lane throws, THROWS21-1): the compiled-catch census
    /// is skipped only with the opt-in switch on AND the method-stats line (its
    /// only reader) off; the answer is latched per VM.
    #[test]
    fn census_is_skipped_only_with_the_switch_and_without_method_stats() {
        assert!(!skipped_with(None, None), "opt-in: off by default");
        assert!(skipped_with(Some("1"), None));
        assert!(
            !skipped_with(Some("1"), Some("1")),
            "the stats line still gets its counts"
        );
        let cache = ThrowableLayoutCache::new();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_THROW_CENSUS_STATS_ONLY", Some("1"))],
            || assert!(cache.throw_census_skipped()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_THROW_CENSUS_STATS_ONLY", None)],
            || assert!(cache.throw_census_skipped(), "latched per VM"),
        );
    }
}
