// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object heap, GC coordination and the VM-wide GC root tables.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.mem.<field>`.

use crate::memory::vm_heap::{G1ConfigOverrides, GcBackend, VmHeap};
use crate::runtime::lock_order::{LockLevel, OrderedPlMutex, OrderedPlRwLock};
use crate::threading::gc_barrier::GcBarrier;
use crate::types::{ObjectRef, Value};
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// Object heap, GC coordination and the VM-wide GC root tables.
pub struct HeapRealm {
    /// Object/array heap — generational GC with young + old gen.
    pub heap: VmHeap,

    /// fork6 GC_STRESS fix — SATB queue shared between the heap's write
    /// barrier (`satb_barrier`, attached via `enable_concurrent_gc` at
    /// construction) and every concurrent old-gen cycle's marker
    /// (`ConcurrentMarker::with_shared` in `maybe_concurrent_gc`). Without
    /// this shared instance the barrier logs nowhere and remark drains
    /// nothing — the concurrent mark had no write barrier.
    pub concurrent_satb: std::sync::Arc<cratonvm_gc::SatbQueue>,
    /// Concurrent old-gen cycle phase state, shared with every cycle's marker
    /// (`ConcurrentMarker::with_shared`) and read by the root builders
    /// (`generational_concurrent_mark_open`). The heap's `satb_barrier` gates
    /// on `concurrent_satb`, not on this (gc-common w5-e).
    pub concurrent_gc_state: std::sync::Arc<cratonvm_gc::ConcurrentGcState>,

    /// T10 — pool for reusing operand stack Vec<u64> allocations.
    pub operand_stack_pool: crate::runtime::alloc_fastpath::VecPool<u64>,

    /// T10 — pool for reusing tag Vec<u8> allocations.
    pub tag_pool: crate::runtime::alloc_fastpath::VecPool<u8>,

    /// Interned string pool: maps Rust strings to Java String ObjectRefs.
    /// Used by `ldc` string constants and `String.intern()`.
    /// T10.9.B: FxHashMap — keys come from `ldc` constant-pool strings and
    /// internal `String.intern()` calls, not untrusted runtime input.
    pub string_pool: RwLock<FxHashMap<String, ObjectRef>>,

    /// Pre-allocated singleton `java.lang.OutOfMemoryError`, thrown when the
    /// heap is too full to even materialize a fresh exception object (the
    /// OOM-during-OOM case — see `runtime::exceptions::ensure_singleton_oom`,
    /// which fills this once before user `main`). Kept alive permanently by the
    /// GC root scan (`memory::roots`). `None` until pre-allocated; the OOM-throw
    /// sites fall back to their prior behaviour while it is empty.
    pub singleton_oom: RwLock<Option<ObjectRef>>,

    /// The preallocated `OutOfMemoryError`s for the messages other than `Java
    /// heap space` (which stays [`Self::singleton_oom`]): HotSpot's
    /// `Universe::out_of_memory_error_array_size` / `_metaspace`. Filled by
    /// `runtime::exceptions::ensure_singleton_oom` only under the opt-in
    /// `CRATONVM_GC_PREALLOCATED_OOME_KINDS`; empty otherwise, and every
    /// fallback then throws the singleton as before. Rooted by
    /// `memory::roots` step 8c and remapped by `memory::gc` step 6c together
    /// with the singleton. gcd d2/j (2026-09-27), item 4 of
    /// `gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928`.
    pub preallocated_oome: RwLock<PreallocatedOome>,

    /// B-J: permanent GC-root registry for `java.lang.invoke.VarHandle` objects.
    /// VarHandles are long-lived singletons stored in `static final` fields
    /// (e.g. `ConcurrentLinkedDeque.NEXT`) and used for lock-free CAS. They were
    /// not being traced as live, so a moving GC left their static holder slots
    /// pointing at a zeroed (all-zero header) location → `VarHandle.set` /
    /// `compareAndSet` misdispatched onto `java/lang/Object` and the heap
    /// appeared corrupt (kafka consumer-suite crashes — see B-J). Registering
    /// each VarHandle here gets it copied into the GC pointer-map, which lets
    /// the existing static-field remap fix the holder slot. Keyed by identity
    /// hash (stable across moves); values are remapped in `update_all_roots`.
    ///
    /// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
    /// rank 2): a bucket per hash, never a single slot. Two live objects of
    /// one VM can share an identity hash, and a second registration used to
    /// OVERWRITE the first, unrooting it. See [`VarHandleRoots`].
    pub var_handle_roots: RwLock<VarHandleRoots>,

    /// GC barrier for stop-the-world coordination across threads.
    pub gc_barrier: GcBarrier,

    /// Reference processor for weak/soft/phantom reference tracking during GC.
    ///
    /// L7 in the global lock hierarchy — see [`crate::runtime::lock_order`].
    /// [`OrderedPlMutex`] is a drop-in for `parking_lot::Mutex` that asserts the
    /// descending acquisition order on every `.lock()`.
    pub ref_processor: OrderedPlMutex<cratonvm_gc::ReferenceProcessor>,

    /// Finalizer thread queue — objects with `finalize()` overrides are enqueued
    /// here when the GC determines they are unreachable.  The VM drains this
    /// queue and invokes each object's `finalize()` method (JLS §12.6).
    pub finalizer_thread: cratonvm_gc::reference::FinalizerThread,

    /// Cleaner thread queue — Cleaner actions are enqueued when the associated
    /// referent becomes unreachable.  The VM drains and executes them.
    pub cleaner_thread: cratonvm_gc::reference::CleanerThread,

    /// Flag to request an explicit GC at next safepoint (set by System.gc() / GC.run).
    pub gc_requested: std::sync::atomic::AtomicBool,

    /// `-verbose:gc`, as it is NOW: seeded from `VmConfig::verbose_gc` and
    /// switched by `MemoryMXBean.setVerbose` (`NativeSystemAccess::set_verbose_gc`).
    /// The per-pause `[GC] pause:` lines read this, not the immutable launch
    /// config (gc-common w18-g).
    pub verbose_gc: std::sync::atomic::AtomicBool,

    /// A native array allocation crossed the occupancy threshold. The next
    /// native-call boundary collects after pinning and remapping its arguments.
    pub native_array_gc_requested: std::sync::atomic::AtomicBool,

    /// T19.3.G1 — number of TLAB refills across all threads since VM start.
    ///
    /// Incremented inside `tlab_alloc_object` in the interpreter each
    /// time a thread exhausts its current TLAB and requests a new
    /// buffer from the shared arena. Visible to `--verbose:gc` and
    /// used by the allocation-storm regression tests to prove that
    /// the adaptive sizer is keeping refill pressure below the
    /// Quarkus static-init baseline (~330 Hz on the old 64 KB TLAB).
    pub tlab_refill_count: std::sync::atomic::AtomicU64,

    /// T19.3.G1 — number of minor/major GC cycles completed.
    ///
    /// Incremented by the interpreter's `maybe_gc` / `maybe_gc_forced`
    /// helpers after a collection finishes. Used by the
    /// allocation-storm regression tests to assert that GC frequency
    /// stays below the 0.2 Hz target under synthetic 25 MB/s load.
    pub gc_cycle_count: std::sync::atomic::AtomicU64,

    /// Sum of every collection's sealed duration, nanoseconds, as the GC event
    /// plumbing measures it (`gc_events.rs::gc_event_finish`) --
    /// backend-independent. `getCollectionTime()` reads it where the collector
    /// keeps no pause sum of its own (ZGC). gc-common w6-f
    /// (`handoff-w6f-zgc-collection-time-from-the-pause-events`).
    pub gc_pause_ns_total: std::sync::atomic::AtomicU64,

    /// Completed full collections of the `System.gc()` and metaspace doors
    /// (`GcDoor::SystemGc`, `GcDoor::MetadataThreshold`), bumped by
    /// `run_collection_pause` inside the pause, before the release. A
    /// `System.gc()` that lost its STW request reads it to coalesce with a
    /// sibling `System.gc()` that completed after the call began (HotSpot's
    /// `VM_GC_Operation::skip_operation`), instead of running another full
    /// collection. Per VM: another VM's `System.gc()` satisfies nothing here.
    /// gc-common w7-g (`common-e-small-findings` item 7).
    pub system_gc_collections: std::sync::atomic::AtomicU64,

    /// ZGC's JMX beans (`ZGC Cycles` / `ZGC Pauses`, the `ZHeap` pool) and
    /// their GC-notification queue, booked by the common GC event plumbing
    /// (`gc_events.rs`). Read through `HeapRealm::backend_gc_beans` only;
    /// unused on the other backends (G1 keeps its own in `G1State`,
    /// Generational in its heap). Here, not in the heap, because
    /// `VmHeap::Zgc` holds an `Arc<ZgcRealHeap>` the dispatcher cannot
    /// extend. gc-common w7-f.
    pub zgc_gc_beans: std::sync::Arc<cratonvm_gc::gc_metrics::BackendGcBeans>,

    /// Consecutive allocation-failure GCs that freed almost nothing (post-GC
    /// heap still ≥98% full). When this reaches the GC-overhead limit
    /// (`runtime::interpreter::gc_overhead_limit_exceeded`), the allocation
    /// paths surface a catchable `OutOfMemoryError` (the pre-allocated
    /// `singleton_oom`) instead of spinning in an O(n²) GC death-spiral on a
    /// heap that is full of live (retained) objects. Reset to 0 by any
    /// productive forced GC. Mirrors HotSpot's `UseGCOverheadLimit`.
    pub gc_unproductive_streak: std::sync::atomic::AtomicU32,

    /// T19.3.G1 — total bytes allocated across all TLAB and slow-path heap
    /// allocations since VM start. TLAB observations are coalesced per thread
    /// and flushed at 64 KiB or before a refill/wedge decision, rather than
    /// making this shared cache line bounce for every object.
    ///
    /// Sampled by diagnostics and written to `--verbose:gc` after
    /// each cycle so operators can compute allocation rate.
    pub bytes_allocated_total: std::sync::atomic::AtomicU64,

    /// Every Java thread's PUBLISHED allocation total, for this VM only: what
    /// `ThreadMXBean.getTotalThreadAllocatedBytes` sums (the reader adds the
    /// calling thread's unpublished bytes, `Tlab::vm_thread_unpublished_bytes`).
    /// Fed by each thread's TLAB retires, and by its non-TLAB allocations in
    /// 64 KiB batches, through the pointer
    /// `Tlab::attach_vm_thread_allocation_total` installs at the TLAB refill
    /// (and `ensure_vm_thread_allocation_total` at the non-TLAB sites).
    /// gc-common w6-c: it replaced the process static
    /// `tlab::PROCESS_ALLOCATED_BYTES`, so VM A's total no longer grows while
    /// A is idle and VM B allocates, and threads of different VMs no longer
    /// share its cache line.
    pub thread_allocated_total: std::sync::atomic::AtomicU64,

    /// The JIT refill wedge breaker's and the refill-time young trigger's
    /// counters (`runtime/interpreter/gc_and_alloc.rs`), per VM: another
    /// VM's refills must not re-arm this VM's `needs_gc` consult or fire its
    /// forced collection. Until gc-common w6-c these were four process
    /// statics.
    pub tlab_wedge: TlabWedgeState,

    /// What the previous allocation-failure collection left behind, for the
    /// mutator-progress half of the GC-overhead limit
    /// (`runtime::interpreter::note_gc_productivity`). Per VM.
    /// gen r4w5/thrash5 (2026-09-24).
    pub gc_progress: GcProgressMarks,

    /// The allocation-failure ladder's per-VM state: the futile-young-cycle
    /// backoff of the object doors and the `jit_alloc_oom` fail-closed
    /// counter (`runtime::interpreter::gc_and_alloc`). gcd d2/j (2026-09-27).
    pub alloc_ladder: AllocLadderState,
}

/// See [`HeapRealm::alloc_ladder`]. Heuristic state, `Relaxed` throughout: a
/// lost update between two racing doors costs at most one extra (or one
/// skipped) forced young cycle.
#[derive(Default)]
pub struct AllocLadderState {
    /// Consecutive allocation-door forced young cycles after which young still
    /// could not serve the object that missed (`0`: the backoff is idle). Set
    /// by `note_forced_young_cycle_outcome`, read by
    /// `futile_young_backoff_skips` (`gc_and_alloc.rs`).
    pub futile_young_streak: AtomicU32,
    /// [`HeapRealm::bytes_allocated_total`] at the last futile verdict: the
    /// backoff re-arms the forced cycle after a quantum of allocation since.
    pub futile_young_alloc_mark: std::sync::atomic::AtomicU64,
    /// Door entries whose forced young cycle the backoff skipped since the
    /// last verdict: the entry-counted re-arm, for a door whose allocation
    /// does not reach the byte counter.
    pub futile_young_skipped_since: std::sync::atomic::AtomicU64,
    /// Census: futile verdicts over the VM's life.
    pub futile_young_verdicts: std::sync::atomic::AtomicU64,
    /// Census: forced young cycles the backoff skipped over the VM's life.
    pub futile_young_skips: std::sync::atomic::AtomicU64,
    /// `jit_alloc_oom` reached with no JIT thread to publish the
    /// `OutOfMemoryError` on (item 2 of
    /// `gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928`): the compiled
    /// caller then takes its exception exit with nothing pending. By
    /// construction unreachable; counted, and the first one per VM printed.
    pub jit_alloc_oom_without_thread: std::sync::atomic::AtomicU64,
    /// gcd d4/j: a heap `OutOfMemoryError` was raised and the native funnel
    /// has not collected for it yet. Set at the chokepoint every heap-OOME
    /// door passes (`maybe_dump_heap_on_oom_for`); read by the native funnel,
    /// which then collects before its callback (`native_call_owes_oome_major`).
    ///
    /// gcd d5/q: one packed word, `0` = nothing owed, else the RAISING
    /// thread's key (`ThreadId + 1`) above two bits of remaining funnel
    /// payments (`gc_and_alloc::oome_debt_decision`). Only the raising thread
    /// pays -- a JDK daemon's native call used to take the debt at a random
    /// moment, before the program had dropped anything -- and a payment that
    /// left the heap still full keeps one more for the call after the drop.
    pub oome_native_debt: std::sync::atomic::AtomicU64,
    /// gce e2/o: graced attempts the latched GC-overhead exit has spent in the
    /// current latched episode (`gc_and_alloc::latched_overhead_grace`).
    /// Cleared whenever the streak resets.
    pub latched_grace_used: AtomicU32,
}

/// A VM-raised `OutOfMemoryError` message with a preallocated default of its
/// own ([`HeapRealm::preallocated_oome`]). `Java heap space` is not one: it is
/// [`HeapRealm::singleton_oom`], unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OomeKind {
    /// `Requested array size exceeds VM limit`
    /// (`Universe::out_of_memory_error_array_size`).
    ArraySize,
    /// `Metaspace` (`Universe::out_of_memory_error_metaspace`).
    Metaspace,
}

impl OomeKind {
    /// Every kind, in slot order.
    pub const ALL: [OomeKind; 2] = [OomeKind::ArraySize, OomeKind::Metaspace];

    /// The Java-visible detail message (HotSpot's text).
    pub fn message(self) -> &'static str {
        match self {
            OomeKind::ArraySize => crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT,
            OomeKind::Metaspace => "Metaspace",
        }
    }

    /// The kind whose message is exactly `message`, if any. A site-detailed
    /// `Java heap space (...)` and every other text answer `None`.
    pub fn for_message(message: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.message() == message)
    }

    fn slot(self) -> usize {
        match self {
            OomeKind::ArraySize => 0,
            OomeKind::Metaspace => 1,
        }
    }
}

/// See [`HeapRealm::preallocated_oome`]: one slot per [`OomeKind`].
#[derive(Default)]
pub struct PreallocatedOome {
    defaults: [Option<ObjectRef>; 2],
}

impl PreallocatedOome {
    /// The preallocated default for `kind`, if one was built.
    pub fn get(&self, kind: OomeKind) -> Option<ObjectRef> {
        self.defaults[kind.slot()]
    }

    /// Install `kind`'s default.
    pub fn set(&mut self, kind: OomeKind, obj: ObjectRef) {
        self.defaults[kind.slot()] = Some(obj);
    }

    /// Every built default, for the root scan (`memory/roots.rs`, step 8c).
    pub fn values(&self) -> impl Iterator<Item = &ObjectRef> + '_ {
        self.defaults.iter().flatten()
    }

    /// Every built default, for the post-move remap (`memory/gc.rs`, 6c).
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut ObjectRef> + '_ {
        self.defaults.iter_mut().flatten()
    }
}

/// See [`HeapRealm::gc_progress`]. Written by `note_gc_productivity` at the end
/// of every allocation-failure collection this VM's threads initiate, read by
/// the next one. Heuristic state: `Relaxed` throughout, and a lost update
/// between two racing forced collections costs at most one cycle's verdict.
#[derive(Default)]
pub struct GcProgressMarks {
    /// `false` until the first forced collection has set the three marks
    /// below; the progress half abstains until then.
    pub armed: std::sync::atomic::AtomicBool,
    /// [`HeapRealm::bytes_allocated_total`] after that collection.
    pub bytes_allocated_mark: std::sync::atomic::AtomicU64,
    /// [`HeapRealm::thread_allocated_total`] after that collection.
    pub thread_allocated_mark: std::sync::atomic::AtomicU64,
    /// The live-bytes estimate that collection left (young live + old used).
    pub live_after_mark: std::sync::atomic::AtomicU64,
}

/// The permanent-root registry behind `NativeContext::register_var_handle_root`
/// / `read_var_handle_root` ([`HeapRealm::var_handle_roots`]).
///
/// gc-common w29-d. The registry used to be `FxHashMap<i32, ObjectRef>` keyed
/// by identity hash, and `register` was a plain `insert`. An identity hash is
/// 32 bits minted from a wrapping per-heap counter, so two LIVE objects of one
/// VM can share one; the second registration then replaced the first. The first
/// object lost its permanent root (a native still caching it read a reclaimed
/// slot after the next collection) and every read of its key answered the
/// second object.
///
/// Now each hash owns a bucket:
///
/// * [`insert`](Self::insert) APPENDS, and is idempotent per object (compared
///   by address, which the collector keeps current in every entry). Nothing is
///   ever displaced, so every registered object stays rooted.
/// * [`get`](Self::get) answers the FIRST object registered under the hash:
///   the answer a key gave before a collider arrived never changes. A collider
///   registered later cannot be told apart by its hash alone, so its reads
///   answer the first object. That residual is why consumers are moving off
///   `read_var_handle_root` to JNI global handles (`add_global_root` /
///   `resolve_global_root`), which are unique per object.
///
/// The method names match the `HashMap` calls the older call sites used
/// (`insert`, `get`), so those still compile.
#[derive(Default)]
pub struct VarHandleRoots {
    buckets: FxHashMap<i32, smallvec::SmallVec<[ObjectRef; 1]>>,
    /// Objects held across every bucket (for the root-count metrics and tests).
    len: usize,
}

impl VarHandleRoots {
    /// Root `obj` under `key` unless this very object is already filed there.
    /// Returns `true` when a new row was added.
    pub fn insert(&mut self, key: i32, obj: ObjectRef) -> bool {
        let bucket = self.buckets.entry(key).or_default();
        if bucket.iter().any(|held| *held == obj) {
            return false;
        }
        bucket.push(obj);
        self.len += 1;
        true
    }

    /// The first object registered under `key` (see the type doc).
    pub fn get(&self, key: &i32) -> Option<&ObjectRef> {
        self.buckets.get(key).and_then(|bucket| bucket.first())
    }

    /// Every object registered under `key`, oldest first.
    pub fn bucket(&self, key: i32) -> &[ObjectRef] {
        self.buckets.get(&key).map(|b| b.as_slice()).unwrap_or(&[])
    }

    /// Number of rooted objects (not buckets).
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every rooted object, for the root scan (`memory/roots.rs`, step 8b).
    pub fn values(&self) -> impl Iterator<Item = &ObjectRef> + '_ {
        self.buckets.values().flat_map(|bucket| bucket.iter())
    }

    /// Every rooted object, for the post-move remap (`memory/gc.rs`, 6b).
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut ObjectRef> + '_ {
        self.buckets.values_mut().flat_map(|bucket| bucket.iter_mut())
    }
}

/// See [`HeapRealm::tlab_wedge`]. The field names are the old statics' names,
/// lower-cased (`TLAB_GATE_CONSECUTIVE_FAILS` -> `gate_consecutive_fails`,
/// and so on). All four are rate limiters read and written with `Relaxed`
/// ordering, exactly as the statics were.
#[derive(Default)]
pub struct TlabWedgeState {
    /// Consecutive JIT TLAB-refill gate failures, counted by
    /// `tlab_refill_wedge_break` and reset by every successful refill (never
    /// by a gate pass: see the note in `tlab_alloc_shaped_inner`).
    pub gate_consecutive_fails: std::sync::atomic::AtomicU64,
    /// `bytes_allocated_total` at the last forced wedge break; a second break
    /// re-arms only after `WEDGE_REARM_BYTES` of further allocation. 0 means
    /// no break has fired yet.
    pub last_break_alloc_total: std::sync::atomic::AtomicU64,
    /// Slow-path entries since the last refill-time `needs_gc()` fire (the
    /// crumb-treadmill fix). Entry-counted, not byte-counted: the degraded
    /// modes this guards (per-object allocation, crumb-sized mini-TLABs)
    /// enter the slow path orders of magnitude more often than healthy TLAB
    /// flow, so the counter accelerates exactly when the wedge deepens, and a
    /// bytes-based stamp would freeze (the per-object path does not bump
    /// `bytes_allocated_total`).
    pub slowpath_entries_since_gc: std::sync::atomic::AtomicU64,
    /// Bytes handed out by SUCCESSFUL TLAB refills since the last refill-time
    /// `needs_gc()` fire: the healthy-path re-arm metric, OR-ed with the entry
    /// count above. The history (`gen-gc-minor-pause-20260902`: the trigger
    /// was consulted zero times per cycle on healthy JIT flow) is on
    /// `NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES` in `gc_and_alloc.rs`.
    pub refill_bytes_since_gc: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
mod gcd_d2j_preallocated_oome_tests {
    use super::{OomeKind, PreallocatedOome};
    use crate::types::ObjectRef;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: non-null, 8-byte aligned, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Each kind answers exactly HotSpot's message; `Java heap space` (with
    /// or without a site detail) is the singleton's, not a kind.
    #[test]
    fn a_kind_is_found_by_its_exact_message_only() {
        for kind in OomeKind::ALL {
            assert_eq!(OomeKind::for_message(kind.message()), Some(kind));
        }
        assert_eq!(
            OomeKind::for_message("Requested array size exceeds VM limit"),
            Some(OomeKind::ArraySize)
        );
        assert_eq!(OomeKind::for_message("Metaspace"), Some(OomeKind::Metaspace));
        for other in ["Java heap space", "Java heap space (alloc_array length 8)", "", "metaspace"] {
            assert_eq!(OomeKind::for_message(other), None, "{other:?}");
        }
    }

    /// One slot per kind; the root scan and the remap see every built one.
    #[test]
    fn every_built_default_is_scanned_and_remapped() {
        let mut table = PreallocatedOome::default();
        assert!(table.values().next().is_none());
        assert_eq!(table.get(OomeKind::Metaspace), None);
        table.set(OomeKind::Metaspace, fake(0x2000));
        assert_eq!(table.get(OomeKind::Metaspace), Some(fake(0x2000)));
        assert_eq!(table.get(OomeKind::ArraySize), None);
        table.set(OomeKind::ArraySize, fake(0x1000));
        assert_eq!(table.values().count(), 2);
        for slot in table.values_mut() {
            if *slot == fake(0x1000) {
                *slot = fake(0x4_0000_1000);
            }
        }
        assert_eq!(table.get(OomeKind::ArraySize), Some(fake(0x4_0000_1000)));
        assert_eq!(table.get(OomeKind::Metaspace), Some(fake(0x2000)));
    }
}

#[cfg(test)]
mod var_handle_roots_tests {
    use super::VarHandleRoots;
    use crate::types::ObjectRef;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: non-null, 8-byte aligned, never dereferenced (lesson o).
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// gc-common w29-d: two live objects with ONE identity hash. The second
    /// registration used to overwrite the first (unrooting it); both must stay
    /// rooted and the key's answer must stay the first object.
    #[test]
    fn a_same_hash_second_registration_keeps_the_first_rooted() {
        let mut roots = VarHandleRoots::default();
        let a = fake(0x1_0000_1000);
        let b = fake(0x2_0000_1000);
        let key = 0x1000;
        assert!(roots.insert(key, a));
        assert!(roots.insert(key, b));
        assert_eq!(roots.len(), 2);
        assert_eq!(roots.get(&key).copied(), Some(a), "first registrant keeps the key");
        assert_eq!(roots.bucket(key), &[a, b]);
        let scanned: Vec<ObjectRef> = roots.values().copied().collect();
        assert!(scanned.contains(&a) && scanned.contains(&b), "both are roots");
    }

    #[test]
    fn re_registering_one_object_is_idempotent() {
        let mut roots = VarHandleRoots::default();
        let a = fake(0x3000);
        assert!(roots.insert(7, a));
        assert!(!roots.insert(7, a));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots.bucket(7), &[a]);
    }

    /// The remap walks every bucket member, and a re-registration with the
    /// post-move address still dedups against the remapped row.
    #[test]
    fn remap_reaches_every_bucket_member() {
        let mut roots = VarHandleRoots::default();
        let (a, b) = (fake(0x4000), fake(0x5000));
        roots.insert(9, a);
        roots.insert(9, b);
        let (a2, b2) = (fake(0x4_0000_4000), fake(0x4_0000_5000));
        for slot in roots.values_mut() {
            if *slot == a {
                *slot = a2;
            } else if *slot == b {
                *slot = b2;
            }
        }
        assert_eq!(roots.bucket(9), &[a2, b2]);
        assert!(!roots.insert(9, a2), "the moved object is still the same row");
        assert_eq!(roots.len(), 2);
        assert!(roots.get(&10).is_none());
        assert!(!roots.is_empty());
    }
}
