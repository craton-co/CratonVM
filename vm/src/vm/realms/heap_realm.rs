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
    /// Concurrent old-gen cycle phase state, shared with the heap for the
    /// `satb_barrier` `is_marking_active()` fast-path gate (see
    /// `concurrent_satb`).
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
    pub var_handle_roots: RwLock<FxHashMap<i32, ObjectRef>>,

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

    /// T19.3.G1 — number of TLAB fast-path (bump-only) allocations.
    ///
    /// Incremented on every successful `thread.tlab.alloc()` that did
    /// not need a refill. Ratio to [`tlab_refill_count`] must stay
    /// above ~100:1 for a healthy hot allocation loop; drops below 10:1
    /// when the adaptive sizer is mis-tuned.
    pub tlab_hit_count: std::sync::atomic::AtomicU64,

    /// T19.3.G1 — number of minor/major GC cycles completed.
    ///
    /// Incremented by the interpreter's `maybe_gc` / `maybe_gc_forced`
    /// helpers after a collection finishes. Used by the
    /// allocation-storm regression tests to assert that GC frequency
    /// stays below the 0.2 Hz target under synthetic 25 MB/s load.
    pub gc_cycle_count: std::sync::atomic::AtomicU64,

    /// Consecutive allocation-failure GCs that freed almost nothing (post-GC
    /// heap still ≥98% full). When this reaches the GC-overhead limit
    /// (`runtime::interpreter::gc_overhead_limit_exceeded`), the allocation
    /// paths surface a catchable `OutOfMemoryError` (the pre-allocated
    /// `singleton_oom`) instead of spinning in an O(n²) GC death-spiral on a
    /// heap that is full of live (retained) objects. Reset to 0 by any
    /// productive forced GC. Mirrors HotSpot's `UseGCOverheadLimit`.
    pub gc_unproductive_streak: std::sync::atomic::AtomicU32,

    /// T19.3.G1 — total bytes allocated across all TLAB and
    /// slow-path heap allocations since VM start.
    ///
    /// Sampled by diagnostics and written to `--verbose:gc` after
    /// each cycle so operators can compute allocation rate.
    pub bytes_allocated_total: std::sync::atomic::AtomicU64,
}
