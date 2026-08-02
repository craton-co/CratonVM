// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Unified heap abstraction for the VM.
//!
//! `VmHeap` wraps the available GC implementations (GenerationalHeap, G1Collector,
//! and ZgcRealHeap when enabled) behind a single type, so the VM code doesn't
//! need to be generic or use trait objects.

use crate::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
use crate::g1::{G1Collector, G1CollectorConfig};
use crate::g1_concurrent::ConcurrentMarkController;
use crate::gc::GcResult;
use crate::gen_heap::GenerationalHeap;
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
use crate::old_gen::OldGen;
use crate::satb::SatbQueue;
#[cfg(feature = "zgc")]
use crate::zgc::ZgcRealHeap;
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::Mutex;
use std::sync::Arc;

/// G1 collector + slot for its current concurrent-mark controller.
///
/// Task #56: `VmHeap::g1_start_concurrent_mark` and
/// `VmHeap::g1_signal_marking_complete` are the spawn/join boundary
/// for the background marker. The controller's lifetime is one mark
/// cycle, parked in `Mutex<Option<…>>`. `Deref<Target = G1Collector>`
/// keeps existing `VmHeap::G1(h) => h.method()` dispatch sites compiling
/// unchanged.
pub struct G1State {
    /// `Arc` so the controller's worker thread can hold a clone for
    /// the duration of its cycle.
    pub collector: Arc<G1Collector>,
    /// Active controller, or `None` between cycles.
    concurrent_mark: Mutex<Option<ConcurrentMarkController>>,
}

impl G1State {
    pub fn new(config: G1CollectorConfig) -> Self {
        Self {
            collector: Arc::new(G1Collector::new(config)),
            concurrent_mark: Mutex::new(None),
        }
    }

    /// `true` iff a controller is currently parked. Diagnostics / tests.
    pub fn has_active_controller(&self) -> bool {
        self.concurrent_mark.lock().is_some()
    }
}

impl std::ops::Deref for G1State {
    type Target = G1Collector;
    #[inline]
    fn deref(&self) -> &G1Collector {
        &self.collector
    }
}

/// Which GC backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcBackend {
    /// Generational semi-space + old-gen mark-sweep (default).
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
    /// ZGC-real memory-backed stop-the-world collector.
    #[cfg(feature = "zgc")]
    Zgc,
}

/// Explicit G1 tuning overrides wired from the `-XX:` knobs, applied by
/// [`VmHeap::new_with_overrides`]. `None` keeps the collector default.
#[derive(Debug, Default, Clone, Copy)]
pub struct G1ConfigOverrides {
    /// `-XX:G1HeapRegionSize=<bytes>`.
    pub region_size: Option<usize>,
    /// `-XX:InitiatingHeapOccupancyPercent=<n>` (clamped to 1..=100).
    pub ihop_percent: Option<u8>,
    /// `-XX:MaxGCPauseMillis=<n>`.
    pub max_gc_pause_ms: Option<u64>,
    /// `-XX:±UseStringDeduplication`.
    pub string_dedup: Option<bool>,
}

// ─── GPU-offload coordination (Phase 6 item 1) ───────────────────────────
//
// Process-wide counter tracking the number of live SafepointTokens
// (see `crate::safepoint`). While non-zero, every collect_garbage
// entry point on `GenerationalHeap` / `G1Collector` spin-yields
// instead of running a collection.
//
// Why process-wide rather than per-heap: there is always exactly one
// VmHeap per CratonVM process, so the distinction is academic. A
// `static` keeps the safepoint API zero-cost (no `&Heap` plumbed
// through the `enter_gpu_critical` API).
//
// The legacy `Heap` struct in `heap.rs` keeps its own per-instance
// counter for backwards compatibility with the Phase 1 tests — the
// two paths are independent.

/// Process-wide GPU-critical-section counter. Public so the VM
/// crate can manage the count manually from `runtime::offload` for
/// the Phase 7 deferred-finalize path (which crosses thread
/// boundaries and therefore can't use the `!Send` `SafepointToken`).
#[cfg(feature = "gpu-offload")]
pub static GPU_CRITICAL_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Spin-yield until every live `SafepointToken` has been dropped.
///
/// Called from the GC entry points on `GenerationalHeap` and
/// `G1Collector` before starting a collection cycle, so a kernel
/// running under a token never observes its inputs being moved.
///
/// Emits a tracing warning after [`crate::safepoint::GPU_CRITICAL_DEADLINE_SECS`]
/// seconds so an indefinitely-blocked collector still surfaces in logs.
#[cfg(feature = "gpu-offload")]
pub fn wait_for_gpu_critical_drain() {
    use std::sync::atomic::Ordering;
    if GPU_CRITICAL_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(crate::safepoint::GPU_CRITICAL_DEADLINE_SECS);
    let mut warned = false;
    loop {
        std::thread::yield_now();
        let now = GPU_CRITICAL_COUNT.load(Ordering::Acquire);
        if now == 0 {
            return;
        }
        if !warned && start.elapsed() >= deadline {
            tracing::warn!(
                "GC delayed >{}s by GPU critical section — {} active tokens",
                crate::safepoint::GPU_CRITICAL_DEADLINE_SECS,
                now,
            );
            warned = true;
        }
    }
}

/// No-op when the gpu-offload feature is disabled. Callers in the
/// GC entry points can use this unconditionally.
#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub fn wait_for_gpu_critical_drain() {}

// ─── Task #25: SATB triad-ordering debug assertion ───────────────────────
//
// In debug builds, every `write_barrier_pre` call arms a per-thread
// sentinel that the next `write_barrier` call clears. If `write_barrier`
// observes the sentinel still armed at the start of the *next*
// `write_barrier_pre` (i.e. two pre-barriers with no intervening post),
// it means a store path called `write_barrier_pre` and then forgot to
// follow up with the matching post-store `write_barrier` — the exact
// shape of the SATB-without-post bug. The sentinel is `thread_local`
// and entirely compiled out in release builds, so the production hot
// path is unchanged.

#[cfg(debug_assertions)]
thread_local! {
    static PENDING_PRE_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Opt-out for the young-mirror deferral in [`VmHeap::mirror_pin_deferrable`]
/// (`CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1` restores the old old-gen-only
/// behaviour). Read once — consulted per class mirror per root scan.
#[inline]
fn mirror_pin_young_defer_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER").is_none()
    })
}

#[cfg(debug_assertions)]
#[inline]
fn arm_pending_pre_barrier() {
    PENDING_PRE_BARRIER.with(|f| {
        // If we are arming AGAIN without an intervening `write_barrier`,
        // a previous (pre, store, post) triad was left incomplete. The
        // assertion catches missing post-store calls without crashing
        // release builds.
        debug_assert!(
            !f.get(),
            "write_barrier_pre called twice with no intervening write_barrier — \
             SATB triad order violated; missing post-store barrier somewhere on \
             the prior reference store"
        );
        f.set(true);
    });
}

#[cfg(debug_assertions)]
#[inline]
fn clear_pending_pre_barrier() {
    PENDING_PRE_BARRIER.with(|f| f.set(false));
}

/// Unified heap wrapping either GenerationalHeap or G1Collector.
///
/// Provides the same API surface as `GenerationalHeap` so existing call sites
/// work unchanged. For G1-specific methods that don't apply, reasonable defaults
/// are returned.
pub enum VmHeap {
    Generational(GenerationalHeap),
    G1(G1State),
    #[cfg(feature = "zgc")]
    Zgc(ZgcRealHeap),
}

// Safety: both inner types are already Send + Sync.
unsafe impl Send for VmHeap {}
unsafe impl Sync for VmHeap {}

/// Macro to dispatch a method call to the inner heap implementation.
macro_rules! dispatch {
    ($self:expr, $method:ident ( $($arg:expr),* ) ) => {
        match $self {
            VmHeap::Generational(h) => h.$method($($arg),*),
            VmHeap::G1(h) => h.$method($($arg),*),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.$method($($arg),*),
        }
    };
}

impl VmHeap {
    /// Create a new VmHeap with the specified backend and total capacity.
    pub fn new(backend: GcBackend, total_bytes: usize) -> Self {
        Self::new_with_overrides(backend, total_bytes, G1ConfigOverrides::default())
    }

    /// Like [`Self::new`] but applies explicit G1 tuning overrides (wired from
    /// the `-XX:` knobs: `InitiatingHeapOccupancyPercent`, `G1HeapRegionSize`,
    /// `MaxGCPauseMillis`, `±UseStringDeduplication`). Each `None` keeps the
    /// collector default; an explicit value wins over the heap-size-based
    /// region-size ergonomic. No effect on the generational backend.
    pub fn new_with_overrides(
        backend: GcBackend,
        total_bytes: usize,
        overrides: G1ConfigOverrides,
    ) -> Self {
        match backend {
            GcBackend::Generational => {
                VmHeap::Generational(GenerationalHeap::with_capacity(total_bytes))
            }
            GcBackend::G1 => {
                let mut config = G1CollectorConfig::default();
                config.heap_size = total_bytes;
                // Scale region size: 1 MB for heaps < 4 GB, 2 MB for larger
                if total_bytes > 4 * 1024 * 1024 * 1024 {
                    config.region_size = 2 * 1024 * 1024;
                }
                // Explicit -XX: overrides take precedence over the ergonomic.
                if let Some(rs) = overrides.region_size {
                    if rs > 0 {
                        config.region_size = rs;
                    }
                }
                if let Some(ihop) = overrides.ihop_percent {
                    config.ihop_percent = ihop.clamp(1, 100);
                }
                if let Some(pause) = overrides.max_gc_pause_ms {
                    config.max_gc_pause_ms = pause.max(1);
                }
                if let Some(dedup) = overrides.string_dedup {
                    config.string_dedup_enabled = dedup;
                }
                VmHeap::G1(G1State::new(config))
            }
            #[cfg(feature = "zgc")]
            GcBackend::Zgc => VmHeap::Zgc(ZgcRealHeap::with_capacity(total_bytes)),
        }
    }

    // =====================================================================
    // Core allocation (both backends implement GarbageCollector trait)
    // =====================================================================

    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        dispatch!(self, alloc_object(class_id, num_fields))
    }

    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        dispatch!(self, alloc_array(class_id, element_type, length))
    }

    /// Try to allocate an object. Returns `None` on OOM (caller should trigger GC and retry).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_object(class_id, num_fields),
            VmHeap::G1(h) => h.try_alloc_object(class_id, num_fields),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_object(class_id, num_fields),
        }
    }

    /// Try to allocate directly in the old generation. This is only available
    /// for the generational heap; other heap implementations return `None` so
    /// callers can fall back to their normal allocation path.
    pub fn try_alloc_object_old(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_object_old(class_id, num_fields),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Allocate a same-layout old-generation batch under one allocator lock.
    /// Only the generational backend currently exposes a non-moving old-gen
    /// pool; other backends return an empty batch so callers retain their
    /// ordinary allocation fallback.
    pub fn try_alloc_objects_old_batch(
        &self,
        class_id: ClassId,
        num_fields: usize,
        count: usize,
    ) -> Vec<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_objects_old_batch(class_id, num_fields, count),
            VmHeap::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => Vec::new(),
        }
    }

    /// Fallible twin of [`alloc_object`](Self::alloc_object): same (no-GC)
    /// allocation path including the old-generation spill, but returns `None`
    /// on true heap exhaustion instead of aborting the VM. Lets the JIT
    /// object-alloc helper raise a catchable `OutOfMemoryError`.
    pub fn try_alloc_object_full(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_object_full(class_id, num_fields),
            VmHeap::G1(h) => h.try_alloc_object(class_id, num_fields),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_object(class_id, num_fields),
        }
    }

    /// Fallible twin of [`alloc_array`](Self::alloc_array): same (no-GC)
    /// allocation path, but returns `None` on true heap exhaustion instead of
    /// aborting the VM. Lets native callers (e.g. `ArrayList(int)`) raise a
    /// catchable `OutOfMemoryError` for an over-large backing array.
    pub fn try_alloc_array_full(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_array_full(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        }
    }

    /// DBG (bc math-ec `0x4`): scan young from-space for the first `0x4` seed
    /// slot. See [`GenerationalHeap::dbg_first_young_small_ref`]. G1 unsupported.
    pub fn dbg_first_young_small_ref(&self) -> Option<(usize, u32, usize, usize, u64)> {
        match self {
            VmHeap::Generational(h) => h.dbg_first_young_small_ref(),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Allocate a new Java object and initialize primitive-typed slots to
    /// their spec-mandated typed zero based on JVM field descriptor bytes.
    ///
    /// This is the safe allocation entry point — it fixes the
    /// ConcurrentHashMap.initTable CAS livelock by guaranteeing that an
    /// unwritten `int` slot reads back as `Value::Int(0)` instead of
    /// `Value::Object(None)`. See
    /// [`crate::heap::default_value_for_descriptor`] for the mapping.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        match self {
            VmHeap::Generational(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            VmHeap::G1(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
        }
    }

    /// Fallible variant: returns `None` when the heap is out of space.
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            VmHeap::G1(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
        }
    }

    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_array(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        }
    }

    // =====================================================================
    // Header access
    // =====================================================================

    pub fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        dispatch!(self, get_header(obj))
    }

    pub fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        dispatch!(self, class_id_of(obj))
    }

    /// NEW-1.5 conservative validity check for a *raw stack-spill address*.
    ///
    /// Used by JIT frame root scanning to filter spurious values: returns
    /// `Some(ObjectRef)` only if `addr` lands on a live object header in
    /// this heap. See [`crate::gen_heap::GenerationalHeap::is_object_address`]
    /// for the full contract.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.is_object_address(addr),
            VmHeap::G1(h) => h.is_object_address(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_object_address(addr),
        }
    }

    /// `[lo, hi)` envelope containing every address [`Self::is_object_address`]
    /// can possibly accept, or `None` when the backend cannot cheaply supply
    /// one (ZGC keeps live bases in a registry, not a contiguous arena).
    ///
    /// Purely an optimization hint for conservative stack scanning: a word
    /// outside the envelope is definitely not an object address, so the
    /// caller can skip the full per-word validator. A word inside it still
    /// has to go through `is_object_address`.
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        match self {
            VmHeap::Generational(h) => h.conservative_addr_span(),
            VmHeap::G1(h) => h.conservative_addr_span(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// BUG-03 — whether this heap backend supports the cross-thread STW JIT
    /// TLAB skip-region protocol, i.e. it can collect safely while a frozen
    /// in-JIT peer holds an un-retired TLAB:
    ///
    /// - Generational: the young collection degrades to the non-moving sweep
    ///   that consumes [`GenerationalHeap::set_jit_tlab_skip_regions`].
    /// - G1 (INT-3): the published tails are skipped by every region walker
    ///   and their regions (plus every region holding a conservative frozen-
    ///   peer root — the VM pins those via
    ///   [`crate::gc_quiescence::add_pinned_jit_root`]) are excluded from the
    ///   CSet, so nothing a frozen peer can address moves.
    /// - ZGC (INT-3 residual): trivially safe — `ZgcRealHeap` is a
    ///   non-moving STW mark-sweep whose sweep walks the allocation-base
    ///   REGISTRY (never linear memory), and [`Self::refill_tlab`] never
    ///   hands ZGC mutators a TLAB, so un-retired tails cannot exist. A
    ///   frozen peer's conservative roots are ordinary (pinned-by-design)
    ///   mark roots.
    ///
    /// The collector only engages the forcible in-JIT-peer take-over when
    /// this is `true` — now on every backend.
    pub fn supports_jit_tlab_skip(&self) -> bool {
        true
    }

    /// BUG-03 / INT-3 — publish the reserved TLAB tails of forcibly-stopped
    /// in-JIT peers so the collection skips them (non-moving-sweep skip list
    /// on Generational; region-walker skip + CSet exclusion on G1). No-op on
    /// ZGC, whose mutators never hold TLABs (the published list is always
    /// empty there — see [`Self::supports_jit_tlab_skip`]).
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        match self {
            VmHeap::Generational(h) => h.set_jit_tlab_skip_regions(regions),
            VmHeap::G1(h) => h.set_jit_tlab_skip_regions(regions),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// BUG-03 / INT-3 — clear any published JIT TLAB skip regions.
    pub fn clear_jit_tlab_skip_regions(&self) {
        match self {
            VmHeap::Generational(h) => h.clear_jit_tlab_skip_regions(),
            VmHeap::G1(h) => h.clear_jit_tlab_skip_regions(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// DIAGNOSTIC-ONLY (cceres3, CRATONVM_DBG_BLOCKGC frame-desync hunt): if
    /// `addr` is a heap address whose header is a forwarding marker (an
    /// already-evacuated old address kept readable by the stale-objref
    /// quarantine ring), return the forwarded (new) address. `None` when the
    /// stale-objref canary is off, the address is outside the heap, or the
    /// header is a live header. Generational backend only — the others never
    /// quarantine old addresses.
    pub fn debug_forwarded_target(&self, addr: usize) -> Option<usize> {
        match self {
            VmHeap::Generational(h) => h.debug_forwarded_target(addr),
            _ => None,
        }
    }

    /// DIAGNOSTIC-ONLY (cce0079): minor-GC epoch for the `[SETFIELD-GC]`
    /// assertion. Zero on non-Generational backends.
    pub fn debug_minor_gc_count(&self) -> u64 {
        match self {
            VmHeap::Generational(h) => h.debug_minor_gc_count(),
            _ => 0,
        }
    }

    /// Loose validity check: alignment + heap-region containment.
    ///
    /// Unlike [`Self::is_object_address`] this does NOT read the object
    /// header. Used by operand-stack root scanning to root ambiguous
    /// JVM-long-vs-jobject slots without rejecting legitimate references
    /// whose header is transiently unreadable (interior pointers, mid-
    /// initialisation slots, or stale-bit-pattern Long slots that the GC
    /// is well-equipped to ignore via its own size-sanity guard).
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.is_heap_addr(addr),
            VmHeap::G1(h) => h.is_heap_addr(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_heap_addr(addr),
        }
    }

    /// Diagnostic decomposition of [`Self::is_addr_live`] into its two arms,
    /// plus where the address actually sits.
    ///
    /// `is_addr_live` ORs an old-gen allocation test with a young-survivor
    /// test, so a `false` is indistinguishable between "the old-gen block is on
    /// the free list", "the address is in the wrong semispace" and "it is in
    /// neither generation". Those have completely different causes, and the one
    /// consumer that DESTROYS state on a `false` — the collection-overlay prune
    /// — has to be debugged against the specific arm.
    ///
    /// Returns `(old_gen_allocated, young_survivor, region)`.
    pub fn liveness_arms(&self, addr: usize) -> (bool, bool, &'static str) {
        match self {
            VmHeap::Generational(h) => {
                let old = h.is_live_old_gen_addr(addr);
                let young = h.is_live_young_survivor(addr);
                let region = if h.is_in_old(addr as *const u8) {
                    "old-gen"
                } else if h.is_heap_addr(addr).is_some() {
                    "young"
                } else {
                    "off-heap"
                };
                (old, young, region)
            }
            _ => (false, false, "n/a"),
        }
    }

    /// T1.7.1 — Brooks-pointer read barrier.
    ///
    /// Consults the object's compact header: if the `LockState` is
    /// `Forwarded`, the object has been evacuated by a concurrent
    /// compaction cycle and the real object lives at
    /// `header.forwarding_ptr()`. The barrier self-heals stale
    /// pointers by returning the forwarded address.
    ///
    /// Under stop-the-world GC this is **always a no-op fast path**
    /// because the header's lock state is only set to `Forwarded`
    /// during evacuation, and the collector updates all root
    /// references before resuming mutators. The barrier becomes
    /// meaningful when concurrent compaction is enabled and a
    /// mutator loads a reference *while* the collector is relocating
    /// the target object.
    ///
    /// The fast path is a single load + mask + branch: an empty
    /// inline-able sequence on both x86-64 and AArch64. The slow
    /// path (forwarded) is one additional mask operation on the
    /// 30-bit shifted address.
    ///
    /// Idempotent: calling `load_and_forward` on an already-forwarded
    /// object returns the *terminal* forwarding target in one hop
    /// because Brooks pointers only ever point "forward" (the
    /// evacuation pass never builds chains longer than one link).
    ///
    /// # Safety
    ///
    /// The caller must hold a live root to `obj` (or the GC must be
    /// stopped) so the header read is against a valid object. The
    /// returned `ObjectRef` points into the same heap region.
    ///
    /// # Backend compatibility
    ///
    /// All current `VmHeap` backends (`Heap`, `GenerationalHeap`,
    /// `G1Collector`) allocate objects with the **full** 32-byte
    /// `ObjectHeader` layout — none of them use the compact 64-bit
    /// header format. This barrier therefore reads
    /// `ObjectHeader.forwarding_ptr` directly. Decoding the first 8
    /// bytes of a full header as a `CompactHeader` would alias the
    /// `forwarding_ptr` bit-pattern onto unrelated header fields
    /// (`_padding`, `identity_hash_code`, `array_length`) and could
    /// cause `is_forwarded` to spuriously fire. A backend that adopts
    /// compact headers in the future MUST update this method (and
    /// gate the alternate decode path on the appropriate cfg).
    #[inline]
    pub fn load_and_forward(&self, obj: ObjectRef) -> ObjectRef {
        // SAFETY: the caller guarantees `obj` is a live root. Every
        // current backend lays out `ObjectHeader` at offset 0 of the
        // ObjectRef pointer with `forwarding_ptr` at the documented
        // structural offset; reading the field is well-formed.
        let header = unsafe { &*(obj.as_ptr() as *const crate::heap::ObjectHeader) };
        if !header.is_forwarded() {
            return obj;
        }
        let addr = header.forwarding_address();
        if addr.is_null() {
            // Defensive fallback (shouldn't happen — is_forwarded()
            // already checks for a non-null pointer — but kept for
            // belt-and-braces against concurrent-GC races we did not
            // anticipate). Returning the original pointer is always
            // safe because the original object still exists in memory
            // until the evacuation epoch ends.
            return obj;
        }
        // SAFETY: the forwarding pointer was installed by the GC and
        // points at a valid object header within this heap.
        unsafe { ObjectRef::from_raw(addr) }
    }

    /// Decode the first 8 bytes of an object's header as a compact
    /// 64-bit [`CompactHeader`]. The return is a *copy* of that word so
    /// the caller can inspect it without holding a borrow into the heap.
    ///
    /// NOTE: this is **only** meaningful for a backend that actually
    /// adopts the compact header format. Every current `VmHeap` backend
    /// (`Heap`, `GenerationalHeap`, `G1Collector`) lays objects out with
    /// the full 32-byte [`ObjectHeader`], whose first 8 bytes are
    /// `class_id`/`identity_hash_code` — not a compact header — so this
    /// method has no in-tree callers today. In particular it is **not**
    /// used by [`Self::load_and_forward`], which reads the legacy
    /// `ObjectHeader.forwarding_ptr` field directly (see that method's
    /// backend-compatibility note).
    #[inline]
    pub fn get_compact_header(&self, obj: ObjectRef) -> crate::compact_header::CompactHeader {
        // Read the 64-bit word at offset 0 from the ObjectRef pointer and
        // reinterpret it as a CompactHeader. Only correct once a backend
        // stores compact headers there (see the doc-comment above).
        //
        // SAFETY: ObjectRef is a validated heap address pointing at
        // a live object header. The load is aligned (headers are
        // 8-byte aligned by construction).
        let ptr = obj.as_ptr() as *const u64;
        let raw = unsafe { std::ptr::read(ptr) };
        crate::compact_header::CompactHeader::from_raw(raw)
    }

    pub fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        dispatch!(self, kind_of(obj))
    }

    pub fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        dispatch!(self, element_type_of(obj))
    }

    pub fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        dispatch!(self, identity_hash_code(obj))
    }

    /// H1: mint a fresh non-zero identity hash code for use at object
    /// construction time. Matches the value the slow-path allocators
    /// (`alloc_object` / `alloc_array`) write into the header. The TLAB
    /// fast path in `interpreter::init_object_header` calls this so all
    /// freshly allocated objects have a non-zero `identity_hash_code`
    /// header field, eliminating false-positive "Stale pointer detected"
    /// warnings when a fresh `new Object()` (cid=0, fields=0, hash=0)
    /// otherwise produces an all-zero first 16 bytes that the detector
    /// can't distinguish from genuine stale memory.
    pub fn next_identity_hash(&self) -> i32 {
        dispatch!(self, next_hash())
    }

    // =====================================================================
    // Field access
    // =====================================================================

    pub fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        dispatch!(self, get_field(obj, index))
    }

    pub fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // Task #25: this path fires the inherent write_barrier inside
        // `set_field`, which doesn't go through `VmHeap::write_barrier`
        // and would leave the triad sentinel armed. Clear it here so
        // the (pre, store-via-set_field) sequence closes cleanly.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field(obj, index, value))
    }

    /// INT-8: field store with the G1 SATB pre-barrier SUPPRESSED. Reserved
    /// for the weak-reference PROTOCOL writes (the pre-collection referent
    /// null pass and the remark-time referent clears): those are not
    /// semantic overwrites, and SATB-logging them recorded every active
    /// referent as a mark root — the taint that made bitmap-based reference
    /// processing inert (see `G1Collector::set_field_no_satb`). On the
    /// Generational and ZGC backends this is a plain `set_field`: their
    /// reference protocols never depended on hiding these writes (Gen uses
    /// the watched-referents channel; ZGC processes references against its
    /// own non-moving mark), so no behavior change there.
    pub fn set_field_suppress_satb(&self, obj: ObjectRef, index: usize, value: Value) {
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        match self {
            VmHeap::G1(h) => h.collector.set_field_no_satb(obj, index, value),
            other => dispatch!(other, set_field(obj, index, value)),
        }
    }

    pub fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        dispatch!(self, get_field_volatile(obj, index))
    }

    pub fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        // Task #25: see `set_field` comment — also closes the triad
        // since `set_field_volatile` likewise routes through the
        // inherent write_barrier, bypassing `VmHeap::write_barrier`.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field_volatile(obj, index, value))
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Descriptor-aware get — dispatches to the underlying heap's
    /// [`get_field_as`](crate::collector::GarbageCollector::get_field_as),
    /// which normalizes the returned `Value` against the declared field
    /// type. See the trait-level documentation for the coercion rules.
    pub fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        dispatch!(self, get_field_as(obj, index, desc_byte))
    }

    /// Volatile descriptor-aware get.
    pub fn get_field_volatile_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        dispatch!(self, get_field_volatile_as(obj, index, desc_byte))
    }

    /// Descriptor-aware set.
    pub fn set_field_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        // Like `set_field`, descriptor-aware stores use the collector's
        // inherent barrier path. Close the debug SATB triad sentinel here;
        // otherwise an interpreter/JIT pre-barrier followed by putfield via
        // this typed entry point leaves it armed until the next store.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field_as(obj, index, value, desc_byte))
    }

    /// Volatile descriptor-aware set.
    pub fn set_field_volatile_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        // Same inherent-barrier path and debug-triad closure as
        // `set_field_as` above.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field_volatile_as(obj, index, value, desc_byte))
    }

    // =====================================================================
    // GPU offload — safepoint coordination
    // =====================================================================

    /// Enter a GPU-critical section. Returns a `SafepointToken` whose
    /// lifetime brackets a no-GC window for the calling thread.
    ///
    /// The counter is process-wide (a module-level `AtomicU32`), so
    /// every `VmHeap` instance in the process shares the same gate.
    /// In practice every CratonVM process has exactly one `VmHeap`,
    /// so this is equivalent to per-heap.
    ///
    /// The GC entry points on both `GenerationalHeap` and
    /// `G1Collector` call [`wait_for_gpu_critical_drain`] before
    /// collecting, so a kernel running under this token will not
    /// observe its inputs being moved.
    #[cfg(feature = "gpu-offload")]
    pub fn enter_gpu_critical(&self) -> crate::safepoint::SafepointToken<'static> {
        crate::safepoint::SafepointToken::new(&GPU_CRITICAL_COUNT)
    }

    /// Current number of live `SafepointToken`s. Useful for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_critical_count(&self) -> u32 {
        GPU_CRITICAL_COUNT.load(std::sync::atomic::Ordering::Acquire)
    }

    // =====================================================================
    // Array access
    // =====================================================================

    pub fn array_length(&self, obj: ObjectRef) -> usize {
        dispatch!(self, array_length(obj))
    }

    /// Returns the element type of an array object.
    /// For non-array objects the returned value is meaningless.
    pub fn array_element_type(&self, obj: ObjectRef) -> Option<ArrayElementType> {
        let header = self.get_header(obj);
        if header.kind == ObjectKind::Array {
            Some(header.element_type)
        } else {
            None
        }
    }

    pub fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        dispatch!(self, get_array_element(obj, index))
    }

    pub fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        // Task #25: closes the triad sentinel (same rationale as
        // `set_field`); ref-array stores also route through the
        // inherent write_barrier.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_array_element(obj, index, value))
    }

    /// JNI critical-section region pinning (Step 6 / JEP 423). Pin the G1
    /// region backing `obj` so a moving collection cannot relocate the array
    /// while a native `GetPrimitiveArrayCritical` section holds a detached copy
    /// that must be copied back to *this* object at `Release`. Returns the
    /// pinned region indices for the matching [`Self::unpin_critical_regions`].
    ///
    /// No-op (returns empty) on the generational collector: the copy-back
    /// staleness is reachable in practice only under G1's aggressive young/mixed
    /// evacuation, which is the documented Step 6 target (design doc §3.2.5);
    /// the array's liveness is independently held by the `cratonvm_gc::pinned`
    /// keep-alive set at the JNI call site.
    pub fn pin_critical_region(&self, obj: ObjectRef) -> Vec<usize> {
        match self {
            VmHeap::Generational(_) => Vec::new(),
            VmHeap::G1(h) => h
                .pin_region_for_addr(obj.as_ptr() as usize)
                .into_iter()
                .collect(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => Vec::new(),
        }
    }

    /// Release a pin set taken by [`Self::pin_critical_region`] at
    /// `GetPrimitiveArrayCritical`. No-op on the generational collector / for an
    /// empty set.
    pub fn unpin_critical_regions(&self, region_indices: &[usize]) {
        if let VmHeap::G1(h) = self {
            for &idx in region_indices {
                h.unpin_region(idx);
            }
        }
    }

    /// Read an array element with auto-unboxing of wrapper types.
    /// G1 falls back to plain get_array_element (no unboxing support yet).
    pub fn get_array_element_unboxing(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        match self {
            VmHeap::Generational(h) => h.get_array_element_unboxing(obj, index),
            VmHeap::G1(h) => h.get_array_element(obj, index),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.get_array_element(obj, index),
        }
    }

    /// Bulk-read a char[] array into a `Vec<u16>`.
    pub fn read_char_array_bulk(&self, obj: ObjectRef) -> Vec<u16> {
        match self {
            VmHeap::Generational(h) => h.read_char_array_bulk(obj),
            VmHeap::G1(h) => {
                // Residual humongous OOB fix: a G1 humongous char[] (any
                // char[] larger than ~region_size/2 — common for large
                // Strings) is laid out across SEVERAL NON-contiguous region
                // buffers, each with its own HEADER_SIZE prefix. The previous
                // code read the whole payload as one flat contiguous run from
                // `obj.as_ptr() + HEADER_SIZE`, which walks off the end of the
                // start region's buffer once the offset exceeds one region's
                // usable payload — a heap out-of-bounds read.
                //
                // vm_heap has no reachable G1 API to (a) detect that `obj` is a
                // HumongousStart or (b) translate a flat offset through the
                // region map (`humongous_span` / `humongous_copy` /
                // `lookup_region_for_addr` are all private to g1.rs). The
                // region-aware per-element accessor `G1Collector::get_array_element`
                // IS reachable, and it already routes humongous arrays through
                // `humongous_copy` (see g1.rs `get_array_element`), so every read
                // is confined to the object's own backing memory regardless of
                // how many regions it spans. Use it for the whole array: this is
                // correct for both ordinary and humongous arrays and can never
                // read out of bounds. (We deliberately do NOT keep the flat
                // fast-path for the non-humongous case, because vm_heap cannot
                // soundly tell the two apart without a new g1.rs API.)
                let len = h.array_length(obj);
                let mut out = vec![0u16; len];
                for (i, slot) in out.iter_mut().enumerate() {
                    // Char elements decode to `Value::Int(u16 as i32)`; mask
                    // back to the 16-bit code unit. `i < len` so the index is
                    // always in bounds and `get_array_element` returns `Ok`.
                    if let Ok(v) = h.get_array_element(obj, i) {
                        *slot = v.as_int().unwrap_or(0) as u16;
                    }
                }
                out
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                let len = h.array_length(obj);
                let mut out = vec![0u16; len];
                for (i, slot) in out.iter_mut().enumerate() {
                    if let Ok(v) = h.get_array_element(obj, i) {
                        *slot = v.as_int().unwrap_or(0) as u16;
                    }
                }
                out
            }
        }
    }

    /// Raw pointer to the array data region (just past the header).
    ///
    /// Every array now has a contiguous payload starting at
    /// `obj.as_ptr() + HEADER_SIZE`, so this returns `Some(ptr)` valid for the
    /// full `len * stride` span: generational and ordinary single-region G1
    /// arrays trivially, and G1 **humongous** arrays because all regions are
    /// adjacent slices of one backing arena, making a humongous span one
    /// physically-contiguous block (see `G1Collector`'s `arena` /
    /// `alloc_humongous_locked`). Before that arena change a humongous array
    /// was fragmented across non-contiguous region buffers and this returned
    /// `None` to force callers onto the region-aware per-element fallback; that
    /// fallback (`get_array_element` / `set_array_element` /
    /// `read_char_array_bulk`) is still correct but no longer required for
    /// humongous, and bulk consumers (arraycopy, GPU marshalling, Unsafe) now
    /// get the fast contiguous path. The `Option` is retained for API
    /// stability and so a future non-contiguous layout could opt back out.
    ///
    /// SAFETY (the `Some` case): `obj` is a live array `ObjectRef` in the heap
    /// arena and `HEADER_SIZE` is the layout-documented start of the payload.
    /// The pointer is valid for `len * stride` contiguous bytes for the
    /// lifetime of the object (which the caller must not let be collected or
    /// moved while holding the pointer).
    pub fn array_data_ptr(&self, obj: ObjectRef) -> Option<*mut u8> {
        // Contiguous from `obj + HEADER_SIZE` for every array: generational and
        // single-region G1 trivially, and G1 humongous because all regions are
        // adjacent slices of one arena (so a humongous span is one block).
        Some(unsafe { obj.as_ptr().add(HEADER_SIZE) })
    }

    // =====================================================================
    // GC operations
    // =====================================================================

    pub fn needs_gc(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.needs_gc(),
            VmHeap::G1(h) => h.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.needs_gc(),
        }
    }

    /// GC trigger queried from a JIT allocation/refill helper while the
    /// compiled caller remains discoverable on the native stack.
    pub fn needs_gc_for_jit_allocation(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.needs_gc_for_jit_allocation(),
            VmHeap::G1(h) => h.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.needs_gc(),
        }
    }

    /// Native-wrapper allocation-pressure signal, consumed at the
    /// `safe_native_call` boundary to run the GC the wrappers themselves
    /// cannot.
    ///
    /// Each collector latches this from the point where it notices the
    /// mutator is outrunning it *inside* a native callback — where no
    /// collection can be initiated, because the callback's locals are not all
    /// rooted yet. Generational: a young→old spill
    /// (`GenHeap::young_spill_pressure`). G1: a new Eden region claimed with
    /// the Free pool already under the `needs_gc` threshold
    /// (`G1Collector::native_alloc_pressure`) — without it, a workload that
    /// allocates only from inside natives never reaches ANY safepoint and G1's
    /// infallible allocator aborts the process on a heap full of garbage (see
    /// `docs/internal/fixed-suite-bugs/g1-native-alloc-no-safepoint-oom-FIXED.md`).
    #[inline]
    pub fn young_spill_pressure(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.young_spill_pressure(),
            VmHeap::G1(h) => h.native_alloc_pressure(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// Clear the native-wrapper allocation-pressure signal.
    #[inline]
    pub fn clear_young_spill_pressure(&self) {
        match self {
            VmHeap::Generational(h) => h.clear_young_spill_pressure(),
            VmHeap::G1(h) => h.clear_native_alloc_pressure(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// Record a native-wrapper allocation-pressure event — see
    /// [`Self::young_spill_pressure`].
    #[inline]
    pub fn note_young_spill_pressure(&self) {
        match self {
            VmHeap::Generational(h) => h.note_young_spill_pressure(),
            VmHeap::G1(h) => h.note_native_alloc_pressure(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// DBG: young-arena state snapshot — see `GenHeap::young_arena_diag`.
    pub fn young_arena_diag(&self) -> (usize, usize, usize, usize) {
        match self {
            VmHeap::Generational(h) => h.young_arena_diag(),
            _ => (0, 0, 0, 0),
        }
    }

    /// Live-bytes estimate for GC-productivity accounting — see
    /// `GenHeap::live_bytes_estimate` (young free-list-aware; the raw bump
    /// cursor never retreats under the non-moving sweep). Other collectors
    /// fall back to `allocated_bytes`, their historical metric.
    pub fn live_bytes_estimate(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.live_bytes_estimate(),
            _ => self.allocated_bytes(),
        }
    }

    /// Total bytes promoted young→old across all collections (selective
    /// promotion + the moving collector's tenuring). Used by the
    /// GC-overhead productivity metric: a promotion-only cycle conserves
    /// live bytes but did useful allocation-enabling work.
    pub fn bytes_promoted_total(&self) -> u64 {
        match self {
            VmHeap::Generational(h) => h.stats().snapshot().bytes_promoted,
            _ => 0,
        }
    }

    /// Bytes the TENURED space can still absorb — the "is the heap actually
    /// wedged?" half of the GC-overhead limit (see
    /// `interpreter::note_gc_productivity`).
    ///
    /// The freed-bytes threshold on its own cannot tell a genuine
    /// retained-allocation death spiral (survivors promoted into an old
    /// generation that is already full, so nothing drains) from a young
    /// generation that simply has nothing to promote while old gen sits nearly
    /// empty. The distinguishing fact is whether old gen can still absorb a
    /// young drain, which is a question about the OLD generation specifically,
    /// not about total fullness: in the spiral the young semi is emptied every
    /// cycle, so *total* fullness parks near young/total and never looks
    /// exhausted — which is exactly why a total-fullness gate was rejected.
    ///
    /// Non-generational backends have no separate tenured space; report their
    /// whole-heap headroom, which for a single-space collector is the same
    /// question.
    pub fn old_gen_headroom(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.old_gen_capacity().saturating_sub(h.old_gen_used()),
            _ => self.heap_capacity().saturating_sub(self.allocated_bytes()),
        }
    }

    /// Run a garbage collection cycle.
    ///
    /// The `stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        match self {
            VmHeap::Generational(h) => h.collect_garbage(stw, roots, monitors),
            VmHeap::G1(h) => h.collect_garbage(stw, roots, monitors),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.collect_garbage(stw, roots, monitors),
        }
    }

    /// GC with finalizer-aware resurrection: dead-but-finalizable objects
    /// are kept alive by the collection (evacuated under G1, marked under
    /// ZGC, forwarded under the semispace young collector) and their
    /// POST-collection addresses returned so the caller can enqueue them
    /// for `finalize()` — and mark their `ReferenceProcessor` entries
    /// enqueued so each object is finalized at most once.
    ///
    /// The `stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage_with_finalizers(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        match self {
            VmHeap::Generational(h) => {
                h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
            }
            VmHeap::G1(h) => {
                h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
            }
        }
    }

    pub fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // Task #25 debug-triad assertion: if this thread recently fired
        // a `write_barrier_pre` (post-store sequence), the matching
        // `write_barrier` MUST follow within the same logical store —
        // the SATB / card-marking invariants depend on (pre, store,
        // post) being co-located. We clear the flag here so the next
        // pre-barrier starts a fresh triad. The flag is per-thread and
        // updated only under `debug_assertions`, so release builds pay
        // nothing.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        match self {
            VmHeap::Generational(h) => h.write_barrier(obj, stored_value),
            VmHeap::G1(h) => h.write_barrier(obj, stored_value),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.write_barrier(obj, stored_value),
        }
    }

    /// Task #25: SATB pre-store barrier dispatched through the
    /// `GarbageCollector::write_barrier_pre` trait method. Call BEFORE
    /// overwriting a heap-managed reference slot. `old` is the value
    /// that will be lost; concurrent marking treats it as a root for
    /// the remainder of the cycle.
    ///
    /// `slot` is reserved for future debug triad-assertions and may be
    /// `null_mut()` if the caller only has the value (e.g. SATB log
    /// drain rebroadcasts).
    #[inline]
    pub fn write_barrier_pre(&self, slot: *mut ObjectRef, old: ObjectRef) {
        // Task #25: arm the per-thread triad sentinel. Cleared on the
        // matching `write_barrier`. Debug-only — release builds elide.
        #[cfg(debug_assertions)]
        arm_pending_pre_barrier();
        match self {
            VmHeap::Generational(h) => {
                <GenerationalHeap as GarbageCollector>::write_barrier_pre(h, slot, old)
            }
            VmHeap::G1(h) => <G1Collector as GarbageCollector>::write_barrier_pre(h, slot, old),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => <ZgcRealHeap as GarbageCollector>::write_barrier_pre(h, slot, old),
        }
    }

    /// Enqueue a reference as live for an active SATB mark cycle without
    /// performing a heap store. This is used by `Reference.get()` paths: the
    /// referent was read, not overwritten, so it must not participate in the
    /// debug `(pre, store, post)` triad tracked by [`Self::write_barrier_pre`].
    #[inline]
    pub fn write_barrier_keep_alive(&self, referent: ObjectRef) {
        match self {
            VmHeap::Generational(h) => <GenerationalHeap as GarbageCollector>::write_barrier_pre(
                h,
                std::ptr::null_mut(),
                referent,
            ),
            VmHeap::G1(h) => <G1Collector as GarbageCollector>::write_barrier_pre(
                h,
                std::ptr::null_mut(),
                referent,
            ),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => <ZgcRealHeap as GarbageCollector>::write_barrier_pre(
                h,
                std::ptr::null_mut(),
                referent,
            ),
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.allocated_bytes(),
            VmHeap::G1(h) => h.allocated_bytes(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.allocated_bytes(),
        }
    }

    /// Return stable generational card-table metadata for JIT inline barriers.
    ///
    /// G1 and ZGC require collector-specific remembered-set/barrier protocols,
    /// so they return `None` and generated code retains the helper call.
    pub fn jit_card_table_info(&self) -> Option<(usize, usize, usize)> {
        match self {
            VmHeap::Generational(heap) => Some(heap.jit_card_table_info()),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Return the total number of GC collections performed so far.
    ///
    /// For the generational heap, this is the sum of minor + major cycle
    /// counts (sampled with `Relaxed` ordering — see `HeapStats::snapshot`).
    /// Used by the JMX `GarbageCollectorMXBean.getCollectionCount/Time`
    /// natives; H2's `Utils.collectGarbage()` polls this in a
    /// `while(prev == cur) { System.gc(); }` loop, so a constant `0` here
    /// hangs the H2 test runner indefinitely.
    pub fn collection_count(&self) -> u64 {
        match self {
            VmHeap::Generational(h) => {
                let s = h.stats().snapshot();
                s.minor_gc_count + s.major_gc_count
            }
            VmHeap::G1(h) => h.collection_count(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.gc_count() as u64,
        }
    }

    /// Return the total heap capacity in bytes.
    pub fn heap_capacity(&self) -> usize {
        match self {
            VmHeap::Generational(h) => {
                // 2 semi-spaces (only one active) + old gen
                h.young_semi_capacity() + h.old_gen_capacity()
            }
            VmHeap::G1(h) => h.heap_capacity(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.heap_capacity(),
        }
    }

    /// Return young generation (used, capacity) in bytes.
    pub fn young_gen_stats(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => {
                let from = h.young_from_used();
                let cap = h.young_semi_capacity();
                (from, cap)
            }
            VmHeap::G1(h) => h.eden_stats(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => (0, 0),
        }
    }

    /// Return old generation (used, capacity) in bytes.
    pub fn old_gen_stats(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => {
                let cap = h.old_gen_capacity();
                let used = h.old_gen_used();
                (used, cap)
            }
            VmHeap::G1(h) => h.old_gen_stats(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => (h.allocated_bytes(), h.heap_capacity()),
        }
    }

    /// Free-heap estimate in **megabytes** for the `SoftReference` LRU policy,
    /// deliberately keyed on *allocatable* rather than merely *unused* bytes.
    ///
    /// HotSpot's `LRUMaxHeapPolicy` clears a soft reference when it has been
    /// idle longer than `SoftRefLRUPolicyMSPerMB * free_heap_MB`, so this number
    /// is the entire pressure dimension of the policy: return a large value and
    /// nothing is ever cleared, return zero and everything is.
    ///
    /// **Why not simply `heap_capacity() - live_bytes_estimate()`.** The default
    /// collector does not compact. `moving_young` defaults false, and
    /// `gen_heap` fail-closes to a non-moving mark-sweep whenever any thread
    /// holds a live JIT frame — the steady state at a 500-invocation JIT
    /// threshold; compaction's correctness blocker closed 2026-07-26
    /// (`docs/internal/fixed-suite-bugs/app-jvm-bugs/moving-young-gen-drops-jit-held-oops-FIXED.md`),
    /// and moving-young is now the default. Under a
    /// non-moving, fragmenting heap "unused bytes" and "bytes an
    /// allocation can actually obtain" diverge without bound: a heap can be 60%
    /// unused and still fail a modest allocation because no single free run is
    /// large enough. A policy keyed on unused bytes then refuses to clear soft
    /// references precisely while allocation is failing — the worst possible
    /// time — and `OutOfMemoryError` is thrown with a heap full of reclaimable
    /// soft-reachable objects.
    ///
    /// So this reports the free space of the generation that must satisfy the
    /// next allocation (young/eden), not the whole-heap figure, and floors the
    /// result at the old generation's headroom only insofar as young space is
    /// backed by it. It is intentionally *pessimistic*: under-reporting free
    /// space makes the policy clear soft references sooner, which costs cache
    /// hit rate; over-reporting makes it clear them never, which costs the
    /// process. Prefer the recoverable failure.
    ///
    /// # HANDOFF — this accessor has no caller yet (cross-owner)
    ///
    /// The two production reference-processing sites both pass a hardcoded
    /// `64`, in a file this module's owner may not edit:
    ///
    /// * `vm/src/runtime/interpreter.rs`, in `process_references_after_gc`:
    ///   `let result = ref_proc.process_references(&is_marked, 64, 0);`
    /// * `vm/src/runtime/interpreter.rs`, in `g1_remark_process_references`:
    ///   `let result = ref_proc.process_references(is_marked, 64, 0);`
    ///
    /// Both should become `shared.mem.heap.soft_ref_policy_free_mb()` in place
    /// of the `64`. (The `0` third argument no longer matters: `gc::reference`
    /// now substitutes the mutator clock it observes through
    /// `touch_soft_reference` when the caller passes `0`. See the
    /// `last_observed_clock_ms` field doc there.) Until that lands, the soft-ref
    /// policy runs on a constant 64 MB of assumed headroom and therefore does
    /// not respond to memory pressure at all. Tracked in
    /// `docs/internal/arch-2026-07-26/refs-metaspace-unloading.md`.
    pub fn soft_ref_policy_free_mb(&self) -> usize {
        const MB: usize = 1024 * 1024;
        let (young_used, young_cap) = self.young_gen_stats();
        let (old_used, old_cap) = self.old_gen_stats();

        // Whole-heap headroom, as an upper bound.
        let total_free = (young_cap + old_cap).saturating_sub(young_used + old_used);

        // Allocation-relevant headroom. An allocation lands in young/eden; when
        // that space is exhausted a collection runs and the survivors must fit
        // in the old generation. Both must have room, so the binding constraint
        // is the smaller of the two.
        let young_free = young_cap.saturating_sub(young_used);
        let old_free = old_cap.saturating_sub(old_used);
        let allocatable = if young_cap == 0 {
            // Backends that report no separate young space (ZGC's real heap
            // reports `(0, 0)`): the old-gen pair carries the whole heap.
            old_free
        } else {
            young_free.min(old_free)
        };

        // Never report more than the whole-heap headroom, and round DOWN: a
        // sub-megabyte remainder must read as 0 MB (maximum pressure), not 1.
        allocatable.min(total_free) / MB
    }

    // =====================================================================
    // GenerationalHeap-specific methods (no-ops for G1)
    // =====================================================================

    /// SATB barrier for concurrent marking.
    /// For G1, logs the old reference to the global SATB queue when marking is active.
    pub fn satb_barrier(&self, old_value: Value) {
        match self {
            VmHeap::Generational(h) => h.satb_barrier(old_value),
            VmHeap::G1(h) => {
                if let Value::Object(Some(obj_ref)) = old_value {
                    h.satb_pre_barrier(obj_ref.as_ptr() as usize);
                }
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// Round-5 fix (CRIT — UAF): drain THIS thread's per-thread SATB
    /// buffer into the global SATB queue.
    ///
    /// Must be called on every mutator thread immediately before it
    /// blocks at a GC safepoint, and on the GC-initiating thread before
    /// it runs initial-mark / remark. Without this drain, up to
    /// `DEFAULT_SATB_CAPACITY` (256) overwritten references per thread
    /// remain in the thread-local buffer and never reach the marker —
    /// causing the classic SATB lost-object scenario (A→B replaced by
    /// A→null after A is scanned but before B is scanned), which the
    /// next evacuation turns into a use-after-free on B.
    ///
    /// The `thread_local!` storage means each thread must call this
    /// itself; the GC cannot reach into another thread's buffer.
    pub fn flush_thread_satb(&self) {
        match self {
            VmHeap::G1(h) => {
                if h.satb_queue().is_active() {
                    crate::satb::flush_thread_satb_buffer(h.satb_queue());
                }
            }
            VmHeap::Generational(h) => {
                // Clone-free: this runs on EVERY JIT helper entry, and the
                // common case (no concurrent old-gen mark running) is a
                // single Acquire load — don't pay an Arc refcount round
                // trip just to check `is_active`.
                if let Some(q) = h.satb_queue_ref() {
                    if q.is_active() {
                        crate::satb::flush_thread_satb_buffer(q);
                    }
                }
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// Check if old generation needs GC (generational only).
    pub fn old_gen_needs_gc(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.old_gen_needs_gc(),
            VmHeap::G1(_) => false, // G1 manages its own concurrent marking
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// Get old generation base pointer and capacity (generational only).
    pub fn old_gen_info(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => h.old_gen_info(),
            VmHeap::G1(_) => (0, 0),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => (0, 0),
        }
    }

    /// Lock the old generation for direct access (generational only).
    /// Returns None for G1.
    pub fn old_gen_lock(&self) -> Option<parking_lot::MutexGuard<'_, OldGen>> {
        match self {
            VmHeap::Generational(h) => Some(h.old_gen_lock()),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Collect every young-gen object's reference to an old-gen object
    /// (mandatory concurrent old-gen marking roots — see
    /// `GenerationalHeap::collect_young_to_old_roots`). Generational only;
    /// empty for G1 (its concurrent marking has its own remembered sets).
    /// Must be called during a GC safepoint.
    pub fn collect_young_to_old_roots(&self) -> Vec<usize> {
        match self {
            VmHeap::Generational(h) => h.collect_young_to_old_roots(),
            VmHeap::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => Vec::new(),
        }
    }

    /// Enable concurrent GC support (generational only).
    pub fn enable_concurrent_gc(
        &mut self,
        satb_queue: Arc<SatbQueue>,
        gc_state: Arc<ConcurrentGcState>,
    ) {
        match self {
            VmHeap::Generational(h) => h.enable_concurrent_gc(satb_queue, gc_state),
            VmHeap::G1(_) => {} // G1 has built-in concurrent marking
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// Probe whether a young-gen allocation would succeed, OR whether a young
    /// GC should be forced first to keep an evacuation reserve.
    ///
    /// Returning `None` routes the caller (the JIT alloc helpers'
    /// `if try_alloc_young_probe(..).is_none() { maybe_gc_forced }` path) into a
    /// young GC before the allocation is attempted.
    pub fn try_alloc_young_probe(&self, size: usize) -> Option<()> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_young_probe(size),
            // G1: JIT-compiled code never reaches the interpreter's `maybe_gc`
            // safepoint poll (which consults `needs_gc()`), so without a probe
            // here a JIT-heavy mutator allocates Eden right up to a 100%-full
            // heap before *any* young GC fires. At that point every region is
            // Eden (in the collection set) and ZERO Free regions remain for
            // to-space, so `evacuate_object` finds no destination — without the
            // self-forward net the whole CSet (incl. the live set) is reclaimed
            // (the SteadyChurn `-XX:+UseG1GC` + JIT OOM; the generational
            // collector hides it via its non-moving JIT-active sweep, which needs
            // no to-space). Mirror the interpreter's threshold: signal "collect
            // now" (`None`) while the evacuation reserve (Free regions) is still
            // intact, so the forced young GC has somewhere to evacuate. `size`
            // is unused for G1 — the reserve is region-granular (`needs_gc()` =
            // Free regions < 25%), not a byte-level bump check.
            VmHeap::G1(_) => {
                if self.needs_gc() {
                    None
                } else {
                    Some(())
                }
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {
                if self.needs_gc() {
                    None
                } else {
                    Some(())
                }
            }
        }
    }

    /// O(1) probe: can the young-gen BUMP TAIL supply `size` bytes right now?
    ///
    /// Unlike [`Self::try_alloc_young_probe`], this never consults the young
    /// free list (`largest_free_block` is an O(free-blocks) scan — calling it
    /// per allocation from the JIT TLAB-refill gate was ~55% of a
    /// binarytrees-18 run once young fragmented). Used to decide whether a
    /// TLAB refill is worth attempting: a fragmented-but-full young (the
    /// non-moving-sweep steady state, whose cursor can never retreat) answers
    /// `false`, sending the caller to the old-gen spill path instead of
    /// scanning the free list on every allocation.
    pub fn young_bump_headroom(&self, size: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.young_bump_headroom(size),
            // G1's Eden is region-granular; reuse the reserve signal.
            VmHeap::G1(_) => !self.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => !self.needs_gc(),
        }
    }

    /// Amortized-O(1) probe: could a young TLAB refill of `size` bytes be
    /// served from RECLAIMED young space right now? See
    /// `GenerationalHeap::young_has_free_block` — early-exit scan behind the
    /// cached largest-block upper bound, safe to consult per allocation.
    /// Together with [`Self::young_bump_headroom`] this forms the JIT
    /// TLAB-refill gate.
    pub fn young_has_free_block(&self, size: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.young_has_free_block(size),
            // G1 refills carve whole-region chunks from Eden; the reserve
            // signal is the applicable "room without forcing a GC" answer.
            VmHeap::G1(_) => !self.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => !self.needs_gc(),
        }
    }

    /// Returns whether this heap is using the G1 collector.
    pub fn is_g1(&self) -> bool {
        matches!(self, VmHeap::G1(_))
    }

    /// Check if G1 should start concurrent marking (IHOP threshold crossed).
    pub fn g1_should_start_marking(&self) -> bool {
        match self {
            VmHeap::G1(g1) => {
                // Check if old gen bytes exceed the G1 marking threshold (IHOP)
                g1.old_gen_bytes() > g1.marking_threshold_bytes()
                    && g1.marking_threshold_bytes() > 0
            }
            VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// Check if G1 concurrent marking is currently active.
    pub fn g1_is_marking_active(&self) -> bool {
        match self {
            VmHeap::G1(g1) => g1.gc_state.is_marking_active(),
            VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// Start a G1 concurrent mark cycle: activate SATB, clear bitmaps,
    /// and spawn the background [`ConcurrentMarkController`].
    ///
    /// Task #56: the controller is parked in `G1State::concurrent_mark`
    /// and joined later by [`Self::g1_signal_marking_complete`].
    ///
    /// Edge case (re-entry): if a controller is already parked from a
    /// previous unfinished cycle, we log a warning and join the stale
    /// worker before restarting. Skipping the new cycle would leave a
    /// running worker racing with the bitmap clear below.
    pub fn g1_start_concurrent_mark(&self) {
        if let VmHeap::G1(state) = self {
            let mut slot = state.concurrent_mark.lock();
            if let Some(stale) = slot.take() {
                tracing::warn!(
                    "g1_start_concurrent_mark: prior controller still parked \
                     ({} steps) — joining before restart",
                    stale.steps_performed(),
                );
                drop(slot); // release before potentially-blocking join
                let _ = stale.request_stop_and_join();
                slot = state.concurrent_mark.lock();
            }
            // Order matters: phase must be ConcurrentMark before the
            // worker starts stepping.
            state.collector.start_concurrent_mark();
            *slot = Some(ConcurrentMarkController::spawn(Arc::clone(
                &state.collector,
            )));
        }
    }

    /// INT-8: publish the referent-slot skip set for the cycle that
    /// [`Self::g1_start_concurrent_mark`] just opened — the Weak/Soft/Phantom
    /// `Reference` OBJECT addresses from the VM's reference registry,
    /// snapshotted inside the same initial-mark STW. Must run BEFORE
    /// [`Self::g1_mark_roots`] seeds the gray set (the skip set gates how
    /// Reference objects are scanned). No-op on other backends.
    pub fn g1_set_reference_skip_set(&self, addrs: &[usize]) {
        if let VmHeap::G1(state) = self {
            state.collector.set_reference_skip_set(addrs);
        }
    }

    /// Mark roots into G1's mark bitmap.
    pub fn g1_mark_roots(&self, roots: &[cratonvm_types::ObjectRef]) {
        if let VmHeap::G1(g1) = self {
            g1.remark(roots); // remark marks roots + drains SATB
                              // The worker (spawned by `g1_start_concurrent_mark` just before this)
                              // may have already drained the initially-empty worklist and parked
                              // with `quiesced=true`. These roots seed real work, so wake it and
                              // clear the premature quiescence — otherwise the completion poll
                              // could fire before the seeded graph is marked.
            if let Some(ctrl) = g1.concurrent_mark.lock().as_ref() {
                ctrl.notify_work_available();
            }
        }
    }

    /// Perform one step of G1 concurrent marking. Returns true when done.
    pub fn g1_concurrent_mark_step(&self, work_amount: usize) -> bool {
        match self {
            VmHeap::G1(g1) => g1.concurrent_mark_step(work_amount),
            VmHeap::Generational(_) => true,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => true,
        }
    }

    /// Probe whether the G1 concurrent-mark controller (if any) has
    /// exhausted its worklist and the worker thread has exited. Used
    /// by the coordinator to schedule `g1_signal_marking_complete`
    /// once natural completion is reached, without holding the
    /// controller in a blocking join. Returns `true` when there is no
    /// active controller (vacuously finished).
    pub fn g1_concurrent_mark_finished(&self) -> bool {
        if let VmHeap::G1(state) = self {
            let slot = state.concurrent_mark.lock();
            // Completion = the worker has marked to a FIXED POINT (`is_quiesced`),
            // NOT that the worker thread exited (`!is_running`). The worker parks
            // (stays alive) at a fixed point and only exits on `request_stop`,
            // which the coordinator issues *after* observing completion — so
            // polling `!is_running` here deadlocked, leaving marking permanently
            // "active" and mixed GC never firing (old-gen never reclaimed).
            return slot.as_ref().map_or(true, |c| c.is_quiesced());
        }
        true
    }

    /// G1 STW final remark + cleanup — the cycle-ending twin of
    /// [`Self::g1_mark_roots`].
    ///
    /// MUST be called from inside a stop-the-world pause (the caller holds
    /// the GC barrier; every mutator is parked at its safepoint), with
    /// `roots` covering ALL threads. Performs, in order:
    ///
    /// 1. joins the background marker (its worklist is at a fixed point —
    ///    the caller gates on [`Self::g1_concurrent_mark_finished`], and a
    ///    stopped worker rules out interleaving with the drain below);
    /// 2. `remark(roots)` — re-scans roots and drains the SATB log (every
    ///    parked mutator's local buffer + the global shards) into the gray
    ///    set. Without this the SATB write barrier's entire output was
    ///    DISCARDED at cleanup: the cycle was a one-shot closure from the
    ///    initial roots, racing mutator edge deletions, and any live object
    ///    whose only marker-visible path was rewritten mid-trace stayed
    ///    unmarked (the SteadyChurn freed-live-Old-region defect);
    /// 3. drains the gray set to a fixed point (including the overflow
    ///    rescans — `concurrent_mark_step` returns `false` until both the
    ///    worklist is empty and no overflow recovery is pending);
    /// 4. INT-8 — when `process_refs` is supplied, invokes it with the
    ///    post-remark liveness predicate (`G1Collector::is_live_after_mark`:
    ///    bitmap + TAMS snapshot; conservative LIVE without positive
    ///    evidence). The callback runs the VM's reference processing
    ///    (clears, queue links, finalizer/cleaner submissions) and returns
    ///    the addresses that must be RESURRECTED — dead finalizables about
    ///    to run `finalize()`, pending cleaner chains, policy-retained soft
    ///    referents. Those are marked gray and the closure re-drained, so
    ///    step 5 cannot free them. This window — bitmap complete, nothing
    ///    freed yet — is the only point in the cycle where weak/soft refs
    ///    to dead OLD-region referents can be cleared (evacuation pauses
    ///    only ever see CSet deaths);
    /// 5. `cleanup()` — per-region liveness, in-place free of wholly-dead
    ///    Old regions, humongous reclaim, SATB deactivation — while the
    ///    world is still stopped.
    ///
    /// Returns `false` (and does nothing) when no cycle is active, so a
    /// second initiator that lost the STW race cannot re-run remark against
    /// an already-completed cycle.
    pub fn g1_final_remark_and_cleanup(
        &self,
        roots: &[cratonvm_types::ObjectRef],
        process_refs: Option<&mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize>>,
    ) -> bool {
        if let VmHeap::G1(state) = self {
            let Some(ctrl) = state.concurrent_mark.lock().take() else {
                // Defensive: a marking-active phase with no controller is an
                // orphaned cycle nobody is driving — abort it (no bitmap
                // verdicts) rather than letting the completion gate spin on
                // it forever. We are inside an STW here, so this cannot race
                // `g1_start_concurrent_mark` (also STW-only).
                if state.collector.gc_state.is_marking_active() {
                    tracing::warn!(
                        "g1_final_remark_and_cleanup: marking active with no \
                         controller — aborting orphaned cycle"
                    );
                    state.collector.abort_concurrent_mark();
                } else {
                    tracing::trace!("g1_final_remark_and_cleanup: no active controller");
                }
                return false;
            };
            let steps = ctrl.steps_performed();
            let outcome = ctrl.request_stop_and_join();
            state.collector.remark(roots);
            while !state.collector.concurrent_mark_step(usize::MAX) {}
            tracing::debug!(
                "g1_final_remark_and_cleanup: remark+drain done (worker steps={}, joined_ok={})",
                steps,
                outcome.is_ok(),
            );
            // INT-8 (step 4): reference processing against the completed
            // bitmap, then resurrection of everything the processor handed
            // out, BEFORE cleanup can free it.
            if let Some(cb) = process_refs {
                let collector = &state.collector;
                let is_live = |addr: usize| collector.is_live_after_mark(addr);
                let resurrect = cb(&is_live);
                collector.resurrect_after_remark(&resurrect);
            }
            state
                .collector
                .gc_state
                .set_phase(ConcurrentGcPhase::ConcurrentSweep);
            state.collector.cleanup();
            state.collector.gc_state.set_phase(ConcurrentGcPhase::Idle);
            return true;
        }
        false
    }

    /// Signal that G1 concurrent marking is complete: join the worker,
    /// run cleanup, return phase to `Idle`.
    ///
    /// NOTE: prefer [`Self::g1_final_remark_and_cleanup`] — this variant
    /// skips the final remark (no root re-scan, no SATB drain), so any
    /// reference the mutators overwrote during the concurrent phase never
    /// reaches the bitmap. Retained for tests and as the abort/teardown
    /// path; the runtime cycle driver no longer calls it.
    ///
    /// Task #56: drains the [`ConcurrentMarkController`] slot and joins
    /// the background worker (blocking). The STW remark the caller runs
    /// next requires a quiescent worklist, so the join is mandatory.
    ///
    /// Edge case (no active controller): defensive no-op — happens when
    /// called twice, or before any cycle started. Skipping cleanup keeps
    /// the phase machine clean (cleanup itself is idempotent, but
    /// running it from Idle would flip `marking_complete` spuriously).
    pub fn g1_signal_marking_complete(&self) {
        if let VmHeap::G1(state) = self {
            let ctrl = state.concurrent_mark.lock().take();
            match ctrl {
                Some(ctrl) => {
                    let steps = ctrl.steps_performed();
                    let outcome = ctrl.request_stop_and_join();
                    tracing::debug!(
                        "g1_signal_marking_complete: joined (steps={}, joined_ok={})",
                        steps,
                        outcome.is_ok(),
                    );
                    state
                        .collector
                        .gc_state
                        .set_phase(ConcurrentGcPhase::ConcurrentSweep);
                    state.collector.cleanup();
                    state.collector.gc_state.set_phase(ConcurrentGcPhase::Idle);
                }
                None => {
                    tracing::trace!("g1_signal_marking_complete: no active controller");
                }
            }
        }
    }

    /// Enable GC logging (verbose:gc).
    pub fn enable_gc_logging(&self) {
        match self {
            VmHeap::G1(g1) => g1.enable_gc_logging(),
            VmHeap::Generational(_) => {
                // Generational heap doesn't have a logging toggle yet,
                // but log that GC logging was requested.
                tracing::info!("[GC] Verbose GC logging enabled (generational collector)");
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {
                tracing::info!("[GC] Verbose GC logging enabled (ZGC-real collector)");
            }
        }
    }

    /// Print the aggregate per-collection pause summary (p50/p99/max young +
    /// mixed) to stderr. No-op for the generational collector (which keeps no
    /// pause history) and when no G1 collection has run. Called at VM shutdown
    /// when GC stats are requested (`--verbose:gc` or `CRATONVM_GC_STATS`).
    pub fn print_gc_summary(&self) {
        if let VmHeap::G1(g1) = self {
            g1.print_gc_summary();
        }
        // Collection COUNTS, unconditionally. Without these the summary is not
        // comparable across configurations: the moving-young line below only
        // prints when moving-young is requested, so a `CRATONVM_NO_MOVING_YOUNG`
        // run printed nothing at all and "did this config collect more?" — the
        // first question to ask about an allocation-heavy regression — could not
        // be answered from a log. Cheap: two relaxed loads at shutdown.
        if let VmHeap::Generational(h) = self {
            let s = h.stats().snapshot();
            eprintln!(
                "[GC] generational: minor={} major={}",
                s.minor_gc_count, s.major_gc_count,
            );
            // Re-publish the normalization denominators so the card-cost report
            // below divides by the CURRENT heap rather than by whatever the
            // last collection saw. Cheap: two arena locks at shutdown.
            h.publish_gc_metrics_occupancy();
        }
        // Old-gen free-list coalescing (the counterpart of the young sweep's
        // post-sweep coalescer). A large `merged` with compaction never having
        // run is the fragmentation regime this exists for; `calls>0 merged=0`
        // says the free list was already maximally coalesced.
        {
            use std::sync::atomic::Ordering as O;
            let calls = crate::old_gen::COALESCE_CALLS.load(O::Relaxed);
            let merged = crate::old_gen::BLOCKS_MERGED.load(O::Relaxed);
            if calls > 0 {
                eprintln!("[GC] oldgen_coalesce: calls={calls} blocks_merged={merged}");
            }
        }
        // H2-CID0 — the conservative-root and free-list invariants. Printed
        // unconditionally when non-zero so a soak log answers "did the workload
        // actually enter the regime this fix is about?" without a debug flag.
        //
        // `interior_root_pins` non-zero means conservative roots really are
        // interior words of live old-gen objects in this workload, i.e. the hole
        // the pin closes was live. `freed_interior_pinned` is ZERO BY
        // CONSTRUCTION unless `CRATONVM_GC_NO_OLD_INTERIOR_PINS` disabled the
        // pin — it is the negative control, and a non-zero value on an ordinary
        // run would mean the pin has regressed. `free_list_overlaps` non-zero
        // means an old-gen span was freed twice.
        {
            use std::sync::atomic::Ordering as O;
            let pins = crate::gen_heap::OLDMARK_INTERIOR_ROOT_PINS.load(O::Relaxed);
            let freed = crate::gen_heap::OLD_SWEEP_FREED_INTERIOR_PINNED.load(O::Relaxed);
            let overlaps = crate::gen_heap::OLD_FREE_LIST_OVERLAPS.load(O::Relaxed);
            if pins | freed | overlaps != 0 {
                eprintln!(
                    "[GC] oldgen_conservative: interior_root_pins={pins} \
                     freed_interior_pinned={freed} free_list_overlaps={overlaps}"
                );
            }
            // The compacting arm's half of the same question, and the cost of
            // the answer. `dropped_interior_root` is zero by construction once
            // the downgrade is in; `downgraded` against `major=N` above says how
            // often compaction had to give way to the in-place sweep.
            let c_watched = crate::gen_heap::COMPACT_DROPPED_WATCHED.load(O::Relaxed);
            let c_interior = crate::gen_heap::COMPACT_DROPPED_INTERIOR_ROOT.load(O::Relaxed);
            let c_down = crate::gen_heap::COMPACT_DOWNGRADED_INTERIOR_ROOT.load(O::Relaxed);
            if c_watched | c_interior | c_down != 0 {
                eprintln!(
                    "[GC] oldgen_compact: dropped_watched_referents={c_watched} \
                     dropped_interior_root={c_interior} downgraded_to_inplace={c_down}"
                );
            }
        }
        // What the collector actually did on the last cycle and why. This is
        // the line that settles the `docs/GC.md` ("young collections run
        // non-moving whenever any JIT frame is active") vs `ARCHITECTURE.md`
        // ("per-cycle coverage proof, moving is possible") disagreement for
        // THIS run — see `docs/gc/tlab-and-card-audit.md` §3.
        eprintln!("{}", crate::gc_metrics::collector_decision_report());
        // Card / remembered-set costs, raw and normalized per allocated object
        // and per live byte.
        eprintln!("{}", crate::gc_metrics::gc_metrics_report());
        let fallbacks = crate::gc_quiescence::moving_young_coverage_fallback_count();
        if crate::gc_quiescence::moving_young_enabled() || fallbacks > 0 {
            // Both numbers, always. A correct answer while `cycles == 0` means
            // the young generation never actually copied anything, which is the
            // exact way the 2026-07-01 validation declared moving-young working
            // while it was inert (see
            // `docs/internal/arch-2026-07-26/moving-young-corruption-rootcause.md`
            // section 6). The histogram then names what stopped it.
            let cycles = crate::gc_quiescence::moving_young_cycle_count();
            eprintln!("[GC] moving_young: cycles={cycles} coverage_fallbacks={fallbacks}");
            let counts = crate::gc_quiescence::moving_young_fallback_reason_counts();
            for (reason, n) in counts.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] moving_young_fallback_reason: {}={}",
                        crate::gc_quiescence::incomplete_reason::label(reason),
                        n
                    );
                }
            }
        }
    }

    /// Get the number of fields (slots) in an object.
    pub fn num_fields(&self, obj: ObjectRef) -> usize {
        dispatch!(self, get_header(obj)).num_slots() as usize
    }

    /// Carve out a TLAB from the young generation (generational) or Eden region (G1).
    /// Returns `Some((ptr, size))` on success.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        match self {
            VmHeap::Generational(h) => h.refill_tlab(requested_size),
            VmHeap::G1(h) => h.refill_tlab(requested_size),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Check if a raw address is within a live (non-Free) region of the heap.
    /// For generational GC, always returns false (not applicable).
    /// For G1, checks whether the address falls within an allocated portion
    /// of a non-Free region — used by reference processing to distinguish
    /// live objects in non-collected regions from dead objects.
    pub fn is_addr_live(&self, addr: usize) -> bool {
        match self {
            // An old-gen address is live if it is inside an ALLOCATED span.
            // Reference processing uses this to avoid clearing weak/soft refs
            // whose referent was tenured in an earlier cycle; returning `false`
            // here unconditionally — the original behavior — cleared every weak
            // reference to a promoted object on the next young GC.
            //
            // This used to be the bare range check `is_old_gen_addr`, on the
            // premise that "a minor GC never collects the old generation". That
            // premise is false: `sweep_old_gen_non_moving` reclaims dead old-gen
            // blocks IN PLACE, during a young collection, whenever a live JIT
            // frame blocks the moving young collector — so a freed block kept
            // answering "live" and no consumer of this predicate ever pruned a
            // dangling old-gen entry. See
            // `docs/internal/fixed-suite-bugs/gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
            //
            // Young-GC live-reclaim ROOT FIX (2026-07-07): also recognize
            // kept-in-place young survivors of the NON-MOVING sweep (which
            // produces no pointer_map entries), or reference processing
            // judges every live young Reference object/referent dead —
            // skipping the post-GC referent restore and pruning live
            // processor entries (the RRWL hold-count IMSE/hang family). See
            // `GenerationalHeap::is_live_young_survivor` for the soundness
            // argument (STW-window-only, zeroed-span discriminator,
            // moving-collection compatibility).
            VmHeap::Generational(h) => {
                h.is_live_old_gen_addr(addr) || h.is_live_young_survivor(addr)
            }
            VmHeap::G1(h) => h.is_addr_in_live_region(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_heap_addr(addr).is_some(),
        }
    }

    /// Exact post-collection survival verdict for a PRE-collection address
    /// that was published as WATCHED before the collection ran (every address
    /// the reference processor holds — see
    /// `ReferenceProcessor::all_tracked_addrs`).
    ///
    /// This is the strict counterpart of [`Self::is_addr_live`], and exists
    /// because that predicate's old-generation arm is deliberately permissive:
    /// it answers `true` for *any* address inside the old-gen arena, which is
    /// correct for a minor cycle (old gen is not touched, so nothing moved)
    /// but wrong the moment an old-gen reclamation runs in the same cycle.
    /// After a mark-compact `major_gc` a dead old-gen object has been slid
    /// over by a live neighbour or left in the zeroed freed tail; after the
    /// in-place `sweep_old_gen_non_moving` its block is back on the free list.
    /// Reference processing then judged the dead entry "survived", never
    /// pruned it, and every subsequent collection wrote a referent pointer
    /// through the stale address into whatever now occupied that memory —
    /// dropped by the `gen_heap::set_field` out-of-bounds guard when the
    /// victim was the zeroed tail, and a silent dangling-pointer store into a
    /// live object otherwise (`SIGSEGV` /
    /// `gen_heap::read_slot: corrupt Value cell`, the HIB-CV-32 family; see
    /// `docs/internal/fixed-suite-bugs/h2/bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`).
    ///
    /// Both old-gen paths now emit an identity `pointer_map` entry for every
    /// watched address that survived without moving, so once
    /// `gc_quiescence::old_gen_reclaimed_last_cycle()` is set, map membership
    /// is a complete and exact proof.
    pub fn watched_pre_gc_addr_survived(
        &self,
        addr: usize,
        pointer_map: &std::collections::HashMap<usize, usize>,
    ) -> bool {
        if pointer_map.contains_key(&addr) {
            return true;
        }
        // Bisection escape hatch — restores the pre-fix permissive predicate.
        if crate::gc_flags().no_exact_refproc_survival {
            return self.is_addr_live(addr);
        }
        match self {
            VmHeap::Generational(h) => {
                if h.is_old_gen_addr(addr) {
                    return !crate::gc_quiescence::old_gen_reclaimed_last_cycle();
                }
                h.is_live_young_survivor(addr)
            }
            // G1/ZGC: `is_addr_live` is already exact (live-region membership
            // / registry lookup), and neither has a generation whose addresses
            // are trusted wholesale.
            VmHeap::G1(_) => self.is_addr_live(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => self.is_addr_live(addr),
        }
    }

    /// Is `addr` inside memory the collector has RECLAIMED?
    ///
    /// H2-CID0 — see [`GenerationalHeap::reclaimed_hole_at`] for why this
    /// exists: it is the flag-free discriminator between an ordinary
    /// `new Object()` and a reference into a span the collector freed and
    /// zeroed, which are otherwise indistinguishable at the point a
    /// `checkcast` fails with `java.lang.Object` as the actual class.
    ///
    /// G1/ZGC return `None`: their liveness is region/registry based and
    /// `is_addr_live` already answers exactly, so there is no free-list view
    /// to consult.
    pub fn reclaimed_hole_at(&self, addr: usize) -> Option<(&'static str, usize, usize)> {
        match self {
            VmHeap::Generational(h) => h.reclaimed_hole_at(addr),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Generational: is `addr` inside EITHER young semispace? Used by
    /// reference processing to detect stale PRE-GC Reference addresses
    /// (young + absent from the pointer map ⇒ did not survive the GC).
    /// G1: `false` (no semispace; staleness is handled by `is_addr_live`).
    pub fn is_in_young_addr(&self, addr: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.is_in_young_either(addr as *const u8),
            VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// True when `addr` is safe to defer to the loader-scoped
    /// `metadata_pin` side-channel instead of pushing it as an unconditional
    /// GC root (see `vm::memory::roots`'s static-field, class-lock and
    /// CONSTANT_Dynamic root sections, all gated on `conditional_metadata`).
    ///
    /// Generational: ONLY old-gen addresses. `metadata_pin` is consulted
    /// exclusively by `old_gen_gc`'s BFS (`gen_heap.rs`), which never scans
    /// the young generation, and `sweep_young_non_moving` / the moving young
    /// copy closure seed strictly from the direct root set — neither
    /// consults `metadata_pin`. A YOUNG address deferred here has no path to
    /// ever be marked: if it has no other reachability (the common case for
    /// a static field's value immediately after `<clinit>` assigns it, or a
    /// class-lock/condy object with no other reference), it is silently
    /// reclaimed and its memory reused by the very next allocation —
    /// producing a live object that reads back as a DIFFERENT, unrelated
    /// type. See `docs/known-issues/spb1-springframework-util-investigation.md`'s
    /// repro-3 follow-up for the observed corruption shape (a `ClassUtils`
    /// static field, loaded via a user-defined `ClassLoader`, read back as
    /// an unrelated live object from later in the same `<clinit>`).
    ///
    /// G1 / ZGC: always `true` — both backends' `metadata_pin` consumers
    /// (`g1.rs`, `zgc.rs`) walk every live region uniformly during the same
    /// full-mark pass that activates `conditional_metadata`, so deferring a
    /// young-resident object is sound; this matches their existing,
    /// unconditional behavior and is unchanged here.
    pub fn metadata_pin_deferrable(&self, addr: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.is_old_gen_addr(addr),
            VmHeap::G1(_) => true,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => true,
        }
    }

    /// Same question as [`Self::metadata_pin_deferrable`], but for the
    /// `mirror_pin` side channel specifically (`vm::memory::roots` step 6 —
    /// `java.lang.Class` mirrors of user-loader-defined classes).
    ///
    /// `metadata_pin_deferrable` is old-gen-only under Generational because
    /// `metadata_pin`'s guaranteed consumer is `old_gen_gc`'s BFS. `mirror_pin`
    /// has a SECOND consumer the metadata case cannot rely on:
    /// `mark_young_precise_object` follows it as an ordinary marking edge — so
    /// whenever this cycle is certain to take the non-moving young marker, a
    /// still-YOUNG mirror is reachable through its loader and does not need an
    /// unconditional root.
    ///
    /// This is what makes class unloading work at all for a short-lived loader:
    /// a webapp/JSP class whose mirror has never been promoted (the common case
    /// when an explicit `System.gc()` is the first collection of the run) would
    /// otherwise be rooted directly forever, and — since a mirror's
    /// `classLoader` field is a real heap edge — would drag its entire defining
    /// loader and everything that loader references along with it. Symptom:
    /// `TestDefaultInstanceManager.testClassUnloading` counting 9 cached
    /// annotation entries where HotSpot has 8, because the evicted JSP's
    /// `Class` was the sole root-held anchor of the whole compiler graph. See
    /// `docs/known-issues/tomcat/26-defaultinstancemanager-classunload-offbyone.md`.
    ///
    /// Opt out with `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1`, which restores the
    /// old old-gen-only behaviour for bisection.
    pub fn mirror_pin_deferrable(&self, addr: usize) -> bool {
        match self {
            VmHeap::Generational(h) => {
                if h.is_old_gen_addr(addr) {
                    return true;
                }
                mirror_pin_young_defer_enabled()
                    && crate::gc_quiescence::young_marker_follows_side_tables()
            }
            VmHeap::G1(_) => true,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => true,
        }
    }

    /// Backend-generic post-GC staleness verdict for a PRE-collection
    /// address: `true` iff the object at `addr` did NOT survive the
    /// collection whose `pointer_map` is supplied — i.e. writing through
    /// `addr` (or `pointer_map`-relocating it) would touch freed/reused
    /// memory. Used by the post-GC reference-processing writers
    /// (clear/enqueue/finalize/cleaner) as their anti-corruption guard.
    ///
    /// Per backend:
    /// - Generational: a young-space address absent from the pointer map did
    ///   not survive (a live young object is always in the map after a
    ///   moving young GC, and non-moving sweeps emit identity entries for
    ///   watched survivors). Old-gen addresses are conservatively treated
    ///   as surviving (they do not move in a minor GC; major relocations
    ///   are merged into the map).
    /// - G1: every live CSet object is in the pointer map (identity entries
    ///   for self-forwarded ones) and every live non-CSet address sits in a
    ///   live region — so "absent from the map AND not in a live region" is
    ///   exact. This closes the G1/ZGC hole where the old young-only guard
    ///   was hardwired inert and stale finalize/cleaner addresses flowed to
    ///   `run_finalizers` (UAF on recycled CSet memory).
    /// - ZGC: non-moving — dead means gone from the registry
    ///   (`is_addr_live` false).
    pub fn pre_gc_addr_did_not_survive(
        &self,
        addr: usize,
        pointer_map: &std::collections::HashMap<usize, usize>,
    ) -> bool {
        if pointer_map.contains_key(&addr) {
            return false;
        }
        match self {
            VmHeap::Generational(h) => h.is_in_young_either(addr as *const u8),
            VmHeap::G1(_) => !self.is_addr_live(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => !self.is_addr_live(addr),
        }
    }

    /// Walk all live objects in the heap (both generations / all regions).
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        dispatch!(self, walk_objects())
    }

    /// TEMPORARY diagnostic (CRATONVM_DBG_MIRRORPIN / TestDefaultInstanceManager
    /// investigation): scan every live object in the heap for a reference to
    /// `target_addr`, returning `(holder_addr, holder_class_id)` for each
    /// match. Must be called during a GC safepoint (same contract as
    /// `walk_objects`, which this is built on). O(live objects × avg field
    /// count) — debug-only, never on a hot path. Remove once the
    /// investigation concludes.
    pub fn find_referrers(&self, target_addr: usize) -> Vec<(usize, u32)> {
        let mut out = Vec::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object, so
            // `obj_ptr` targets a valid, fully-initialized `ObjectHeader`.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let mut found = false;
            unsafe {
                crate::gen_heap::for_each_ref_slot(obj_ptr, header, |ref_ptr, _slot| {
                    if ref_ptr as usize == target_addr {
                        found = true;
                    }
                });
            }
            if found {
                out.push((obj_ptr as usize, header.class_id.as_u32()));
            }
        }
        out
    }

    /// TEMPORARY diagnostic (doc-26 class-unloading investigation): BFS the
    /// full *referrer* closure of `target_addr` in a SINGLE heap walk, so a
    /// "why is this still alive" question can be answered without one
    /// O(live objects) pass per hop.
    ///
    /// Returns `(depth, addr, class_id, referrer_count)` for every object that
    /// transitively references `target_addr`, breadth-first from the target
    /// (`depth == 0` is the target itself), capped at `max_nodes`. An entry
    /// with `referrer_count == 0` is held by NO heap field — i.e. it is
    /// retained by a GC ROOT, which is exactly the interesting case: the
    /// closure's zero-referrer members enumerate every root that could be
    /// keeping the target alive.
    ///
    /// Same safepoint contract as `walk_objects`. Debug-only.
    pub fn referrer_closure(
        &self,
        target_addr: usize,
        max_nodes: usize,
    ) -> Vec<(usize, usize, u32, usize)> {
        // One walk -> reverse edge map (referent -> [(holder, holder_cid)]).
        let mut reverse: std::collections::HashMap<usize, Vec<(usize, u32)>> =
            std::collections::HashMap::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let holder = (obj_ptr as usize, header.class_id.as_u32());
            unsafe {
                crate::gen_heap::for_each_ref_slot(obj_ptr, header, |ref_ptr, _slot| {
                    if !ref_ptr.is_null() {
                        reverse.entry(ref_ptr as usize).or_default().push(holder);
                    }
                });
            }
        }

        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<(usize, usize, u32)> =
            std::collections::VecDeque::new();
        queue.push_back((0, target_addr, u32::MAX));
        seen.insert(target_addr);
        while let Some((depth, addr, cid)) = queue.pop_front() {
            let referrers = reverse.get(&addr).map_or(0, Vec::len);
            out.push((depth, addr, cid, referrers));
            if out.len() >= max_nodes {
                break;
            }
            if let Some(holders) = reverse.get(&addr) {
                for &(holder, holder_cid) in holders {
                    if seen.insert(holder) {
                        queue.push_back((depth + 1, holder, holder_cid));
                    }
                }
            }
        }
        out
    }

    /// TEMPORARY diagnostic (doc-26 class-unloading investigation): traverse
    /// the FULL referrer closure of `target_addr` and report only its
    /// *root-held* members — objects that no live heap field points at, yet
    /// which are themselves live. Those are precisely the entry points through
    /// which a GC root (or a side-table propagation such as `mirror_pin` /
    /// `loader_pin` / an overlay owner edge) is keeping `target_addr` alive.
    ///
    /// Each returned entry is a path `[root_held, .., target]` of
    /// `(addr, class_id)` pairs, so the retaining chain is readable end to end
    /// instead of having to be reassembled from a flat node dump. At most
    /// `max_paths` paths are returned; the traversal itself is bounded by
    /// `max_nodes` so a pathological heap cannot wedge the diagnostic.
    ///
    /// Same safepoint contract as `walk_objects`. Debug-only.
    pub fn root_held_paths(
        &self,
        target_addr: usize,
        max_nodes: usize,
        max_paths: usize,
    ) -> Vec<Vec<(usize, u32)>> {
        self.retention_paths(target_addr, &|_| false, max_nodes, max_paths)
    }

    /// As [`Self::root_held_paths`], but additionally terminates a branch at
    /// any address the caller's `is_root` predicate accepts — i.e. at a member
    /// of the ACTUAL root vector the collector was handed this cycle.
    ///
    /// This is the query that distinguishes the two ways an object can survive:
    /// a path ending at a root-set member says "a real GC root reaches it, and
    /// this chain is how"; a path ending at a zero-referrer node inside a
    /// strongly-connected component says "nothing outside this component
    /// references it — it was marked through a side-table propagation
    /// (`loader_pin` / `mirror_pin` / `metadata_pin` / an overlay owner edge)".
    /// Chasing the wrong one of those wastes an entire investigation cycle.
    pub fn retention_paths(
        &self,
        target_addr: usize,
        is_root: &dyn Fn(usize) -> bool,
        max_nodes: usize,
        max_paths: usize,
    ) -> Vec<Vec<(usize, u32)>> {
        let mut reverse: std::collections::HashMap<usize, Vec<(usize, u32)>> =
            std::collections::HashMap::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let holder = (obj_ptr as usize, header.class_id.as_u32());
            unsafe {
                crate::gen_heap::for_each_ref_slot(obj_ptr, header, |ref_ptr, _slot| {
                    if !ref_ptr.is_null() {
                        reverse.entry(ref_ptr as usize).or_default().push(holder);
                    }
                });
            }
        }

        // BFS outwards along reverse edges, remembering each node's parent (the
        // object it references) so a discovered root-held node can be walked
        // back down to the target.
        let mut parent: std::collections::HashMap<usize, (usize, u32)> =
            std::collections::HashMap::new();
        let mut cid_of: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let mut root_held: Vec<usize> = Vec::new();
        queue.push_back(target_addr);
        seen.insert(target_addr);
        let mut visited = 0usize;
        while let Some(addr) = queue.pop_front() {
            visited += 1;
            if visited >= max_nodes {
                break;
            }
            // A root-set member terminates the branch: we have the answer for
            // this chain and walking past it would only find its own referrers.
            if addr != target_addr && is_root(addr) {
                root_held.push(addr);
                continue;
            }
            match reverse.get(&addr) {
                None => root_held.push(addr),
                Some(holders) if holders.is_empty() => root_held.push(addr),
                Some(holders) => {
                    for &(holder, holder_cid) in holders {
                        if seen.insert(holder) {
                            cid_of.insert(holder, holder_cid);
                            parent.insert(holder, (addr, holder_cid));
                            queue.push_back(holder);
                        }
                    }
                }
            }
        }

        let mut paths = Vec::new();
        for node in root_held.into_iter().take(max_paths) {
            let mut path = vec![(node, cid_of.get(&node).copied().unwrap_or(u32::MAX))];
            let mut cur = node;
            // Walk parent links down to the target (bounded by `seen`'s size).
            while let Some(&(next, _)) = parent.get(&cur) {
                path.push((next, cid_of.get(&next).copied().unwrap_or(u32::MAX)));
                cur = next;
                if cur == target_addr || path.len() > 64 {
                    break;
                }
            }
            paths.push(path);
        }
        paths
    }

    /// TEMPORARY diagnostic (is_live_young_survivor false-positive
    /// investigation): scan every live object in the heap for one whose
    /// `class_id` matches `target_class_id`, returning its address. Used to
    /// tell apart "a live instance of the evicted class genuinely still
    /// exists somewhere (loader_pin correctly keeps its loader alive)" from
    /// "nothing legitimate justifies the loader being marked alive." Must be
    /// called during a GC safepoint (same contract as `walk_objects`).
    pub fn find_instances_of_class(&self, target_class_id: u32) -> Vec<usize> {
        let mut out = Vec::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object, so
            // `obj_ptr` targets a valid, fully-initialized `ObjectHeader`.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if header.class_id.as_u32() == target_class_id {
                out.push(obj_ptr as usize);
            }
        }
        out
    }
}

// ─── Phase 6 #1: GPU/GC coordination tests ───────────────────────────
//
// Verify that `enter_gpu_critical` increments/decrements the global
// `GPU_CRITICAL_COUNT` and that `wait_for_gpu_critical_drain` blocks
// while any token is alive.

#[cfg(all(test, feature = "gpu-offload"))]
mod gpu_coordination_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn token_lifecycle_drives_global_counter() {
        let heap = VmHeap::new(GcBackend::Generational, 1 << 20);
        let before = GPU_CRITICAL_COUNT.load(Ordering::Acquire);
        {
            let _t = heap.enter_gpu_critical();
            assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before + 1);
            assert_eq!(heap.gpu_critical_count(), before + 1);
            {
                let _t2 = heap.enter_gpu_critical();
                assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before + 2);
            }
            assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before + 1);
        }
        assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before);
    }

    /// Hold a token on a worker thread for 200 ms; assert the
    /// drain on the main thread blocks at least 150 ms.
    #[test]
    fn drain_blocks_until_every_token_dropped() {
        let heap = std::sync::Arc::new(VmHeap::new(GcBackend::Generational, 1 << 20));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let heap2 = heap.clone();
        let holder = std::thread::spawn(move || {
            let _t = heap2.enter_gpu_critical();
            started_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        started_rx.recv().unwrap();
        let start = Instant::now();
        wait_for_gpu_critical_drain();
        let elapsed = start.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed >= Duration::from_millis(150),
            "drain returned in {elapsed:?} but the token was held for 200ms",
        );
    }
}

// ─── Task #56: ConcurrentMarkController wiring into VmHeap ──────────────
//
// Exercises the start/complete coordinator: that g1_start_concurrent_mark
// actually spawns a controller, that g1_signal_marking_complete actually
// joins it, that the SATB barrier is captured during the concurrent
// phase, and that the edge cases (re-entry, defensive no-op, repeated
// cycles) behave as documented.

#[cfg(test)]
mod concurrent_mark_controller_tests {
    use super::*;
    use crate::g1::G1CollectorConfig;

    fn make_g1_heap() -> VmHeap {
        // Small heap — fast to construct, big enough for the few
        // allocations these tests perform.
        let mut cfg = G1CollectorConfig::default();
        cfg.heap_size = 4 * 1024 * 1024;
        cfg.region_size = 1024 * 1024;
        VmHeap::G1(G1State::new(cfg))
    }

    /// Helper: extract the G1State for white-box assertions on the
    /// controller slot. Returns `None` for a generational heap.
    fn g1_state(heap: &VmHeap) -> Option<&G1State> {
        match heap {
            VmHeap::G1(s) => Some(s),
            _ => None,
        }
    }

    /// The `-XX:` G1 knobs flow config → `G1ConfigOverrides` →
    /// `new_with_overrides` → `G1CollectorConfig`. region_size is observable via
    /// the region count; the other overrides take the identical match-arm path.
    #[test]
    fn g1_config_overrides_apply() {
        // Default (1 MiB regions): 8 MiB / 1 MiB = 8 regions.
        let h = VmHeap::new(GcBackend::G1, 8 * 1024 * 1024);
        assert_eq!(g1_state(&h).unwrap().num_regions(), 8);
        // -XX:G1HeapRegionSize=2m override: 8 MiB / 2 MiB = 4 regions.
        let ov = G1ConfigOverrides {
            region_size: Some(2 * 1024 * 1024),
            ..Default::default()
        };
        let h2 = VmHeap::new_with_overrides(GcBackend::G1, 8 * 1024 * 1024, ov);
        assert_eq!(
            g1_state(&h2).unwrap().num_regions(),
            4,
            "G1HeapRegionSize override must change the region count"
        );
        // Generational backend ignores G1 overrides (no panic / no effect).
        let _ = VmHeap::new_with_overrides(GcBackend::Generational, 8 * 1024 * 1024, ov);
    }

    #[test]
    fn start_then_signal_complete_spawns_and_joins_controller() {
        let heap = make_g1_heap();

        // Before start: no controller parked.
        assert!(
            !g1_state(&heap).unwrap().has_active_controller(),
            "fresh heap should have no controller",
        );

        // Start: phase flips to ConcurrentMark and a controller is
        // installed.
        heap.g1_start_concurrent_mark();
        assert!(
            heap.g1_is_marking_active(),
            "phase must be ConcurrentMark after start"
        );
        assert!(
            g1_state(&heap).unwrap().has_active_controller(),
            "controller must be parked after g1_start_concurrent_mark",
        );

        // Complete: drains the slot, joins the worker, runs cleanup,
        // phase returns to Idle.
        heap.g1_signal_marking_complete();
        assert!(
            !g1_state(&heap).unwrap().has_active_controller(),
            "controller slot must be drained after g1_signal_marking_complete",
        );
        assert!(
            !heap.g1_is_marking_active(),
            "phase must return to Idle after cleanup",
        );
    }

    #[test]
    fn repeated_cycles_do_not_leak_threads() {
        // Each cycle spawns and joins a worker; running many cycles
        // back-to-back must not accumulate join handles or Arc clones.
        let heap = make_g1_heap();
        let collector_arc_before = Arc::strong_count(&g1_state(&heap).unwrap().collector);

        for _ in 0..10 {
            heap.g1_start_concurrent_mark();
            heap.g1_signal_marking_complete();
        }

        // After every cycle has joined, the only strong reference to
        // the collector must be the one VmHeap holds. A leaked worker
        // thread would still be holding a clone and bump this count.
        let collector_arc_after = Arc::strong_count(&g1_state(&heap).unwrap().collector);
        assert_eq!(
            collector_arc_before, collector_arc_after,
            "Arc<G1Collector> ref count leaked across cycles \
             (before={collector_arc_before}, after={collector_arc_after})",
        );
        assert!(
            !g1_state(&heap).unwrap().has_active_controller(),
            "no controller should remain after the final complete",
        );
    }

    #[test]
    fn double_start_joins_stale_controller_before_spawning_new_one() {
        // Edge case: calling g1_start_concurrent_mark twice without a
        // complete in between. The implementation joins the stale
        // controller (logging a warning) and replaces it. The phase
        // remains ConcurrentMark; the new controller is parked.
        let heap = make_g1_heap();

        heap.g1_start_concurrent_mark();
        let first_arc_count = Arc::strong_count(&g1_state(&heap).unwrap().collector);

        // Second start without complete in between — must not leak.
        heap.g1_start_concurrent_mark();
        assert!(
            g1_state(&heap).unwrap().has_active_controller(),
            "a controller must still be parked after the second start",
        );

        // Single signal-complete drains everything.
        heap.g1_signal_marking_complete();
        let final_arc_count = Arc::strong_count(&g1_state(&heap).unwrap().collector);
        assert!(
            final_arc_count <= first_arc_count,
            "double start should not strand additional Arc clones \
             (first_active={first_arc_count}, final={final_arc_count})",
        );
        assert!(
            !g1_state(&heap).unwrap().has_active_controller(),
            "controller slot must be empty after complete",
        );
    }

    #[test]
    fn signal_complete_without_active_controller_is_noop() {
        // Defensive: calling g1_signal_marking_complete before any
        // g1_start_concurrent_mark must not panic, must leave the
        // collector in Idle, and must not install a controller.
        let heap = make_g1_heap();

        assert!(!heap.g1_is_marking_active());
        heap.g1_signal_marking_complete(); // no-op path
        assert!(!heap.g1_is_marking_active());
        assert!(!g1_state(&heap).unwrap().has_active_controller());

        // Calling it twice (after a real cycle then a stray call) must
        // also be a no-op.
        heap.g1_start_concurrent_mark();
        heap.g1_signal_marking_complete();
        heap.g1_signal_marking_complete(); // stray second call
        assert!(!heap.g1_is_marking_active());
    }

    #[test]
    fn satb_pre_barrier_captured_during_concurrent_phase() {
        // SATB sanity: while the concurrent phase is active (between
        // g1_start_concurrent_mark and g1_signal_marking_complete) the
        // satb_barrier path must enqueue overwritten references. This
        // is the same property the #54-era marker tests assert at the
        // ConcurrentMarker level — here we exercise it through the
        // VmHeap public API to prove the wiring actually drives SATB.
        let heap = make_g1_heap();

        // Allocate an object inside G1 to use as a "previously live"
        // reference that mutators overwrite during marking.
        let obj = heap.alloc_object(cratonvm_types::ClassId::new(1), 0);

        heap.g1_start_concurrent_mark();

        // Before the overwrite, the SATB queue is empty (initial-mark
        // activated it but nothing was logged yet).
        let satb_queue = g1_state(&heap).unwrap().collector.satb_queue().clone();
        assert!(
            satb_queue.is_active(),
            "SATB must be active during concurrent mark"
        );
        let before = satb_queue.len();

        // Simulate a mutator overwrite: barrier sees the old value.
        heap.satb_barrier(Value::Object(Some(obj)));
        // The pre-barrier writes through the thread-local SATB buffer,
        // so the global queue may not see it yet. Flush this thread's
        // buffer so the assertion is deterministic.
        heap.flush_thread_satb();

        let after = satb_queue.len();
        assert!(
            after > before,
            "satb_barrier during concurrent mark must enqueue the old reference \
             (before={before}, after={after})",
        );

        // Cleanup so we don't strand the worker.
        heap.g1_signal_marking_complete();
    }

    /// Residual humongous OOB regression: `read_char_array_bulk` on a G1
    /// humongous char[] must read every code unit correctly through the
    /// region-aware path, never off the end of the start region's buffer.
    ///
    /// With `region_size = 1 MiB` the per-region usable payload is just under
    /// 1 MiB, so a char[] of 600_000 elements (1.2 MB of payload) is humongous
    /// (total > region_size/2) AND its payload spans TWO regions. The old flat
    /// `copy_nonoverlapping(obj+HEADER_SIZE, .., len*2)` would have walked past
    /// the first region's buffer; the per-element path must not.
    #[test]
    fn read_char_array_bulk_humongous_crosses_region_boundary() {
        let heap = make_g1_heap();
        let len = 600_000usize; // 1.2 MB payload > 1 MiB region => spans 2 regions
        let arr = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Char, len);

        // Write a position-dependent pattern so a stale/short read is detected.
        // Sample a sparse set of indices (writing all 600k is wasteful), making
        // sure to cover the first region, the boundary, and the tail.
        let probe = |i: usize| -> u16 { ((i.wrapping_mul(2654435761)) & 0xFFFF) as u16 };
        let region_usable_chars = (1024 * 1024 - HEADER_SIZE) / 2;
        let mut indices = vec![
            0,
            1,
            region_usable_chars - 1,
            region_usable_chars,
            region_usable_chars + 1,
            len - 1,
        ];
        indices.dedup();
        for &i in &indices {
            heap.set_array_element(arr, i, Value::Int(probe(i) as i32))
                .unwrap();
        }

        let out = heap.read_char_array_bulk(arr);
        assert_eq!(out.len(), len, "bulk read must return all elements");
        for &i in &indices {
            assert_eq!(
                out[i],
                probe(i),
                "code unit at index {i} (region-crossing read) corrupted"
            );
        }
        // Indices we never wrote default to 0 (fresh zeroed payload).
        assert_eq!(
            out[region_usable_chars / 2],
            0,
            "untouched element must read as 0, not garbage"
        );
    }

    /// Sanity: the ordinary (single-region) G1 char[] path still works through
    /// the same per-element bulk reader.
    #[test]
    fn read_char_array_bulk_small_g1_array_roundtrips() {
        let heap = make_g1_heap();
        let len = 8usize;
        let arr = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Char, len);
        for i in 0..len {
            heap.set_array_element(arr, i, Value::Int((0x4100 + i) as i32))
                .unwrap();
        }
        let out = heap.read_char_array_bulk(arr);
        assert_eq!(out.len(), len);
        for i in 0..len {
            assert_eq!(out[i], (0x4100 + i) as u16);
        }
    }

    /// After the single-arena change a G1 humongous array is one contiguous
    /// block, so `array_data_ptr` hands out a flat pointer valid for the whole
    /// payload (not just the first region). Verify ordinary AND humongous
    /// arrays both return `obj + HEADER_SIZE`, and that the humongous flat
    /// pointer round-trips a tail element living far past region 0 — exactly
    /// the bulk-consumer (arraycopy/Unsafe/GPU) access pattern.
    #[test]
    fn array_data_ptr_is_flat_and_contiguous_for_g1_humongous() {
        let heap = make_g1_heap();

        // Ordinary single-region int[]: contiguous pointer just past the header.
        let small = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Int, 8);
        let sptr = heap
            .array_data_ptr(small)
            .expect("ordinary array must have a flat pointer");
        assert_eq!(
            sptr,
            unsafe { small.as_ptr().add(HEADER_SIZE) },
            "flat pointer must be obj + HEADER_SIZE",
        );

        // Humongous int[] (~1.6 MB > 1 MiB region) is now also contiguous.
        let n = 400_000usize;
        let large = heap.alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Int, n);
        let lptr = heap
            .array_data_ptr(large)
            .expect("humongous array now exposes a contiguous flat pointer")
            as *mut i32;
        assert_eq!(
            lptr as *mut u8,
            unsafe { large.as_ptr().add(HEADER_SIZE) },
            "humongous flat pointer must be obj + HEADER_SIZE",
        );

        // The flat pointer must reach the tail element (far past region 0)
        // without escaping the object: write via the raw pointer (as a bulk
        // consumer would), read back via the GC accessor.
        unsafe { std::ptr::write(lptr.add(n - 1), 0x1234_5678) };
        assert_eq!(
            heap.get_array_element(large, n - 1).unwrap().as_int(),
            Some(0x1234_5678),
            "flat write to the humongous tail must round-trip",
        );
    }

    // ======================================================================
    // soft_ref_policy_free_mb — the pressure input to the SoftReference LRU.
    // See the accessor's doc comment for why it is keyed on allocatable
    // rather than merely unused bytes, and for the cross-owner handoff.
    // ======================================================================

    #[test]
    fn soft_ref_policy_free_mb_is_bounded_by_whole_heap_headroom() {
        let heap = VmHeap::new(GcBackend::Generational, 32 * 1024 * 1024);
        let (young_used, young_cap) = heap.young_gen_stats();
        let (old_used, old_cap) = heap.old_gen_stats();
        let total_free_mb =
            (young_cap + old_cap).saturating_sub(young_used + old_used) / (1024 * 1024);

        let reported = heap.soft_ref_policy_free_mb();
        assert!(
            reported <= total_free_mb,
            "reported {reported} MB exceeds whole-heap headroom {total_free_mb} MB"
        );
        // It must also never exceed the young generation's own headroom, which
        // is what an allocation actually has to fit into.
        let young_free_mb = young_cap.saturating_sub(young_used) / (1024 * 1024);
        if young_cap != 0 {
            assert!(
                reported <= young_free_mb,
                "reported {reported} MB exceeds young headroom {young_free_mb} MB"
            );
        }
    }

    #[test]
    fn soft_ref_policy_free_mb_rounds_down_to_max_pressure() {
        // A heap far smaller than 1 MiB of usable headroom must report 0 MB
        // (maximum pressure), never a rounded-up 1 MB that would keep the LRU
        // threshold non-zero.
        let heap = VmHeap::new(GcBackend::Generational, 512 * 1024);
        assert_eq!(
            heap.soft_ref_policy_free_mb(),
            0,
            "sub-megabyte headroom must read as zero free MB"
        );
    }

    #[test]
    fn soft_ref_policy_free_mb_shrinks_as_the_heap_fills() {
        let heap = VmHeap::new(GcBackend::Generational, 64 * 1024 * 1024);
        let before = heap.soft_ref_policy_free_mb();
        // Allocate a few hundred KB of objects; the figure must not grow.
        for _ in 0..2000 {
            let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
        }
        let after = heap.soft_ref_policy_free_mb();
        assert!(
            after <= before,
            "free-MB estimate rose from {before} to {after} while allocating"
        );
    }
}
