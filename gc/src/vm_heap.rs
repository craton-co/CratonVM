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
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE};
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

/// F-14 — the largest region size the ergonomic or an explicit
/// `-XX:G1HeapRegionSize` will produce. HotSpot's cap, and for the same reason:
/// past this, a single region is a large enough unit of collection that
/// `max_gc_pause_ms` stops being controllable and humongous allocation
/// (anything over half a region) stops being reachable for ordinary arrays.
pub const G1_MAX_REGION_SIZE: usize = 32 * 1024 * 1024;

/// F-14 — how many regions the ergonomic aims for, independent of heap size.
///
/// The region COUNT, not the region size, is what several per-pause passes are
/// linear in: the pre-evacuation `(type, cursor)` snapshot, the free-region
/// census, the collection-set filter, `phase4_regions_to_walk`, and both
/// free-region searches. The remembered set's worst case is quadratic in it.
/// Holding the count roughly constant as the heap grows is the whole point;
/// HotSpot targets the same number.
const G1_TARGET_REGION_COUNT: usize = 2048;

/// Round `requested` to a power of two inside
/// `[MIN_REGION_SIZE, G1_MAX_REGION_SIZE]`.
///
/// Power-of-two because the collector's address→region lookup is a shift (see
/// `g1::normalize_region_size`); clamped because both ends of the range are
/// operator-facing policy rather than arithmetic.
pub fn clamp_region_size(requested: usize) -> usize {
    let clamped = requested.clamp(crate::region::MIN_REGION_SIZE, G1_MAX_REGION_SIZE);
    let rounded = crate::g1::normalize_region_size(clamped);
    // Rounding UP can leave the ceiling; rounding down to the ceiling keeps it
    // a power of two because `G1_MAX_REGION_SIZE` is one.
    rounded.min(G1_MAX_REGION_SIZE)
}

/// F-14 — region size for a heap of `total_bytes`, targeting
/// [`G1_TARGET_REGION_COUNT`] regions.
///
/// Replaces a two-step ladder (1 MiB below 4 GiB, 2 MiB above) whose region
/// COUNT grew without bound with heap size: 4096 regions at 4 GiB, 8192 at
/// 16 GiB, 16384 at 32 GiB. This keeps it near 2048 across the range —
/// 1 MiB regions up to a 2 GiB heap, then 2/4/8/16/32 MiB — and tops out at
/// [`G1_MAX_REGION_SIZE`], after which the count grows again by necessity.
pub fn g1_ergonomic_region_size(total_bytes: usize) -> usize {
    clamp_region_size(
        (total_bytes / G1_TARGET_REGION_COUNT).max(crate::region::DEFAULT_REGION_SIZE),
    )
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
    /// `-XX:ParallelGCThreads=<n>` — evacuation worker count (F-13). `None`
    /// leaves `gc_worker_threads` at its `0` = machine-derived default.
    pub parallel_gc_threads: Option<usize>,
    /// `-Xms` — bytes to commit up front (F-16). `None` leaves
    /// `initial_heap_size` at its `0` = ergonomic default.
    pub initial_heap_size: Option<usize>,
    /// `-XX:G1MixedGCLiveThresholdPercent=<n>` (clamped to 1..=100) — an Old
    /// region at or above this percent live is never a mixed candidate.
    pub mixed_gc_live_threshold_percent: Option<u8>,
    /// `-XX:G1HeapWastePercent=<n>` (clamped to 0..=100) — the mixed phase
    /// ends once the candidates' garbage is below this percent of the heap.
    pub heap_waste_percent: Option<u8>,
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

/// Wait, **bounded**, until every live direct `SafepointToken` has been
/// dropped.
///
/// Called from the GC entry points on `GenerationalHeap` and
/// `G1Collector` before starting a collection cycle. The registry-backed
/// coordination every collector now goes through
/// ([`gpu_coordination::before_collection`], run by
/// [`VmHeap::collect_garbage`] before this is reached) is what decides
/// whether the cycle may relocate; this loop only covers a token taken
/// directly on the legacy counter — which, since 2026-09-02, is a test
/// fixture, not a production path.
///
/// AUDIT 2026-09-02: this loop used to be unbounded, with one warning
/// after five seconds. A counter nobody decremented — an abandoned
/// submission, a torn-down VM — was therefore a collector that spun
/// forever on every thread for the rest of the process. It now gives up
/// after the registry's collector budget, and the cycle proceeds
/// non-moving if the registry says relocation is forbidden (see
/// [`gpu_relocation_forbidden`]).
#[cfg(feature = "gpu-offload")]
pub fn wait_for_gpu_critical_drain() {
    use std::sync::atomic::Ordering;
    if GPU_CRITICAL_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let start = std::time::Instant::now();
    let budget = cratonvm_cuda_bridge::critical::collector_wait_budget();
    loop {
        std::thread::yield_now();
        let now = GPU_CRITICAL_COUNT.load(Ordering::Acquire);
        if now == 0 {
            return;
        }
        if start.elapsed() >= budget {
            tracing::warn!(
                "GC waited {:?} for {} direct GPU critical token(s) and is proceeding \
                 non-moving; a direct token held that long is a leak",
                budget,
                now,
            );
            gpu_coordination::forbid_relocation_this_cycle();
            return;
        }
    }
}

/// No-op when the gpu-offload feature is disabled. Callers in the
/// GC entry points can use this unconditionally.
#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub fn wait_for_gpu_critical_drain() {}

/// Whether the cycle in progress must not relocate because of GPU work.
///
/// Read by every collector at its "may I move this" decision: ZGC's
/// stop-the-world slide and its large-object compactor, the generational
/// collector's moving-young choice, G1's evacuation. `false` for the
/// whole life of a build without `gpu-offload`, and for every cycle of a
/// process that never launched a kernel.
#[cfg(feature = "gpu-offload")]
#[inline]
pub fn gpu_relocation_forbidden() -> bool {
    gpu_coordination::relocation_forbidden()
}

/// See the `gpu-offload` twin. Always `false`.
#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub fn gpu_relocation_forbidden() -> bool {
    false
}

/// GPU critical-section coordination, at the one dispatcher every
/// collector goes through.
///
/// AUDIT 2026-09-02. `cratonvm_cuda_bridge::critical` — owned tokens,
/// leases, a bounded collector wait, keep-alive roots that survive a
/// move — existed for six weeks with no caller in this crate or the VM.
/// The collectors waited on a bare counter, forever; the VM held that
/// counter from dispatch to writeback, so collection stopped for the
/// length of every kernel; and ZGC, the default collector, did not wait
/// at all, so its compacting slide could run under a device-to-host copy
/// landing in the arena from the completion reaper, a thread the
/// stop-the-world barrier never stops.
///
/// This module is the wiring. Before a cycle:
///
/// 1. wait, bounded by [`cratonvm_cuda_bridge::critical::collector_wait_budget`],
///    for every token that declared
///    [`Relocation::Forbidden`](cratonvm_cuda_bridge::critical::Relocation::Forbidden)
///    — the short windows in which a DMA reads or writes the heap arena in
///    place. A keep-alive-only token, the kind a submission holds for the
///    life of its kernel, is NOT waited for: that is the whole point.
/// 2. if the wait expired, veto relocation for this cycle
///    ([`gpu_relocation_forbidden`]); the collectors take their non-moving
///    path and the diversion is counted.
/// 3. otherwise declare a moving cycle to the registry, so a `Forbidden`
///    acquisition from an unstopped thread blocks until the cycle ends.
/// 4. splice every outstanding token's keep-alive addresses in as roots,
///    so a writeback target that is reachable from nothing else survives
///    and is remapped.
///
/// After the cycle the remapped addresses are written back through
/// [`Registry::remap_keepalive`](cratonvm_cuda_bridge::critical::Registry::remap_keepalive),
/// and a holder reads them out through `CriticalToken::keepalive_addrs`
/// before it writes.
#[cfg(feature = "gpu-offload")]
pub mod gpu_coordination {
    use cratonvm_cuda_bridge::critical::{self, Registry, WaitOutcome};
    use cratonvm_types::ObjectRef;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Set for the duration of a cycle that must not relocate.
    static RELOCATION_FORBIDDEN: AtomicBool = AtomicBool::new(false);

    /// The process-wide registry.
    pub fn registry() -> &'static Arc<Registry> {
        critical::global()
    }

    /// See [`super::gpu_relocation_forbidden`].
    #[inline]
    pub fn relocation_forbidden() -> bool {
        RELOCATION_FORBIDDEN.load(Ordering::Acquire)
    }

    /// Veto relocation for the cycle in progress. Idempotent; cleared by
    /// [`CycleGuard::after_collection`].
    pub fn forbid_relocation_this_cycle() {
        if !RELOCATION_FORBIDDEN.swap(true, Ordering::AcqRel) {
            registry().record_forced_non_moving_collection();
        }
    }

    /// What one cycle owes the registry when it finishes.
    #[must_use = "after_collection must run, or the relocation veto and the moving-cycle gate stay set"]
    pub struct CycleGuard {
        /// Keep-alive addresses of every outstanding token at cycle start,
        /// as `ObjectRef`s for the root buffer. Empty in the common case.
        pub extra_roots: Vec<ObjectRef>,
        moving_declared: bool,
    }

    /// Steps 1-4 of the module doc. Runs on the collecting thread, with
    /// the world stopped.
    pub fn before_collection() -> CycleGuard {
        let reg = registry();
        let mut forbid = false;
        if reg.outstanding() != 0 {
            let outcome = reg.wait_for_relocation_clearance(critical::collector_wait_budget());
            match outcome {
                WaitOutcome::Drained { .. } => {}
                WaitOutcome::TimedOut { .. } => forbid = true,
            }
        }
        // A wait that came back drained can be overtaken by a token acquired
        // in the gap; the registry answers the question at this instant.
        if !forbid && reg.relocation_forbidden() {
            forbid = true;
        }
        if forbid {
            forbid_relocation_this_cycle();
        } else {
            reg.begin_moving_cycle();
        }
        let extra_roots = reg
            .outstanding_keepalive_addrs()
            .into_iter()
            // SAFETY: the registry holds addresses declared by live tokens
            // whose holders guarantee the objects are heap objects that
            // were alive at declaration; the token being outstanding is
            // what keeps them alive until now.
            .map(|addr| unsafe { ObjectRef::from_raw(addr as *mut u8) })
            .collect();
        CycleGuard {
            extra_roots,
            moving_declared: !forbid,
        }
    }

    impl CycleGuard {
        /// `remapped` is the tail of the root buffer this guard's
        /// `extra_roots` were appended to, after the collector rewrote it.
        pub fn after_collection(self, remapped: &[ObjectRef]) {
            let reg = registry();
            if !self.extra_roots.is_empty() {
                let moved: rustc_hash::FxHashMap<usize, usize> = self
                    .extra_roots
                    .iter()
                    .zip(remapped)
                    .filter(|(old, new)| old.as_ptr() != new.as_ptr())
                    .map(|(old, new)| (old.as_ptr() as usize, new.as_ptr() as usize))
                    .collect();
                if !moved.is_empty() {
                    reg.remap_keepalive(|addr| moved.get(&addr).copied());
                }
            }
            if self.moving_declared {
                reg.end_moving_cycle();
            }
            RELOCATION_FORBIDDEN.store(false, Ordering::Release);
        }
    }
}

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

/// Whether the trusted `*_validated` twins may skip the membership walk their
/// checking counterparts do.
///
/// Default ON. `CRATONVM_GC_NO_VALIDATE_ONCE=1` makes every twin re-validate,
/// restoring the two and three `is_object_address` walks per `NativeContext`
/// accessor call that "validate once per native accessor call" removed — so
/// that change is an A/B inside ONE binary. It landed with a walk count and no
/// wall clock, and on this path those are not the same measurement: the
/// getfield fix removed 34M walks and bought ~1.05x.
///
/// Read once; consulted on the hottest accessor path in the VM.
#[inline]
fn validate_once_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_VALIDATE_ONCE").is_none()
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
    /// # Why an `Arc` and not the heap by value
    ///
    /// Genuine concurrent marking (2026-08-16) needs the marking engine's
    /// worker threads to keep tracing **after** the mark-start safepoint
    /// returns, and [`crate::zgc::mark::ZMarkCoordinator::new`] takes an
    /// `Arc<dyn ZMarkContext>`. `ZgcRealHeap` *is* that context, so the only
    /// two ways to hand it over are an `Arc` or a raw-pointer bridge whose
    /// soundness argument degrades from "cannot outlive one `&self` call" to
    /// "the heap is never moved", which nothing enforces. This is the honest
    /// one, and `Arc<T>: Deref<Target = T>` keeps every existing
    /// `VmHeap::Zgc(h) => h.method()` call site compiling unchanged --
    /// `ZgcRealHeap` has no `&mut self` method.
    #[cfg(feature = "zgc")]
    Zgc(std::sync::Arc<ZgcRealHeap>),
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
                config.region_size = g1_ergonomic_region_size(total_bytes);
                // Explicit -XX: overrides take precedence over the ergonomic,
                // but are held to the same shape — a region size is a power of
                // two in `[MIN_REGION_SIZE, G1_MAX_REGION_SIZE]` no matter who
                // chose it. `G1Collector::new` would round a non-power-of-two
                // up anyway (see `normalize_region_size`); doing it here as
                // well is what keeps the value an operator reads back out of
                // the config equal to the one the collector is using.
                if let Some(rs) = overrides.region_size {
                    if rs > 0 {
                        config.region_size = clamp_region_size(rs);
                    }
                }
                if let Some(ihop) = overrides.ihop_percent {
                    config.ihop_percent = ihop.clamp(1, 100);
                }
                if let Some(pause) = overrides.max_gc_pause_ms {
                    config.max_gc_pause_ms = pause.max(1);
                }
                if let Some(p) = overrides.mixed_gc_live_threshold_percent {
                    config.mixed_gc_live_threshold_percent = p.clamp(1, 100);
                }
                if let Some(p) = overrides.heap_waste_percent {
                    config.heap_waste_percent = p.min(100);
                }
                if let Some(dedup) = overrides.string_dedup {
                    config.string_dedup_enabled = dedup;
                }
                // F-13: an explicit count is still clamped to the hardware by
                // `parallel_worker_count`; 0 would mean "auto" there, so a
                // `-XX:ParallelGCThreads=0` is rejected by the CLI rather than
                // being silently reinterpreted.
                if let Some(n) = overrides.parallel_gc_threads {
                    if n > 0 {
                        config.gc_worker_threads = n;
                    }
                }
                // F-16: `-Xms`. Clamped to the reservation by
                // `G1Collector::new`, so an `-Xms` above `-Xmx` yields a heap
                // rather than a refusal — the two are specified separately and
                // a user who oversizes one should still get a VM.
                if let Some(n) = overrides.initial_heap_size {
                    config.initial_heap_size = n;
                }
                VmHeap::G1(G1State::new(config))
            }
            #[cfg(feature = "zgc")]
            GcBackend::Zgc => {
                let heap = ZgcRealHeap::new_shared(total_bytes);
                // `-XX:MaxGCPauseMillis` reached ONLY G1 before 2026-09-03.
                // On the DEFAULT collector an operator who asked for a pause
                // target got no answer and no diagnostic; ZGC now sizes its
                // allocation budget from it (`refresh_pause_target_budget`).
                // Applied after construction, so it wins over
                // `CRATONVM_ZGC_PAUSE_TARGET_MS`, which seeded the field --
                // an explicit flag must never be overridden by an A/B switch.
                //
                // `0` is refused by the CLI parser (`-XX:MaxGCPauseMillis=0`
                // is warned about and dropped), so `Some(0)` cannot arrive
                // here to mean "off"; that spelling belongs to the env var.
                if let Some(ms) = overrides.max_gc_pause_ms {
                    heap.set_pause_target_ms(ms.max(1));
                }
                VmHeap::Zgc(heap)
            }
        }
    }

    // =====================================================================
    // Core allocation (both backends implement GarbageCollector trait)
    // =====================================================================

    /// Bind every backend to this VM's compact-layout domain.
    ///
    /// Must be called before the VM defines its first class, because
    /// `class_id` is a per-`ClassStore` index and the layout registry is
    /// process-global: an untold heap defaults to the FIRST domain, which is
    /// correct for a single-VM process and merely costs later VMs their compact
    /// layouts (tagged slots are always correct, just larger).
    pub fn set_layout_domain(&self, domain: u32) {
        dispatch!(self, set_layout_domain(domain))
    }

    /// `CRATONVM_DBG_VACATED_FRAMES` bookkeeping: an address the allocator has
    /// just issued is no longer evidence that anything was moved away from it.
    ///
    /// Every non-TLAB allocation door on this type funnels its result through
    /// here. The TLAB door is [`Self::refill_tlab`], which purges the whole
    /// chunk at once — see `gc_quiescence::note_allocated_range` for why the
    /// per-object door alone left the instrument reporting every fresh young
    /// object as a stale reference on the non-ZGC backends.
    #[inline]
    fn note_alloc(o: ObjectRef) -> ObjectRef {
        if !crate::gc_quiescence::vacated_frames_enabled() {
            return o;
        }
        // The whole EXTENT, not just the base. An address the ledger holds
        // because a small object was moved away from it stops being evidence
        // the moment a LARGER object is allocated over it -- and only the base
        // of that larger object would be purged by an address-keyed door, so
        // every interior word stayed in the ledger for the rest of the run.
        // On BindableTests that is a quarter-megabyte of stale entries per
        // large array, which is the exact false-positive class this ledger was
        // repaired to stop producing.
        //
        // SAFETY: `o` is an object the allocator has just finished laying out;
        // its header is initialised and mapped.
        let base = o.as_ptr() as usize;
        let size =
            unsafe { crate::gen_heap::gen_object_total_size(&*(base as *const ObjectHeader)) };
        crate::gc_quiescence::note_allocated_range(base, base.saturating_add(size.max(8)));
        o
    }

    #[inline]
    fn note_alloc_opt(o: Option<ObjectRef>) -> Option<ObjectRef> {
        o.map(Self::note_alloc)
    }

    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        Self::note_alloc(dispatch!(self, alloc_object(class_id, num_fields)))
    }

    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        Self::note_alloc(dispatch!(self, alloc_array(class_id, element_type, length)))
    }

    /// Try to allocate an object. Returns `None` on OOM (caller should trigger GC and retry).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_object(class_id, num_fields),
            VmHeap::G1(h) => h.try_alloc_object(class_id, num_fields),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_object(class_id, num_fields),
        })
    }

    /// Try to allocate directly in the old generation. This is only available
    /// for the generational heap; other heap implementations return `None` so
    /// callers can fall back to their normal allocation path.
    pub fn try_alloc_object_old(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_object_old(class_id, num_fields),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        })
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
        let batch = match self {
            VmHeap::Generational(h) => h.try_alloc_objects_old_batch(class_id, num_fields, count),
            VmHeap::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => Vec::new(),
        };
        if crate::gc_quiescence::vacated_frames_enabled() {
            let addrs: Vec<usize> = batch.iter().map(|o| o.as_ptr() as usize).collect();
            crate::gc_quiescence::note_allocated(&addrs);
        }
        batch
    }

    /// Fallible twin of [`alloc_object`](Self::alloc_object): same (no-GC)
    /// allocation path including the old-generation spill, but returns `None`
    /// on true heap exhaustion instead of aborting the VM. Lets the JIT
    /// object-alloc helper raise a catchable `OutOfMemoryError`.
    pub fn try_alloc_object_full(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_object_full(class_id, num_fields),
            VmHeap::G1(h) => h.try_alloc_object(class_id, num_fields),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_object(class_id, num_fields),
        })
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
        Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_array_full(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        })
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
        Self::note_alloc_opt(match self {
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
        })
    }

    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_array(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        })
    }

    // =====================================================================
    // Header access
    // =====================================================================

    pub fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        dispatch!(self, get_header(obj))
    }

    pub fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        // KINDOF-SENTINEL: see `kind_of` below — same idiom, same observed
        // sentinel (`0xFFFFFFFFFFFFFFFF`) reaching this dispatch unchecked.
        if self.is_object_address(obj.as_ptr() as usize).is_none() {
            // The `ClassId(0)` this returns is what the H2 residual's own
            // `checkcast` reporter had to be taught to look past — see
            // `note_dead_base_deref`, which reports the swallow instead.
            self.note_dead_base_deref(obj, "class_id_of");
            return ClassId::new(0);
        }
        dispatch!(self, class_id_of(obj))
    }

    /// [`Self::class_id_of`] for a caller that has **just** obtained `obj` from
    /// [`Self::is_object_address`] and still holds it.
    ///
    /// The KINDOF-SENTINEL guard above is a full conservative header validation
    /// — region containment, kind/element tags, header plausibility, and an
    /// extent-fits-the-arena re-scan of the region table. It costs ~3 ns, which
    /// is nothing against a corrupted read, and everything when it is the third
    /// time the same address has been through it in one call.
    ///
    /// That is exactly what a compiled-code native call was doing:
    /// `try_jit_site_cached_native_dispatch` validates the receiver, then calls
    /// `class_id_of` (which validates it again), then `decode_dispatch_values`
    /// validates it a third time — measured in
    /// `jit::helpers::jit_native_dispatch_profile` at 3.2 ns for the validator
    /// and 5.0 ns for `class_id_of`, on a ~100 ns call.
    ///
    /// # Contract
    ///
    /// The caller must hold an `ObjectRef` that `is_object_address` returned
    /// `Some` for, on this heap, with no intervening safepoint. `ObjectRef`
    /// alone is not enough: the codebase constructs them from raw JIT slots and
    /// from JNI handles, and the sentinel above is the record of one arriving
    /// unvalidated. Anything less certain must keep using `class_id_of`.
    pub fn class_id_of_validated(&self, obj: ObjectRef) -> ClassId {
        if !validate_once_enabled() {
            return self.class_id_of(obj);
        }
        dispatch!(self, class_id_of(obj))
    }

    /// NEW-1.5 conservative validity check for a *raw stack-spill address*.
    ///
    /// Used by JIT frame root scanning to filter spurious values: returns
    /// `Some(ObjectRef)` only if `addr` lands on a live object header in
    /// this heap. See [`crate::gen_heap::GenerationalHeap::is_object_address`]
    /// for the full contract.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        // TOTAL walk counter, every caller. The per-site census in
        // `vm/src/jit/helpers.rs` tags only the JIT helpers; comparing its sum
        // against this says whether those sites are the whole story or a
        // fraction. Getting that backwards would mean optimising 3% while
        // claiming 12%.
        match self {
            VmHeap::Generational(h) => h.is_object_address(addr),
            VmHeap::G1(h) => h.is_object_address(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_object_address(addr),
        }
    }

    /// `[lo, hi)` envelope containing every address [`Self::is_object_address`]
    /// can possibly accept, or `None` when the backend cannot cheaply supply
    /// one.
    ///
    /// Purely an optimization hint for conservative stack scanning: a word
    /// outside the envelope is definitely not an object address, so the
    /// caller can skip the full per-word validator. A word inside it still
    /// has to go through `is_object_address` — the envelope is a **filter**,
    /// never an answer. Rooting a word on the strength of the range test alone
    /// would accept object *interiors* as bases, which is precisely the
    /// unsoundness [`Self::is_addr_live`]'s ZGC arm was fixed for (see the
    /// coalesced-free-block argument on that arm below).
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        match self {
            VmHeap::Generational(h) => h.conservative_addr_span(),
            VmHeap::G1(h) => h.conservative_addr_span(),
            // ZGC answers `None` — but NOT, as this doc used to claim, because
            // "ZGC keeps live bases in a registry, not a contiguous arena".
            // The registry is only the live-base *index*; `ZgcRealHeap` backs
            // every object and array with one `Mutex<Arena>` (`zgc.rs:1464`)
            // through the single chokepoint `alloc_raw` (`zgc.rs:1772`), and
            // that arena is built once by `with_capacity` (`zgc.rs:1629`) and
            // never grown (no `Arena::grow` call exists in `zgc.rs`). The
            // envelope therefore EXISTS and is immutable for the heap's
            // lifetime — `[Arena::base_ptr(), +Arena::capacity())` — it is
            // simply not reachable from here: the field is private to the
            // `zgc` module and the only bound it publishes is
            // `heap_capacity()`, a length with no base.
            //
            // What the `None` costs: `conservative_roots.rs:4052-4064` hoists
            // this envelope out of the JIT frame scan exactly so the
            // overwhelming majority of stack words — return addresses, ints,
            // native pointers — die on an inline compare. With `None` every
            // 8-byte stack word instead calls `ZgcRealHeap::is_object_address`
            // (`zgc.rs:1892`), whose first act is `self.registry.lock()`: one
            // mutex acquire PER STACK WORD, per root-gathering pass, per
            // thread. `zgc-vmheap-arm-audit.md` §3.4 (AW-5) names this
            // as a competing explanation for the 35 PASS→HANG classes in
            // `zgc-real-fullsuite-regression-RETIRED-20260807.md`,
            // whose ApplicationContext boot/teardown shape is exactly deep
            // stacks × many threads. `zgc.rs:1467-1474` records that the same
            // shape already "read as a hang at scale" once — that fix covered
            // only the exact-base probe, never the per-word lock.
            //
            // Landed 2026-08-07: `ZgcRealHeap::conservative_addr_span` now
            // publishes the arena envelope, captured once in `with_capacity`
            // as two plain `usize` fields and answered without taking the
            // arena lock — the span exists to let the caller reject a word
            // with a range compare and NO lock, so locking to answer it
            // would defeat the point. Sound because the arena is never
            // grown. The arm's old `None` was justified by "ZGC keeps live
            // bases in a registry, not a contiguous arena" — a false
            // premise, and the reason this went unfixed.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.conservative_addr_span(),
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
    /// - ZGC: since 2026-09-02 [`Self::refill_tlab`] DOES hand ZGC mutators a
    ///   chunk (`zgc/vm_tlab.rs`, kill switch `CRATONVM_ZGC_JIT_TLAB=0`), so
    ///   un-retired tails exist there too. The sweep walks the allocation-base
    ///   REGISTRY, never linear memory, so a tail (which holds no registered
    ///   base) is invisible to it by construction; the SLIDE consumes the
    ///   published list -- `relocate_stw` withholds every page a tail touches
    ///   from the relocation set and never lowers the bump cursor below a
    ///   tail's end. A frozen peer's conservative roots are ordinary
    ///   (pinned-by-design) mark roots.
    ///   The pre-2026-09-02 argument ("`refill_tlab` returns `None` on the
    ///   `Zgc` arm, so un-retired tails cannot exist") is GONE and must not be
    ///   reasoned from.
    ///   Do NOT reuse the "non-moving STW mark-sweep" justification that stood
    ///   here until 2026-09-01: `ZgcRealHeap` COMPACTS by default
    ///   (`CRATONVM_ZGC_RELOCATE`, default-on since 2026-08-13). Whether a
    ///   frozen in-JIT peer is safe against a MOVING cycle is a separate
    ///   question, decided by `zgc_relocation_permitted` and
    ///   `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` in `gc/src/zgc.rs` and not
    ///   by this predicate.
    ///
    /// The collector only engages the forcible in-JIT-peer take-over when
    /// this is `true` — now on every backend.
    pub fn supports_jit_tlab_skip(&self) -> bool {
        true
    }

    /// BUG-03 / INT-3 — publish the reserved TLAB tails of forcibly-stopped
    /// in-JIT peers so the collection skips them (non-moving-sweep skip list
    /// on Generational; region-walker skip + CSet exclusion on G1; page
    /// withholding + bump-cursor floor for the slide on ZGC -- see
    /// [`Self::supports_jit_tlab_skip`]).
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        match self {
            VmHeap::Generational(h) => h.set_jit_tlab_skip_regions(regions),
            VmHeap::G1(h) => h.set_jit_tlab_skip_regions(regions),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.set_jit_tlab_skip_regions(regions),
        }
    }

    /// BUG-03 / INT-3 — clear any published JIT TLAB skip regions.
    pub fn clear_jit_tlab_skip_regions(&self) {
        match self {
            VmHeap::Generational(h) => h.clear_jit_tlab_skip_regions(),
            VmHeap::G1(h) => h.clear_jit_tlab_skip_regions(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.clear_jit_tlab_skip_regions(),
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
    /// Resolve `addr` to the base of the object it points into, for PINNING.
    ///
    /// More permissive than [`Self::is_heap_addr`]: it accepts a MISALIGNED
    /// interior pointer and a ONE-PAST-THE-END cursor, both of which that
    /// method rejects and both of which a frozen peer's registers hold. See
    /// `ZgcRealHeap::resolve_interior_for_pin`. Non-ZGC arms fall back, so this
    /// is a no-op there.
    pub fn resolve_interior_for_pin(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.resolve_interior_for_pin(addr),
            other => other.is_heap_addr(addr),
        }
    }

    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.is_heap_addr(addr),
            VmHeap::G1(h) => h.is_heap_addr(addr),
            // ZGC: apply this method's own stated screen — *alignment* +
            // containment — before entering the backend, because
            // `ZgcRealHeap::is_heap_addr` (`zgc.rs:1902`) is the one
            // implementation that performs neither test. Both shipping
            // backends open with this exact line (`gen_heap.rs:3293`,
            // `g1.rs:7601`); ZGC goes straight to `registry.lock()`.
            //
            // Why it matters more here than there: on ZGC a *miss* is not
            // cheap. After the exact-base hash probe misses, `is_heap_addr`
            // drops the registry lock, RE-TAKES it, and walks the entire live
            // registry dereferencing each base's header to test extents —
            // O(live) per probe, under the mutex (`zgc.rs:1908-1919`). The
            // callers are per-slot conservative root scanners over ambiguous
            // JVM-long-vs-jobject operand slots (`value_stack.rs:1301`,
            // `:1587`, `memory/roots.rs:200`, `frame.rs:1831`,
            // `interpreter/gc_and_alloc.rs:4260`), whose dominant population
            // is zeros, small integers and long bit patterns — every one of
            // which currently buys a full walk of the heap.
            // `zgc-vmheap-arm-audit.md` §3.4 (AW-5), one of the two
            // instrument-separable hypotheses for the 35 PASS→HANG classes in
            // `zgc-real-fullsuite-regression-RETIRED-20260807.md`.
            //
            // Why it cannot lose a root. The guard only ever returns `None`
            // sooner; it can never turn a `None` into a `Some`, so no interior
            // address can be promoted to a base by it.
            //   * `addr == 0`: no live base is 0 and `0 >= base` is false for
            //     every base, so the extent walk already answered `None`. Pure
            //     work elimination, bit-identical result.
            //   * misaligned: every ZGC allocation base is 8-aligned
            //     (`alloc_raw` calls `arena.alloc(size, 8)`, `zgc.rs:1775`),
            //     so no *base* is reachable this way and none can be dropped.
            //     Only a misaligned *interior* word could previously have been
            //     rooted, and that is a word Generational and G1 have rejected
            //     since this method existed — this arm converges on the
            //     contract, it does not invent one.
            //
            // See the `TODO(zgc)` on `conservative_addr_span` above: once
            // `ZgcRealHeap` publishes its arena envelope, the range compare
            // belongs here too, ahead of the lock.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                if addr == 0 || addr & 0x7 != 0 {
                    return None;
                }
                h.is_heap_addr(addr)
            }
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
    /// Shared body of [`Self::load_and_forward`],
    /// [`Self::load_and_forward_checked`] and
    /// [`Self::load_and_forward_validated`].
    ///
    /// `pre_validated` is the caller's promise that `is_object_address` has
    /// ALREADY answered `Some` for `obj` on this heap with no intervening
    /// safepoint - the same contract as [`Self::class_id_of_validated`]. It
    /// suppresses the ENTRY walk only; every other check below is unchanged,
    /// including the forwarding-target validation.
    ///
    /// The returned flag says whether the reference handed back is a
    /// **validated live base**. Only a walk that actually happened inside
    /// this call may set it: the `forwarded_after_slide` answer is reported
    /// UNvalidated even though the relocation table only holds live bases,
    /// because that keeps the flag's meaning to one sentence a caller can
    /// check rather than a chain of invariants it has to trust.
    /// Rate limiter for the stale-barrier census in
    /// [`Self::load_and_forward_inner`], keyed by CALL SITE.
    ///
    /// Returns true at most once per distinct Rust backtrace, and at most
    /// `MAX_SITES` times overall. `CRATONVM_DBG_VACATED_FRAMES` only -- the
    /// capture alone is far too expensive for any other run.
    #[cold]
    #[inline(never)]
    fn stale_barrier_site_is_new() -> bool {
        use std::collections::HashSet;
        use std::hash::{Hash, Hasher};
        const MAX_SITES: usize = 40;
        static SEEN: std::sync::Mutex<Option<HashSet<u64>>> = std::sync::Mutex::new(None);
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bt.hash(&mut h);
        let key = h.finish();
        let mut g = match SEEN.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let set = g.get_or_insert_with(HashSet::new);
        if set.len() >= MAX_SITES {
            return false;
        }
        set.insert(key)
    }

    #[inline]
    fn load_and_forward_inner(&self, obj: ObjectRef, pre_validated: bool) -> (ObjectRef, bool) {
        // KINDOF-SENTINEL: `obj` itself has been observed already invalid
        // here (not merely forwarded-to-garbage — see the forwarding-target
        // check below) when the caller's own value was reconstructed across
        // a JIT bail-to-interpreter boundary with a live JIT frame the
        // collector couldn't get a precise root map for. There is no valid
        // object to hand back in that case; returning `obj` unchanged is a
        // no-op, not a fix, but it at least avoids adding a SECOND
        // dereference on top of the one about to fail below, and every
        // reproduced crash site downstream (`kind_of`, `class_id_of`,
        // `element_type_of`, `identity_hash_code`) now validates its own
        // input independently — see those methods below.
        // `CRATONVM_DBG_VACATED_FRAMES`: was this barrier just asked to repair a
        // reference the collector really did move, and did it fail?
        //
        // This barrier repairs by reading a FORWARDING WORD at the OLD address.
        // ZGC's slide has none to read: `Arena::compact_low_to` zeroes the span
        // above the new cursor and the memmove overwrites everything below it,
        // so a stale reference either lands on zeroed bytes (not a registered
        // base — the early return right below) or on another live object (whose
        // header is not forwarded — the `is_forwarded` return after it). Every
        // caller that treats this call as the repair for "a collection may have
        // run since the frame read" is therefore unprotected on the DEFAULT
        // collector. The ledger is exact (`gc_quiescence::note_allocated`
        // removes re-issued addresses), so a hit here is proof, not a
        // suspicion.
        if crate::gc_quiescence::vacated_frames_enabled() {
            if let Some(moved_to) = crate::gc_quiescence::was_vacated(obj.as_ptr() as usize) {
                // ONE REPORT PER CALL SITE, not per occurrence.
                //
                // This barrier is the choke point every raw `ObjectRef` a
                // native still holds passes through -- `forward_boundary_value`
                // for `set_field` / `set_array_element`, `forward_boundary_args`
                // for the `invoke_*` family -- so a hit names a native that
                // captured a reference before an allocation and used it after.
                // The population is a handful of distinct sites hit thousands
                // of times each, and a flat count of 12 reported the first site
                // twelve times and every other one never. Keyed by the
                // backtrace so the census is of SITES.
                if Self::stale_barrier_site_is_new() {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = format!("{:#x}", obj.as_ptr() as usize),
                        moved_to = format!("{moved_to:#x}"),
                        backtrace = %std::backtrace::Backtrace::force_capture(),
                        "load_and_forward was handed a reference the collector MOVED, and                          cannot repair it: this collector leaves no forwarding word at the                          vacated address. The caller in the backtrace is holding a stale                          ObjectRef that nothing else will fix.",
                    );
                }
            }
        }
        if !pre_validated && self.is_object_address(obj.as_ptr() as usize).is_none() {
            // Not a live base. On a collector that leaves a forwarding word
            // this is the end of the road; ZGC's slide leaves none, so ask its
            // relocation table instead — that is the whole point of
            // `ZgcRealHeap::relocations`, and without it this barrier is a
            // silent no-op on the default collector.
            #[cfg(feature = "zgc")]
            if let VmHeap::Zgc(h) = self {
                if let Some(moved_to) = h.forwarded_after_slide(obj.as_ptr() as usize) {
                    // SAFETY: `forwarded_after_slide` only answers with an
                    // address the object-start registry currently holds, i.e. a
                    // live object base inside the arena.
                    return (unsafe { ObjectRef::from_raw(moved_to as *mut u8) }, false);
                }
            }
            return (obj, false);
        }
        // SAFETY: the caller guarantees `obj` is a live root. Every
        // current backend lays out `ObjectHeader` at offset 0 of the
        // ObjectRef pointer with `forwarding_ptr` at the documented
        // structural offset; reading the field is well-formed.
        let header = unsafe { &*(obj.as_ptr() as *const crate::heap::ObjectHeader) };
        if !header.is_forwarded() {
            return (obj, true);
        }
        let addr = header.forwarding_address();
        if addr.is_null() {
            // Defensive fallback (shouldn't happen — is_forwarded()
            // already checks for a non-null pointer — but kept for
            // belt-and-braces against concurrent-GC races we did not
            // anticipate). Returning the original pointer is always
            // safe because the original object still exists in memory
            // until the evacuation epoch ends.
            return (obj, true);
        }
        // KINDOF-SENTINEL: `header` itself may be corrupted (the same
        // implausible-header family `old_gen::scan_region` guards against —
        // see `validate_header_tags_or_desync`), and unlike `kind_tag`/
        // `elem_tag`, the forwarding-pointer word was never validated
        // before this call trusted it outright. A corrupted header can set
        // the forwarded bit and hand back a garbage `forwarding_address()`
        // — observed as the all-ones sentinel `0xFFFFFFFFFFFFFFFF` — which
        // every caller of this read barrier then dereferences unchecked
        // (`kind_of`, `get_field`, `array_length`, ...). Validate the
        // extracted address is actually a live object in this heap before
        // trusting it; an implausible target falls back to the original
        // pointer, exactly like the null-address case above.
        if self.is_object_address(addr as usize).is_none() {
            return (obj, true);
        }
        // SAFETY: the forwarding pointer was installed by the GC and
        // points at a valid object header within this heap.
        (unsafe { ObjectRef::from_raw(addr) }, true)
    }

    /// The software read barrier: repair `obj` if the collector moved it.
    ///
    /// This is **not** the ZGC colored-word load barrier, and it cannot be
    /// taught to be one: its parameter is an [`ObjectRef`] -- a machine
    /// pointer the caller has already fabricated -- not a reference SLOT, so
    /// a colored word never reaches it in a form it could repair. Handed one
    /// it would build an `ObjectRef` out of bit-63 bits, fail
    /// [`Self::is_object_address`], fall through the `forwarded_after_slide`
    /// lookup and return the same word unchanged: a silent no-op that also
    /// violates `zgc::vaddr::debug_assert_plain_word`. The colored-word
    /// barrier is [`Self::load_ref_slot_barriered`], which takes the slot.
    #[inline]
    pub fn load_and_forward(&self, obj: ObjectRef) -> ObjectRef {
        self.load_and_forward_inner(obj, false).0
    }

    /// [`Self::load_and_forward`], also reporting whether the reference it
    /// hands back is a validated live base.
    ///
    /// `load_and_forward` returns its argument UNCHANGED when validation
    /// fails, so its result is not safe to pass to a `_validated` twin - the
    /// twin would dereference a pointer nothing has checked. This variant
    /// hands the caller the one bit needed to tell the two cases apart, and
    /// costs nothing: the walk that decides it has already happened inside.
    #[inline]
    pub fn load_and_forward_checked(&self, obj: ObjectRef) -> (ObjectRef, bool) {
        self.load_and_forward_inner(obj, false)
    }

    /// [`Self::load_and_forward`] for a caller that has **just** validated
    /// `obj` through [`Self::is_object_address`] and still holds it.
    ///
    /// Skips the entry walk only. The result is always a validated live base:
    /// with the entry branch suppressed, every remaining exit either returns
    /// the caller's already-validated `obj` or a forwarding target this call
    /// validated itself.
    ///
    /// # Contract
    ///
    /// As [`Self::class_id_of_validated`]: `is_object_address` must have
    /// answered `Some` for `obj`, on this heap, with no intervening safepoint.
    #[inline]
    pub fn load_and_forward_validated(&self, obj: ObjectRef) -> ObjectRef {
        self.load_and_forward_inner(obj, validate_once_enabled()).0
    }

    /// Read a heap reference **SLOT** through this backend's load barrier.
    ///
    /// The value returned is a plain machine address (`0` = null) -- exactly
    /// what [`cratonvm_types::narrow_oop::read_ref_slot`] returns on a
    /// non-colored backend. A caller may therefore still run
    /// `plausible_heap_pointer` afterwards, and MUST run it on the *result*
    /// rather than on the slot word: a colored word is deliberately
    /// implausible, so filtering the word first is what silently nulls a live
    /// reference (risk J1 of `docs/feature-designs/zgc-jit-load-barrier.md`).
    ///
    /// # Why this exists at all, and why it is here and not in `vm/`
    ///
    /// The seven raw reference reads in `vm/src/jit/helpers.rs` panic as a
    /// tripwire on a colored word instead of barriering it. The two of them
    /// that still hold a SLOT when the plausibility filter runs -- the
    /// reference-array element load and the compact-reference field load --
    /// now funnel through `helpers::jit_load_ref_slot`, and this is the
    /// function that seam calls. It could not be written in `vm/`:
    /// `zgc::barrier::z_load` needs a `C: ZBarrierContext`, whose only
    /// production implementor is `ZgcRealHeap`, and `VmHeap` publishes no
    /// accessor that hands the context out. Dispatching here keeps every ZGC
    /// detail inside `gc/` instead of growing a per-site ZGC special case,
    /// which is the shape that already missed `emit_load_string_value_ptr`
    /// once.
    ///
    /// # What each arm does
    ///
    /// * `Generational` / `G1`: today's `read_ref_slot`, byte for byte.
    ///   Neither collector has a load barrier and neither may gain one here.
    /// * `Zgc`, barrier NOT armed: the same plain read. This is the only path
    ///   any shipping configuration takes -- see "Today it cannot fire".
    /// * `Zgc`, barrier armed: [`crate::zgc::ZgcRealHeap::load_barrier_slot`],
    ///   which is the one in-tree implementation of the colored-word barrier
    ///   and already gets right the two conversions a fresh one gets wrong. It
    ///   views the slot as an `AtomicU64`, runs
    ///   `barrier::load_barrier_fast_bad`, and on a bad color calls
    ///   `load_barrier_slow` (forward the offset, publish to the marker,
    ///   CAS-heal the slot). Crucially it then converts **offset to address**:
    ///   `z_load` hands back a bare 42-bit heap OFFSET, not a pointer, and
    ///   returning it uncorrected is a silent truncation that
    ///   `gc/src/zgc/relocate.rs` and `gc/src/zgc/mark.rs` both already carry
    ///   doc comments warning about. Re-deriving that conversion here rather
    ///   than delegating would be a second copy of exactly the knowledge those
    ///   comments say must live in one place.
    ///
    /// # TODAY IT CANNOT FIRE -- the armed arm is dead code
    ///
    /// No colored word is stored in a heap slot in any shipping
    /// configuration, so landing this changes nothing measurable:
    ///
    /// * `vm/src/vm/vm_init.rs` pins `const RELOCATION_REQUESTED: bool =
    ///   false`.
    /// * `ZgcRealHeap::set_barrier_color` -- the sole writer of the colored
    ///   state -- has no non-test caller. Its own comment records the
    ///   obligation on the first one.
    /// * `barrier_good_mask` is initialised to `vaddr::Z_REMAPPED` and nothing
    ///   moves it, so `load_barrier_armed()` is false for the process
    ///   lifetime and `zgc::vaddr::color` has no production caller.
    ///
    /// The unarmed cost is therefore one relaxed `AtomicBool` load and a
    /// not-taken branch on top of the read that already happened.
    ///
    /// # P1 -- SLOT WIDTH: compressed oops REFUSE the barrier, they do not get one
    ///
    /// `z_load` takes `&AtomicU64`, which is a promise about the SLOT: 8-byte
    /// aligned, exactly 8 bytes, and valid for WRITES, because the slow path
    /// self-heals with `slot.compare_exchange(observed, healed, AcqRel,
    /// Acquire)`. With `narrow_oops_enabled()` the slot is FOUR bytes
    /// (`read_ref_slot` branches on exactly that), so the `AtomicU64` view
    /// would read and CAS four bytes of the neighbouring field -- the same
    /// class of bug `emit_load_string_value_ptr` had to be fixed for once.
    ///
    /// A 4-byte path was considered and REJECTED, not deferred: a colored word
    /// is `Z_COLORED_TAG | color | 42-bit offset`, i.e. bit 63 plus bits 42-46
    /// plus a 42-bit payload. It does not fit in 32 bits under any encoding,
    /// so there is no narrow colored word for a narrow barrier to operate on.
    /// ZGC plus compressed oops is unsupported until the slot representation
    /// itself changes, which is `zgc-reference-slot-representation.md`'s
    /// problem and not this function's.
    ///
    /// So the narrow arm REFUSES. Unarmed it takes the same plain
    /// `read_ref_slot` as everything else (identical behaviour, and the only
    /// reachable case). Armed it panics, deliberately: the alternative is to
    /// hand compiled code a truncated colored word, and this subsystem's house
    /// rule -- the same one that makes `ZBarrierContext::on_forward_failure`
    /// return `!` -- is that an unrepresentable reference fails loudly rather
    /// than degrading to a wrong pointer. That panic is unreachable twice
    /// over: `vm/src/vm/vm_init.rs` already refuses the ZGC + compressed-oops
    /// combination at startup, and nothing arms the barrier. It is asserted
    /// here anyway because `narrow_oops_enabled()` reads a process-global
    /// `AtomicBool` that any code can set, so the init-time gate is a fact
    /// about a default run and not an invariant of this call -- the hardening
    /// `zgc-reference-slot-representation.md` asks for.
    ///
    /// # P3 -- offset 0 / null ambiguity: answered, with one residual
    ///
    /// `ZFastPath::Good(0)` is ambiguous between null and an object at heap
    /// offset 0 (`vaddr::color_offset_roundtrip_many_offsets` asserts offset 0
    /// is a legal non-null location). `load_barrier_slot` disambiguates it the
    /// way `barrier.rs` says a Rust caller can and machine code cannot: it
    /// re-reads the raw word and answers `None` only for `vaddr::Z_NULL`. So
    /// the decision is made once, here, and not per caller.
    ///
    /// RESIDUAL for whoever arms this: that null test is a SECOND load, so a
    /// mutator store landing between the barrier's load and it can be observed
    /// as "offset 0" rather than null, yielding the arena base instead of `0`.
    /// It is harmless while nothing is armed and it is not fixable without
    /// `ZFastPath::Good` carrying the raw word alongside the offset. The
    /// durable fix is open question 8 of `zgc-jit-load-barrier.md`: reserve
    /// offset 0 in the page allocator so the ambiguity has no legal instance.
    /// Risk J6 of that document is the same fact seen from the JIT side.
    ///
    /// # P4 -- `on_forward_failure` returns `!`: NOT handled here, and cannot be
    ///
    /// A `ZBarrierContext::forward` that answers `None` panics rather than
    /// returning. That is a property of `ZgcRealHeap::forward`, not of this
    /// call: it returns `Some(addr)` unconditionally while `relocate_active`
    /// is false, which is always, so the panic is unreachable today. It stops
    /// being unreachable the moment relocation is real and the forwarding
    /// table can miss, and no wrapper here can turn it into a recoverable
    /// answer -- the `!` return type is the whole point. Whoever makes
    /// relocation real owns that decision at `ZgcRealHeap::forward`.
    ///
    /// # Ordered work list -- THIS FILE IS THE AUTHORITY
    ///
    /// Folded in from `.agent-requests/A9-gc-barrier.txt`. These are the steps
    /// that must complete, in order, before `vm/src/vm/vm_init.rs`'s
    /// `RELOCATION_REQUESTED` may be flipped.
    ///
    /// **Status changes go HERE and nowhere else.**
    /// `gc/src/zgc/census.rs`'s `ZSlotShape::word_is_atomically_accessed_today`
    /// carried a second copy of this sequence until 2026-09-01. The two drifted
    /// apart inside three weeks and ended up each describing the other as the
    /// stale one, which is what two copies of an ordered sequence buy. That
    /// copy is now a pointer to this list plus the per-shape facts only it
    /// knows; do not start a third. If another file needs the status, cite this
    /// item.
    ///
    /// Status as of 2026-09-01. Every label below is a one-command check and
    /// the command is named; re-run it rather than trusting the label.
    ///
    /// 1. **DONE for the Rust writers (2026-09-01).** Every other writer of a
    ///    slot this barrier may CAS must be atomic: a plain write racing
    ///    `load_barrier_slow`'s `compare_exchange` on one location is a data
    ///    race, and the defect is the NON-ATOMICITY, not the ordering.
    ///    `cratonvm_types::narrow_oop::read_ref_slot` / `write_ref_slot` are
    ///    now `Relaxed` atomics in the wide (`AtomicU64`) and narrow
    ///    (`AtomicU32`) arms alike, with the four-part argument for `Relaxed`
    ///    rather than something stronger written out above them in
    ///    `types/src/narrow_oop.rs`. This item quoted a plain
    ///    `(ptr as *mut u64).write(addr)` until 2026-09-01; that write no
    ///    longer exists, and `grep -n 'mut u64).write' types/src/narrow_oop.rs`
    ///    is the check.
    ///    Still open under this heading, and tracked on the `LegacyField` row
    ///    of `zgc::census::ZSlotShape::atomicity_debt_note`: the collector-side
    ///    16-byte `Value` writers in `gc/src/gc.rs`, `gc/src/gen_heap.rs` and
    ///    `gc/src/g1.rs` are still plain `ptr::write` / `ptr::write_unaligned`.
    ///    Tracked, not blocking -- those are the Generational and G1 evacuation
    ///    loops, which never run over a `ZgcRealHeap`, so they are not slots
    ///    this barrier can reach and CAS.
    ///    The JIT-emitted inline reference stores under `jit/src/x64/` are NOT
    ///    this item's problem and never were: machine code is not a Rust memory
    ///    access, an aligned qword `mov` cannot tear against a `lock cmpxchg`,
    ///    and no Rust UB is in play. What they have is a COVERAGE obligation,
    ///    which is step 6.
    /// 2. **DONE.** The armed test:
    ///    [`crate::zgc::ZgcRealHeap::load_barrier_armed`] already existed, so
    ///    no new accessor was needed. Only its slot helper had to widen from
    ///    private to `pub(crate)`.
    /// 3. **DONE.** This function, with P1/P3/P4 decided above.
    ///    **Extended, DONE (2026-09-01), inside `gc/`:** the three
    ///    ZGC-internal accesses to a word the barrier would CAS --
    ///    `ZgcRealHeap::relocate_stw`'s compaction slot-rewrite STORE, and the
    ///    legacy payload READS in `ZgcRealHeap::visit_strong_refs_at` and in
    ///    `zgc::census::reference_slots` (all `gc/src/zgc.rs`). The compaction
    ///    store is the one that mattered: its SAFETY note rested on "the world
    ///    is stopped", which is precisely the property step 7 removes.
    /// 4. **HALF DONE.** `vm/`: route `helpers::jit_load_ref_slot`'s
    ///    `read_ref_slot(slot)` through this call. That seam is the single
    ///    chokepoint and both slot-holding sites funnel through it; it now
    ///    calls `VmHeap::load_ref_slot_barriered` on its `Some(&VmHeap)` arm
    ///    and falls back to a raw `read_ref_slot` on its `None` arm.
    ///    Site B, the compact-reference field load, is DONE (2026-09-01):
    ///    `jit_getfield` takes `vm_ptr` and has `vm` bound already, so it
    ///    passes `Some(&vm.mem.heap)` and dispatches here.
    ///    Site A, `jit_aaload`, is NOT, and cannot be without an ABI change --
    ///    it receives no `vm_ptr` and so has no route to a `&VmHeap`, and takes
    ///    the seam's raw arm. The exact change is written out in
    ///    `.agent-requests/B8-abi.txt` and is IN FLIGHT, not landed; check the
    ///    signature (`grep -n 'fn jit_aaload' vm/src/jit/helpers.rs`) before
    ///    believing either state. The census
    ///    `ref_load_census::COLORED_WORDS_SEEN` is what proves afterwards that
    ///    no Category-A site was missed.
    /// 5. **NOT DONE.** Sites D/E/F of `zgc-jit-load-barrier.md` 2.5.1, which
    ///    hold an `ObjectRef` rather than a slot: their barriers belong
    ///    upstream at `types/src/value.rs`'s `read_value_atomic` reference arm
    ///    and at `vm::get_static_shared`, where they are shared with the
    ///    interpreter rather than duplicated. A static slot is not atomic
    ///    today and so may not be CAS-healable -- it may need a non-healing
    ///    barrier kind. Blocked on the `StaticField` row of
    ///    `zgc::census::ZSlotShape::word_is_atomically_accessed_today`.
    /// 6. **DONE for the emitters (2026-09-01); the residual it names is not
    ///    closable here.** The nine Category-A inline emission points of 2.3
    ///    are kept routed to the helpers by
    ///    `x64::narrow_oops_block_inline_fields()`, which is
    ///    `narrow_oops_enabled() || zgc_read_barrier_blocks_inline_fields()`
    ///    (`jit/src/x64/licm.rs`). `aastore` was the one site that emitted the
    ///    slot load and the element store inline without consulting it -- not
    ///    the UB of step 1 but a coverage hole, since an armed cycle would read
    ///    a coloured word with no colour test and write a plain pointer into a
    ///    slot the barrier next classifies as `Good` and truncates to 42 bits.
    ///    That gate landed at the `0x53` arm of
    ///    `jit/src/x64/bytecode_walk.rs`, with `AASTORE_SITES_WALKED` as the
    ///    denominator that makes its expected ZERO fallback count readable as
    ///    "consulted and correctly declined" rather than "never reached".
    ///    The residual: an emission-time gate cannot reach ALREADY COMPILED
    ///    sequences, so arming must happen at a safepoint. That is step 7's
    ///    obligation, and it is recorded on `ZgcRealHeap::set_barrier_color`.
    /// 7. **NOT DONE.** Only then flip `RELOCATION_REQUESTED`.
    ///
    /// # Safety
    ///
    /// `slot` must point at a live reference slot of the current width
    /// ([`cratonvm_types::narrow_oop::ref_field_size`]) inside a live object --
    /// the same contract as `read_ref_slot`. On the armed ZGC path the slot
    /// must additionally be valid for WRITES, because the barrier self-heals
    /// it; every reference slot inside a live heap object is.
    #[inline]
    pub unsafe fn load_ref_slot_barriered(&self, slot: *const u8) -> u64 {
        // Written as an early return on the one arm that differs rather than
        // as a `match`, so the Generational/G1/unarmed-Zgc answer is LITERALLY
        // the expression `jit_load_ref_slot` evaluates today. A `match` with
        // three arms spelling the same call is three places for them to drift
        // apart, and "landing this changes nothing measurable" has to be
        // checkable by reading rather than by benchmarking.
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            return Self::zgc_load_ref_slot_barriered(h, slot);
        }
        cratonvm_types::narrow_oop::read_ref_slot(slot)
    }

    /// The `Zgc` arm of [`Self::load_ref_slot_barriered`]. Split out so the
    /// common path above stays one branch and one call, and so the ZGC
    /// preconditions sit next to the code that depends on them.
    ///
    /// # Safety
    ///
    /// As [`Self::load_ref_slot_barriered`].
    #[cfg(feature = "zgc")]
    #[inline]
    unsafe fn zgc_load_ref_slot_barriered(h: &ZgcRealHeap, slot: *const u8) -> u64 {
        // P1. Order matters: test the WIDTH before the armed flag, so the
        // unsupported combination is refused rather than silently reading and
        // CAS-ing 8 bytes out of a 4-byte slot. The unarmed narrow read below
        // is the ordinary one, which is what keeps a compressed-oops run
        // byte-identical.
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            assert!(
                !h.load_barrier_armed(),
                "ZGC colored-pointer load barrier armed while compressed oops are enabled: \
                 the reference slot is 4 bytes and a colored word does not fit in 32 bits \
                 (Z_COLORED_TAG is bit 63). vm_init refuses this combination at startup and \
                 set_barrier_color has no non-test caller, so reaching here means one of \
                 those two facts changed without this function being revisited. Refusing \
                 rather than truncating -- see load_ref_slot_barriered, section P1."
            );
            return cratonvm_types::narrow_oop::read_ref_slot(slot);
        }
        if !h.load_barrier_armed() {
            // The only path any shipping configuration takes.
            return cratonvm_types::narrow_oop::read_ref_slot(slot);
        }
        // `AtomicU64` requires 8-byte alignment, and `compare_exchange` on a
        // misaligned address is UB rather than a slow path. Every reference
        // slot is 8-aligned by construction (`HEADER_SIZE` and `SLOT_SIZE` are
        // both multiples of 8, and `ARRAY_DATA_OFFSET` likewise); this is a
        // debug assert because it is an invariant of the layout, not an input
        // to be validated on every load.
        debug_assert_eq!(
            slot as usize % 8,
            0,
            "reference slot must be 8-byte aligned for the AtomicU64 view the load barrier takes"
        );
        // Delegates the color test, the slow-path heal, the null
        // disambiguation (P3) and the OFFSET -> ADDRESS conversion. `None` is
        // null, which this signature spells `0`, matching `read_ref_slot`.
        h.load_barrier_slot(slot as usize).map_or(0, |a| a as u64)
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

    // KINDOF-SENTINEL (2026-08-04): `kind_of`/`element_type_of`/
    // `identity_hash_code` are the innermost dispatch point for the whole
    // `#[repr(u8)]`-header-validation family this file's `old_gen`/
    // `gen_heap` siblings already guard on the GC-internal walk side (see
    // `validate_header_tags_or_desync`). Those fixes hardened the
    // COLLECTOR's own scan of old-gen; they never touched this VM-level
    // read barrier, which every MUTATOR path (interpreter dispatch, JIT
    // helpers, native array/field accessors) funnels through with a bare
    // `ObjectRef` and no independent check of its own. Reproduced: a JIT
    // bail-to-interpreter transition on a thread whose innermost frame
    // belonged to an unguarded/unregistered JIT callee (no precise root
    // map for that root-gathering pass) handed one of these a receiver
    // that read back as the all-ones sentinel `0xFFFFFFFFFFFFFFFF` —
    // `EXCEPTION_ACCESS_VIOLATION` reading exactly that address, at three
    // distinct call sites (`kind_of` itself via
    // `dispatch_virtual::execute_invokevirtual_cached`, `class_id_of` via
    // `NativeHeapAccess::get_field`, and `load_and_forward` via a fourth).
    // `load_and_forward` (above) now validates the forwarding target it
    // hands back, but a corrupted header can also be reached directly
    // without ever going through that barrier, so each of these validates
    // independently rather than trusting an already-validated caller.
    pub fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        if self.is_object_address(obj.as_ptr() as usize).is_none() {
            self.note_dead_base_deref(obj, "kind_of");
            return ObjectKind::Object;
        }
        dispatch!(self, kind_of(obj))
    }

    /// [`Self::kind_of`] for a caller holding a validated `ObjectRef`.
    /// Same contract as [`Self::class_id_of_validated`].
    pub fn kind_of_validated(&self, obj: ObjectRef) -> ObjectKind {
        if !validate_once_enabled() {
            return self.kind_of(obj);
        }
        dispatch!(self, kind_of(obj))
    }

    /// [`Self::element_type_of`] for a caller holding a validated
    /// `ObjectRef`. Same contract as [`Self::class_id_of_validated`].
    pub fn element_type_of_validated(&self, obj: ObjectRef) -> ArrayElementType {
        if !validate_once_enabled() {
            return self.element_type_of(obj);
        }
        dispatch!(self, element_type_of(obj))
    }

    pub fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        if self.is_object_address(obj.as_ptr() as usize).is_none() {
            self.note_dead_base_deref(obj, "element_type_of");
            return ArrayElementType::Reference;
        }
        dispatch!(self, element_type_of(obj))
    }

    /// `CRATONVM_DBG_VACATED_FRAMES`: somebody just dereferenced an address
    /// that is **not a live object base**, and the sentinel guards above
    /// swallowed it into a default.
    ///
    /// This is the EARLY face of the stale-holder family, and the one every
    /// other instrument is blind to. A holder left naming an address the slide
    /// vacated reads a zeroed corpse until the allocator hands the span out
    /// again: `is_object_address` says no, `class_id_of` answers `ClassId(0)`,
    /// `kind_of` answers `Object`, and the caller carries on with a plausible
    /// default. Nothing is thrown, so nothing is reported — and by the time the
    /// address IS re-issued and the failure becomes visible as a
    /// `ClassCastException`, the exact vacated ledger has already dropped the
    /// entry (that pruning is what makes it exact) and every consumption-point
    /// detector goes quiet. That gap is why the H2 MVStore-writer residual
    /// could be measured, cornered to "a raw ObjectRef in VM-side state", and
    /// still not named.
    ///
    /// The backtrace is the whole point: it names the Rust frame holding the
    /// reference, which is the one fact none of the Java-side evidence carries.
    /// `moved_to` distinguishes the two reasons an address fails the live-base
    /// test — the collector moved the object (a stale holder, this defect) or
    /// the value was never an object at all (the `0xFFFF..` sentinel family the
    /// guards above were originally written for).
    #[cold]
    fn note_dead_base_deref(&self, obj: ObjectRef, site: &'static str) {
        if !crate::gc_quiescence::vacated_frames_enabled() {
            return;
        }
        let addr = obj.as_ptr() as usize;
        // Null is not a stale holder; it is the ordinary absent reference, and
        // reporting it would bury the signal.
        if addr == 0 {
            return;
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 24 {
            return;
        }
        let moved_to = crate::gc_quiescence::was_vacated(addr)
            .or_else(|| self.zgc_forwarded_after_slide(addr));
        tracing::error!(
            target: "cratonvm::gc::guard",
            site,
            obj = format!("{addr:#x}"),
            moved_to = moved_to.map(|a| format!("{a:#x}")).unwrap_or_else(|| "<unknown>".into()),
            was_vacated = moved_to.is_some(),
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "a dereference of an address that is NOT a live object base was \
             swallowed into a default. When `was_vacated` is true this is a \
             holder the collector moved out from under and nothing repaired — \
             the caller in the backtrace is the one holding it.",
        );
    }

    /// ZGC's slide ledger, for [`Self::note_dead_base_deref`]. `None` on every
    /// other backend (they leave a forwarding word instead, which
    /// `load_and_forward` reads).
    fn zgc_forwarded_after_slide(&self, addr: usize) -> Option<usize> {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.forwarded_after_slide(addr),
            _ => None,
        }
    }

    pub fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        // KINDOF-SENTINEL: see `kind_of` above.
        if self.is_object_address(obj.as_ptr() as usize).is_none() {
            return 0;
        }
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
        // `CRATONVM_DBG_VACATED_FRAMES`: is the RECEIVER of this read an
        // address the collector moved an object away from?
        //
        // This is the consumption point neither the stack-push nor the
        // frame-local detector can see, and the one the H2 residual's surviving
        // witnesses point at: a stale receiver makes every field read off it
        // return whatever now occupies that memory — a perfectly VALID object
        // of the wrong class, which is why the value being pushed looks clean
        // and the `checkcast` one instruction later does not.
        crate::gc_quiescence::report_vacated_receiver(obj.as_ptr() as usize, "get_field");
        // The re-issue-proof half of the same question — see
        // `gc_quiescence::stale_use_verdict` for why the exact ledger above
        // cannot answer it.
        crate::gc_quiescence::check_stale_use(obj.as_ptr() as usize, "get_field receiver");
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

    /// INT-8: field store with the SATB pre-barrier SUPPRESSED. Reserved
    /// for the weak-reference PROTOCOL writes (the pre-collection referent
    /// null pass and the remark-time referent clears): those are not
    /// semantic overwrites, and SATB-logging them recorded every active
    /// referent as a mark root — the taint that made bitmap-based reference
    /// processing inert (see `G1Collector::set_field_no_satb`).
    ///
    /// **ZGC joined the suppressed set on 2026-08-16, and it had to.** This was
    /// a plain `set_field` on that backend, correctly, for as long as ZGC had
    /// no concurrent cycle and no armed pre-write barrier of its own. Genuine
    /// concurrent marking gave it both, and `ZgcRealHeap::set_field` now
    /// publishes the overwritten reference itself — so the unsuppressed arm
    /// would have handed the concurrent marker EVERY active referent as a mark
    /// root at the pre-collection null pass, which runs while the cycle is
    /// still armed. No weak, soft, phantom or cleaner reference would ever
    /// have been cleared again, and no reference test would have caught it:
    /// they are all satisfied by "the referent survived".
    ///
    /// Generational stays a plain `set_field` — its reference protocol never
    /// depended on hiding these writes (it uses the watched-referents channel).
    pub fn set_field_suppress_satb(&self, obj: ObjectRef, index: usize, value: Value) {
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        match self {
            VmHeap::G1(h) => h.collector.set_field_no_satb(obj, index, value),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.set_field_no_satb(obj, index, value),
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
        if header.kind() == ObjectKind::Array {
            Some(header.element_type())
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
            // ZGC pins the ADDRESS and returns it as its own "region index":
            // this collector has no regions, and the slide's filter is
            // page-granular over addresses. Returning `Vec::new()` was correct
            // only while this collector never moved an object — see
            // `ZgcRealHeap::critical_pins` for what the copy-back at Release
            // does to a moved array.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                let addr = obj.as_ptr() as usize;
                h.pin_critical(addr);
                vec![addr]
            }
        }
    }

    /// Release a pin set taken by [`Self::pin_critical_region`] at
    /// `GetPrimitiveArrayCritical`. No-op on the generational collector / for an
    /// empty set.
    pub fn unpin_critical_regions(&self, region_indices: &[usize]) {
        match self {
            VmHeap::G1(h) => {
                for &idx in region_indices {
                    h.unpin_region(idx);
                }
            }
            // For ZGC the "index" IS the pinned object address — see
            // `pin_critical_region`'s ZGC arm.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                for &addr in region_indices {
                    h.unpin_critical(addr);
                }
            }
            _ => {}
        }
    }

    /// Read an array element with auto-unboxing of wrapper types.
    ///
    /// Only the generational collector needs a separate entry point: G1 and ZGC
    /// un-box inside their own `get_array_element`, so routing them here would
    /// double-decode nothing and the plain accessor already satisfies this
    /// method's contract. Both arms below are therefore un-boxing reads, not
    /// fallbacks — ZGC's was a genuine fallback until 2026-08-09, which is what
    /// made `Stream.mapToLong(...).toArray()` return zeros under `-XX:+UseZGC`.
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
        Some(unsafe { obj.as_ptr().add(ARRAY_DATA_OFFSET) })
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
    /// `g1-native-alloc-no-safepoint-oom-FIXED.md`).
    ///
    /// ZGC: the same defect was live here, verbatim. `ZgcRealHeap` is an
    /// infallible allocator too — `alloc_object` and `alloc_array` end in
    /// `eprintln!("FATAL: ZGC(real): out of heap space …"); std::process::abort()`
    /// (`ZgcRealHeap`'s `GarbageCollector` impl) — and this arm answered a
    /// hardwired `false`, so the one hook that can run a collection on behalf of
    /// a native (`safe_native_call_impl`, `vm/src/vm/vm_exec.rs`) never fired on
    /// this backend. An
    /// allocate-only-from-natives workload therefore reached no safepoint at
    /// all and died by `abort()` on a heap full of garbage, with no Java-visible
    /// `OutOfMemoryError` ever thrown.
    ///
    /// 2026-08-07: this arm used to COMPUTE the signal from `h.needs_gc()`
    /// alone, carrying a `TODO(zgc)` that asserted `ZgcRealHeap` had no pressure
    /// field and that this file could not add one. That claim stopped being true
    /// the same day, and the stale comment was the only thing keeping the gap
    /// open: `zgc.rs` now owns a real `native_alloc_pressure: AtomicBool` on
    /// `ZgcRealHeap` — armed in `alloc_raw`, disarmed at the end of
    /// `collect_garbage` where `gc_rearm` is recomputed — behind the same
    /// `native_alloc_pressure()` / `clear_native_alloc_pressure()` /
    /// `note_native_alloc_pressure()` trio G1 exposes. The two halves of one fix
    /// were written from opposite ends and never met; these three arms are the
    /// join.
    ///
    /// The latch ADDS to the occupancy test rather than replacing it, which is
    /// where this deliberately differs from the G1 arm above (a bare latch
    /// read). G1 can afford that because `note_region_consumed_locked` re-arms
    /// on every region consumption below the threshold. Here, dropping
    /// `|| h.needs_gc()` would NARROW behaviour that is already load-bearing:
    /// the consumer (`vm/src/vm/vm_exec.rs`) clears unconditionally after
    /// acting — including when its own gates said no — so a just-cleared latch
    /// would answer `false` over a heap that is genuinely over its trigger. The
    /// disjunction keeps the `abort()` case covered by construction, and the
    /// latch adds the edge the occupancy test cannot see (a caller that noted
    /// pressure below the trigger).
    ///
    /// Neither term can recreate the `gc_rearm` GC storm. The latch is armed on
    /// `needs_gc`'s predicate VERBATIM — `allocated >= gc_threshold &&
    /// allocated >= gc_rearm`, inlined in `ZgcRealHeap::alloc_raw` — so it
    /// inherits the re-arm floor each collection raises to
    /// `live + max(headroom/4, 64 KiB)`, and can never ask for a collection
    /// `needs_gc` would refuse. The second term IS `needs_gc`, i.e. exactly what
    /// this arm answered before. And the consumer re-checks both
    /// `gc_overhead_limit_exceeded` and `needs_gc` before running anything, so
    /// even the externally-noted edge (which bypasses the heap's own predicate,
    /// by design) buys at most one gate evaluation per note.
    #[inline]
    pub fn young_spill_pressure(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.young_spill_pressure(),
            VmHeap::G1(h) => h.native_alloc_pressure(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.native_alloc_pressure() || h.needs_gc(),
        }
    }

    /// Whether an allocation was **refused** since the last collection.
    ///
    /// The hard twin of [`Self::young_spill_pressure`], and the difference is
    /// the whole point: the boundary consumer re-checks `needs_gc()` before
    /// acting on the soft signal, which is right for an advisory note and
    /// wrong for a request that already failed. On a non-compacting heap those
    /// two states come apart completely — an arena can refuse a 2 MB array
    /// while `allocated` sits at 7% of capacity, because the bytes are there
    /// and no single hole is — and in that state `needs_gc()` answers no and
    /// discards the only signal that knew better.
    ///
    /// Non-ZGC backends answer `false`: Generational's own spill latch plus
    /// `old_gen_needs_gc()` already cover the same ground for it, and G1
    /// relocates, so "no hole this big" is not a durable state there.
    #[inline]
    pub fn hard_alloc_failure(&self) -> bool {
        match self {
            VmHeap::Generational(_) => false,
            VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.hard_alloc_failure(),
        }
    }

    /// Clear the hard-allocation-failure latch — see [`Self::hard_alloc_failure`].
    #[inline]
    pub fn clear_hard_alloc_failure(&self) {
        match self {
            VmHeap::Generational(_) => {}
            VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.clear_hard_alloc_failure(),
        }
    }

    /// Clear the native-wrapper allocation-pressure signal.
    #[inline]
    pub fn clear_young_spill_pressure(&self) {
        match self {
            VmHeap::Generational(h) => h.clear_young_spill_pressure(),
            VmHeap::G1(h) => h.clear_native_alloc_pressure(),
            // 2026-08-07: was a no-op, on the (by then false) grounds that ZGC's
            // signal was computed rather than latched and so had nothing to
            // clear. `ZgcRealHeap` owns the latch now, so this is G1's plain
            // delegation. Idempotent and cheap on purpose — the consumer clears
            // unconditionally after acting, including when its own gates said
            // no. Note this lowers only the LATCH; the `|| h.needs_gc()` half of
            // [`Self::young_spill_pressure`] is the heap's own occupancy and
            // clears itself when a collection actually runs.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.clear_native_alloc_pressure(),
        }
    }

    /// Record a native-wrapper allocation-pressure event — see
    /// [`Self::young_spill_pressure`].
    #[inline]
    pub fn note_young_spill_pressure(&self) {
        match self {
            VmHeap::Generational(h) => h.note_young_spill_pressure(),
            VmHeap::G1(h) => h.note_native_alloc_pressure(),
            // 2026-08-07: was a no-op under a `TODO(zgc)` specifying the latch
            // `ZgcRealHeap` should grow. It grew it (field + arming edge in
            // `alloc_raw` + disarm in `collect_garbage` + the accessor trio), so
            // the TODO is discharged and this is G1's plain delegation.
            //
            // What the no-op cost: `Self::young_spill_pressure` read the heap's
            // own occupancy, which covers the case that actually aborts the
            // process (the heap really is over the trigger) but NOT a caller
            // that spilled BELOW the trigger and wants the next native boundary
            // to collect anyway. That was a gap in the mechanism rather than a
            // live defect — this method still has no call site outside `gc/` —
            // but a silently-dropped signal is a bad thing to leave armed for
            // the first caller that does appear.
            //
            // This edge deliberately bypasses the `gc_threshold` / `gc_rearm`
            // predicate the heap applies to itself: the caller is asserting
            // pressure the heap's counters cannot see. It cannot storm, because
            // the consumer re-checks `gc_overhead_limit_exceeded` and
            // `needs_gc` before collecting and clears the latch either way, so
            // one note buys one gate evaluation.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_native_alloc_pressure(),
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

    /// `(used, free-list bytes, largest free block, capacity)` for the young
    /// from-space, for diagnostics only.
    ///
    /// SB-LOADER-ZIPCONTENT (2026-08-04). `live_bytes_estimate` reports
    /// `young.used - young.free_list` summed with old, which cannot distinguish
    /// "young is genuinely full of live objects" from "young was bumped to the
    /// top once and is now a free list nobody can carve an 8 KB array out of".
    /// Those two want opposite fixes, and the second is what a run of
    /// non-moving young sweeps produces — so the number that tells them apart
    /// belongs next to the overhead-limit numbers that motivated the question.
    ///
    /// Non-generational backends have no young from-space; they report zeros.
    pub fn young_occupancy(&self) -> (usize, usize, usize, usize) {
        match self {
            VmHeap::Generational(h) => h.young_from_occupancy(),
            _ => (0, 0, 0, 0),
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
        #[cfg(feature = "gpu-offload")]
        {
            let cycle = gpu_coordination::before_collection();
            if cycle.extra_roots.is_empty() {
                let result = self.collect_garbage_dispatch(stw, roots, monitors);
                cycle.after_collection(&[]);
                return result;
            }
            // Splice the GPU keep-alive roots onto the caller's, run the
            // collector over both, then hand each half back to its owner
            // with the post-collection addresses.
            let n = roots.len();
            let mut all: Vec<ObjectRef> = Vec::with_capacity(n + cycle.extra_roots.len());
            all.extend_from_slice(roots);
            all.extend_from_slice(&cycle.extra_roots);
            let result = self.collect_garbage_dispatch(stw, &mut all, monitors);
            roots.copy_from_slice(&all[..n]);
            cycle.after_collection(&all[n..]);
            result
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            self.collect_garbage_dispatch(stw, roots, monitors)
        }
    }

    fn collect_garbage_dispatch(
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
        // Retire every published FFM fast-path verdict. Those verdicts are keyed
        // by the carrier's ADDRESS, and after a collection an address no longer
        // identifies the object it identified before: a reclaimed carrier's
        // address can be handed to a different object, and a stale verdict would
        // vouch for it. One relaxed increment per CYCLE, never per access — see
        // `cratonvm_types::ffm_epoch`. Bumped here, at the one dispatcher every
        // collector goes through, so a future collector cannot silently miss it.
        cratonvm_types::ffm_epoch::bump_ffm_epoch();
        #[cfg(feature = "gpu-offload")]
        {
            // Same splice as `collect_garbage`; see there.
            let cycle = gpu_coordination::before_collection();
            if cycle.extra_roots.is_empty() {
                let result =
                    self.collect_with_finalizers_dispatch(stw, roots, finalizer_addrs, monitors);
                cycle.after_collection(&[]);
                return result;
            }
            let n = roots.len();
            let mut all: Vec<ObjectRef> = Vec::with_capacity(n + cycle.extra_roots.len());
            all.extend_from_slice(roots);
            all.extend_from_slice(&cycle.extra_roots);
            let result =
                self.collect_with_finalizers_dispatch(stw, &mut all, finalizer_addrs, monitors);
            roots.copy_from_slice(&all[..n]);
            cycle.after_collection(&all[n..]);
            result
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            self.collect_with_finalizers_dispatch(stw, roots, finalizer_addrs, monitors)
        }
    }

    fn collect_with_finalizers_dispatch(
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

    /// Bytes currently COMMITTED for the Java heap — backing storage the VM
    /// holds, whether or not anything lives in it. `Runtime.totalMemory()` and
    /// the JMX heap `MemoryUsage.getCommitted()`; `freeMemory()` is this minus
    /// [`Self::allocated_bytes`].
    ///
    /// **It must not track live bytes.** HotSpot's `totalMemory()` moves only
    /// when the heap grows or shrinks, and callers rely on that: H2's
    /// `Utils.collectGarbage()` has historically been written as "gc until
    /// `totalMemory()` stops changing", so a value that moved on every
    /// collection would turn one `System.gc()` into a fixed run of full ones.
    /// Every arm below is therefore a CAPACITY, not an occupancy:
    ///
    /// * Generational — both young semi-spaces plus the old generation
    ///   (`committed_heap_bytes`). This is the one arm that can move at all,
    ///   and only when an arena actually grows.
    /// * G1 — the single arena every region is carved from, allocated once and
    ///   never reallocated.
    /// * ZGC — the arena envelope captured at construction.
    ///
    /// The two fixed arms are not a placeholder: those collectors really do
    /// commit their whole heap up front, so reporting it is the honest answer
    /// and matches what `maxMemory()` already reports for them.
    pub fn committed_bytes(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.committed_heap_bytes(),
            VmHeap::G1(h) => h.committed_bytes(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.committed_bytes(),
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
    /// (`moving-young-gen-drops-jit-held-oops-FIXED.md`),
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
    /// `refs-metaspace-unloading.md`.
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
            // Phase 3 (2026-08-13): this arm was `{}`, and that empty body was
            // the whole of ZGC's missing mutator ingress. Every reference store
            // in this VM already reaches here for G1's sake, so the barrier
            // `zgc_concurrent.rs` describes as unwired needed no new call site
            // — it needed this arm. Inert while no cycle is marking: the ZGC
            // side is one relaxed load and a return.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                if let Value::Object(Some(obj_ref)) = old_value {
                    h.satb_pre_barrier(obj_ref.as_ptr() as usize);
                }
            }
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

    /// Returns whether this heap is the Generational collector — the only
    /// backend with a MOVING YOUNG generation.
    ///
    /// Exists because "does moving-young apply here?" was being answered by
    /// `conservative_roots::moving_young_enabled()`, which ANDs a JIT-side gate
    /// with `flags().gc.moving_young` and consults the collector in neither. G1
    /// evacuates by region and ZGC never moves anything, so on both of them the
    /// precise-moving-young question — and the unmemoised full-stack probe that
    /// answers it — is inert work. See the two `moving_young_precise_only`
    /// sites.
    pub fn is_generational(&self) -> bool {
        matches!(self, VmHeap::Generational(_))
    }

    /// Does this backend keep a conservatively-PINNED object at its address for
    /// the rest of the collection?
    ///
    /// # What asks, and why the answer is a capability rather than a policy
    ///
    /// The cross-thread JIT coverage handshake
    /// (`conservative_roots::refresh_moving_young_coverage_for_collection`)
    /// lets a moving cycle proceed while a peer thread holds compiled frames
    /// nobody could prove rewritable, provided those frames' conservative roots
    /// were PINNED instead. That discharge is sound exactly when the collector
    /// about to run honours the pin, and it is a property of the collector, not
    /// of the frames.
    ///
    /// * **G1** — `true`. It evacuates by region and withholds the regions named
    ///   by `gc_quiescence::pinned_jit_roots_snapshot()` from the collection set.
    /// * **ZGC** — `true`. Same shape at page granularity; `relocate_stw`
    ///   consumes the same snapshot.
    /// * **Generational** — **`false`, and it cannot be otherwise.** The young
    ///   collector is Cheney copying: from-space is reclaimed WHOLESALE, so
    ///   every live object in it moves by construction and there is no
    ///   "withhold this one" to implement. Consistently, `gen_heap.rs` and
    ///   `gen_evac.rs` contain no reader of the pin snapshot at all — the
    ///   pinned addresses reach that backend only as ROOTS, which keeps them
    ///   ALIVE and says nothing about keeping them PUT.
    ///
    /// That last distinction is the whole of this method. A peer's frame whose
    /// roots were "pinned" under the generational collector is a frame whose
    /// objects were faithfully kept alive at NEW addresses, with nothing having
    /// rewritten the frame — which is a stale compiled-frame reference, and was
    /// reproducible in ten seconds on the H2 JDBC corpus.
    pub fn honours_conservative_pins(&self) -> bool {
        match self {
            VmHeap::Generational(_) => false,
            VmHeap::G1(_) => true,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => true,
        }
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

    // =====================================================================
    // ZGC concurrent marking (2026-08-16)
    // =====================================================================
    //
    // Deliberately NOT folded into the `g1_*` predicates above. The two
    // collectors reach the same shape (open at a brief STW, trace with
    // mutators running, close at the next collection's STW) from opposite
    // sides -- G1 opens on an old-gen occupancy that only a young collection
    // updates, ZGC on total allocation, and G1 closes on a quiescence poll
    // while ZGC closes when the collection itself arrives. A shared predicate
    // would have to be a union of both, and the arm that did not apply would
    // be dead code that reads as coverage.

    /// Should a ZGC concurrent mark cycle open now?
    ///
    /// On the allocation path (`maybe_gc`), so the ZGC arm is two relaxed
    /// loads and the others are a compile-time-known `false`.
    #[inline]
    pub fn zgc_should_start_concurrent_mark(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.should_start_concurrent_mark(),
        }
    }

    /// Is a ZGC concurrent mark cycle in flight?
    #[inline]
    pub fn zgc_concurrent_mark_active(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.concurrent_mark_active(),
        }
    }

    /// Open a ZGC concurrent mark cycle at a brief stop-the-world pause.
    ///
    /// `roots` must be the COMPLETE root set -- this thread, every parked
    /// peer's snapshot, and the conservative roots of any forcibly-stopped
    /// in-JIT peer. A root missed here is an object the concurrent phase never
    /// traces, and the mark-end re-scan only covers roots that still exist
    /// then. Returns `true` iff a cycle opened.
    pub fn zgc_start_concurrent_mark(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[ObjectRef],
    ) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                let addrs: Vec<u64> = roots.iter().map(|r| r.as_ptr() as u64).collect();
                h.start_concurrent_mark(stw, &addrs)
            }
        }
    }

    /// Abandon an open ZGC concurrent cycle, discarding its mark bits.
    pub fn zgc_abandon_concurrent_mark(&self) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.abandon_concurrent_mark(),
        }
    }

    /// `(started, completed, black_allocations, ingress_replayed, phase_nanos)`
    /// for the ZGC concurrent marker; all zeros on the other backends.
    pub fn zgc_concurrent_mark_stats(&self) -> (usize, usize, usize, usize, u64) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => (0, 0, 0, 0, 0),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.concurrent_mark_stats(),
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
    pub fn g1_start_concurrent_mark(&self, stw: &crate::collector::StopTheWorldToken) {
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
            state.collector.start_concurrent_mark(stw);
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
    ///
    /// I-17: `remark` is an STW phase, so this wrapper takes the witness too
    /// rather than fabricating one — the caller is inside the initial-mark
    /// pause and already holds it.
    pub fn g1_mark_roots(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[cratonvm_types::ObjectRef],
    ) {
        if let VmHeap::G1(g1) = self {
            g1.remark(stw, roots); // remark marks roots + drains SATB
                                   // The worker (spawned by `g1_start_concurrent_mark` just before
                                   // this) may have already drained the initially-empty worklist and
                                   // parked with `quiesced=true`. These roots seed real work, so wake
                                   // it and clear the premature quiescence — otherwise the completion
                                   // poll could fire before the seeded graph is marked.
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
        stw: &crate::collector::StopTheWorldToken,
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
            state.collector.remark(stw, roots);
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
            state.collector.cleanup(stw);
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
    /// **This path therefore reclaims NOTHING, by design** (audit G1-3 /
    /// §8.2). Skipping the remark means the gray set is not drained to a fixed
    /// point, and `G1Collector::cleanup` no longer trusts its caller about
    /// that: it re-checks the worklist and, finding it non-empty, takes the
    /// retain-everything fail-safe — no in-place free of a zero-live Old
    /// region, no humongous reclaim — because on an incomplete closure
    /// `live_bytes == 0` does not mean "unreachable", and acting on it frees
    /// live objects. The pause is recorded with
    /// `g1_degraded::CLEANUP_CLOSURE_INCOMPLETE` so the declined reclamation
    /// is visible in `collector_decision_report()` rather than silent.
    ///
    /// So the useful reading of this method is "stop the marker and return the
    /// phase machine to `Idle`", not "finish the cycle". A caller that wants
    /// the cycle's reclamation must run [`Self::g1_final_remark_and_cleanup`],
    /// which drives `concurrent_mark_step` to a fixed point first. Covered by
    /// `g1::tests::cleanup_with_an_undrained_gray_set_retains_every_region`.
    ///
    /// Task #56: drains the [`ConcurrentMarkController`] slot and joins
    /// the background worker (blocking). The STW remark the caller runs
    /// next requires a quiescent worklist, so the join is mandatory.
    ///
    /// Edge case (no active controller): defensive no-op — happens when
    /// called twice, or before any cycle started. Skipping cleanup keeps
    /// the phase machine clean (cleanup itself is idempotent, but
    /// running it from Idle would flip `marking_complete` spuriously).
    pub fn g1_signal_marking_complete(&self, stw: &crate::collector::StopTheWorldToken) {
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
                    state.collector.cleanup(stw);
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
            // 2026-08-07: this arm used to answer with an honest "unavailable"
            // `tracing::info!` instead of enabling anything, because
            // `ZgcRealHeap` had no toggle to flip and no gated statement to flip
            // it for — so a user running `--verbose:gc -XX:+UseZGC` got nothing
            // for the whole run. `zgc.rs` has since grown the `gc_log_enabled:
            // AtomicBool` field, the `enable_gc_logging` / `disable_gc_logging`
            // pair mirroring G1's, and the per-collection `eprintln!` in
            // `collect_garbage` that reads the flag — so the honest answer is
            // now a plain delegation, exactly like G1's arm above.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.enable_gc_logging(),
        }
    }

    /// Print the aggregate per-collection pause summary (p50/p99/max young +
    /// mixed) to stderr. No-op for the generational collector (which keeps no
    /// pause history) and when no G1 collection has run. Called at VM shutdown
    /// when GC stats are requested (`--verbose:gc` or `CRATONVM_GC_STATS`).
    pub fn print_gc_summary(&self) {
        if let VmHeap::G1(g1) = self {
            g1.print_gc_summary();
            // Independent of GC stats being requested: this census answers
            // "how many accessor calls still take the global regions lock?",
            // which is a question about the MUTATOR, not about collections.
            // Gated by its own flag (`CRATONVM_DBG_G1ACCESSOR`).
            g1.dbg_report_accessor_census();
        }
        // ZGC: the same unconditional-counts treatment the generational branch
        // below gets, and for the same reason — without it a `--verbose:gc` run
        // on this backend printed NOTHING at all (there is no ZGC branch in any
        // logging path; `enable_gc_logging` above only ever emitted a claim),
        // so "did this configuration collect more?" — the first question to ask
        // about the open ZGC HANG/FAIL classes — could not be answered from a
        // log. `occupancy` is the post-sweep live figure, because the sweep
        // stores retained bytes back into `allocated` (`zgc.rs:2472`); it is
        // therefore directly comparable across runs, unlike a bump cursor.
        // Cheap: two relaxed loads and one arena lock at shutdown.
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            eprintln!(
                "[GC] zgc-real: collections={} occupancy={}/{} bytes",
                h.gc_count(),
                h.allocated_bytes(),
                h.heap_capacity(),
            );
            // WHY those collections happened. `needs_gc` has four
            // independent reasons and the count alone cannot separate
            // them, which is what made "13 collections against 2" on the
            // same workload unattributable. A cycle can satisfy more than
            // one, so these do not have to sum to `collections`.
            {
                {
                    // WHICH door started each cycle. Printed beside the
                    // trigger tallies because the two answer different
                    // halves of the same question, and on the run that
                    // motivated both, the tallies were all zero.
                    let (needs, requested, forced, native) =
                        cratonvm_types::gc_entry_census::totals();
                    eprintln!(
                        "[GC] zgc-entry: maybe_gc_needs={needs} \
                         maybe_gc_requested={requested} forced={forced} \
                         from_native={native}"
                    );
                    {
                        let (att, ok, total) = cratonvm_types::gc_entry_census::refill_totals();
                        eprintln!(
                            "[GC] zgc-entry:   tlab refills attempted={att} refill_succeeded={ok};                              bytes_allocated_total={total} (the wedge break's re-arm,                              one break per 64 MB)"
                        );
                        let (rt, rok) = cratonvm_types::gc_entry_census::refill_retry_totals();
                        eprintln!(
                            "[GC] zgc-entry:   post-break refill retries={rt}                              retry_succeeded={rok} (a success seeds the TLAB, whose                              allocations re-arm the breaker)"
                        );
                    }
                    for (site, n) in cratonvm_types::gc_entry_census::forced_sites() {
                        eprintln!("[GC] zgc-entry:   forced by {site}: {n}");
                    }
                }
                let (stress, threshold, headroom, budget, hard) = h.trigger_tallies();
                eprintln!(
                    "[GC] zgc-trigger: stress={stress} live_bytes_threshold={threshold}                      headroom_low={headroom} alloc_budget={budget}                      hard_alloc_refusals={hard}"
                );
            }
            // Phase 2.2's tracked number, on its own line so a suite runner can
            // extract it per class with one grep.
            //
            // `frag_samples=0` is printed rather than suppressed, and it does
            // NOT mean "no fragmentation": it means no collection ever left a
            // quarter of the heap free, so this run says nothing about the
            // subject. Reporting that as a clean score is exactly how a gauge
            // becomes a vacuous green, so the two cases are spelled
            // differently and the reader is told which they have.
            // Did the 2026-08-13 default-on features engage? A gauntlet run
            // that silently took the serial, non-moving path would otherwise
            // look identical to one that exercised both.
            let (par_cycles, compactions, relocated) = h.feature_engagement();
            // `driver_passes` is the one field that separates "the worker pool
            // marked" from "`zgc_concurrent`'s controller drove the cycle":
            // the pool-only path this replaced produced identical mark bits,
            // identical stats and an identical `parallel_mark_cycles`.
            let (driver_passes, mark_fallbacks) = h.driver_engagement();
            // `relocation_skipped_jit` belongs NEXT TO `compaction_cycles`, not
            // on a line of its own: alone, `compaction_cycles=0` reads as a
            // broken collector; beside a large skip count it reads as a
            // workload that is never JIT-quiet. ZGC declines to relocate while
            // a compiled frame is live (its registers and spill slots cannot be
            // rewritten), so a JIT-saturated run legitimately compacts rarely
            // -- and compaction is this collector's only defragmentation, so
            // that is a number somebody needs to see.
            let skipped_jit = h.relocation_skipped_jit();
            // ...and its counterpart: cycles that compacted WITH a compiled
            // frame live, on the collection's per-cycle coverage proof.
            // `skipped_jit` alone cannot tell "this workload is never
            // JIT-quiet, so it never defragments" from "it is never
            // JIT-quiet and defragments anyway, on the proof", and those
            // are the before and after of the H2
            // `TestKillProcessWhileWriting` OutOfMemoryError.
            let proven_jit = h.relocation_on_proven_jit();
            // A retained TLAB chunk would be arena the collector cannot see,
            // and on a compacting heap that is a correctness problem rather
            // than a bookkeeping one. It reads zero on every workload measured
            // so far, which is the point of printing it: the hypothesis it
            // rules out is a good one, and the next reader should not have to
            // re-derive it.
            let tlab_skipped = h.tlab_retire_skipped();
            // `targeted_pages=0` on its own is two different facts: no
            // allocation failure ever named a window, or every named window
            // went unconsumed because no cycle relocated. On
            // `DefaultCatalogAndSchemaTest` (2026-08-30) it was the second --
            // one window recorded, zero consumed, across four
            // `OutOfMemoryError`s -- and the two want opposite repairs.
            let (targets_recorded, targets_consumed) = h.compaction_target_engagement();
            // The starved TLAB rung takes the arena's LARGEST low free block, so
            // every firing lowers the very number a direct allocation is about
            // to fail against. Its switch shipped with no engagement counter,
            // which made "never reached" and "reached constantly" the same run.
            let (recycled_refills, starved_refills, starved_bytes) = h.tlab_recycle_engagement();
            // The VM thread's own TLAB (the JIT inline allocator's buffer) on
            // this backend. `vm_tlab_refills=0` on a run that allocated means
            // the arm never engaged -- switch off, or every allocation took
            // the helper -- and no throughput claim about it can stand.
            let (vm_tlab_refills, vm_tlab_refill_bytes, vm_tlab_tails, vm_tlab_tail_bytes) =
                h.vm_tlab_engagement();
            eprintln!(
                "[GC] zgc-features: parallel_mark_cycles={par_cycles} \
                 driver_passes={driver_passes} mark_fallbacks={mark_fallbacks} \
                 compaction_cycles={compactions} objects_relocated={relocated} \
                 relocation_skipped_jit={skipped_jit} \
                 relocation_on_proven_jit={proven_jit} \
                 tlab_retire_skipped={tlab_skipped} tlab_retire_skipped_at_safepoint={tlab_retire_skipped_at_safepoint}                  targeted_pages={targeted_pages}                  targets_recorded={targets_recorded} targets_consumed={targets_consumed}                  tlab_recycled_refills={recycled_refills} tlab_starved_refills={starved_refills}                  tlab_starved_bytes={starved_bytes} \
                 vm_tlab_refills={vm_tlab_refills} vm_tlab_refill_bytes={vm_tlab_refill_bytes} \
                 vm_tlab_tails_returned={vm_tlab_tails} vm_tlab_tail_bytes_returned={vm_tlab_tail_bytes}",
                targeted_pages = crate::zgc::forwarding::targeted_pages_selected(),
                tlab_retire_skipped_at_safepoint = h.tlab_retire_skipped_at_safepoint(),
                targets_recorded = targets_recorded,
                targets_consumed = targets_consumed,
                recycled_refills = recycled_refills,
                starved_refills = starved_refills,
                starved_bytes = starved_bytes,
            );
            // WHICH of the five terms refused, and — when it was the coverage
            // proof — which obligation. `relocation_skipped_jit` is a count of
            // a conjunction; on its own it names nothing, and the generational
            // collector's `moving_young_fallback_reason` census cannot fill the
            // gap because its only writer is that collector's own per-cycle
            // accounting (`gc_quiescence::moving_young_incomplete_reason_mask`
            // says so). Printed only when something actually declined, so a
            // clean run does not grow two empty sections.
            let skip_reasons = h.relocation_skip_reason_counts();
            for (reason, n) in skip_reasons.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] zgc-relocation-skip-reason: {}={}",
                        crate::zgc::relocation_skip_reason::label(reason),
                        n
                    );
                }
            }
            let cov_reasons = h.relocation_coverage_reason_counts();
            for (reason, n) in cov_reasons.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] zgc-relocation-coverage-reason: {}={}",
                        crate::gc_quiescence::incomplete_reason::label(reason),
                        n
                    );
                }
            }
            // WHAT THE UNREGISTERED-FRAME PROBE ACTUALLY SAW, because the
            // coverage reason above cannot say. `unregistered-jit-frame-on-stack`
            // counts cycles refused; these two count the HITS behind them, split
            // by the only question that decides whether a refusal was earned: was
            // the stack word at or above this thread's returned-JIT-frame residue
            // mark (a band no returned frame can have written -- a live guardless
            // frame) or below it (the leftovers of a frame that has returned)?
            // Printed only when the probe fired at all.
            let (residue_explained, residue_live) =
                crate::gc_quiescence::unregistered_jit_frame_residue_census();
            if residue_explained > 0 || residue_live > 0 {
                eprintln!(
                    "[GC] zgc-unregistered-jit-frame: hits_above_residue_mark={residue_live}                      hits_explained_by_residue={residue_explained}                      (the second kind marks and pins the band but no longer refuses                      relocation; CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0 restores the refusal)"
                );
            }
            // THE OTHER END OF THE ARENA, on its own line.
            //
            // Every number above describes the LOW end. A heap can compact that
            // one on every cycle while the allocation actually failing is
            // served from the large-object end, which until 2026-08-29 nothing
            // could relocate at all -- the H2
            // `TestMVStoreTool`/`TestCachedQueryResults` failures, and the
            // reason `targeted_pages` reads 0 on them. `declined` is printed
            // beside `cycles` for the reason `relocation_skipped_jit` is
            // printed beside `compaction_cycles`: `cycles=0` alone reads as a
            // broken compactor, `cycles=0 declined=812` reads as a workload
            // that never fragmented its large-object end, and only one of those
            // is a defect.
            let (hi_cycles, hi_declined, hi_moved, hi_bytes) = h.high_compaction_engagement();
            // ...and what the LOW slide handed back rather than losing. Reclaim
            // used to be the cursor drop alone, so every byte a slide emptied
            // below a cursor it could not move was invisible to the allocator
            // for the rest of the process -- and to the sweep too, which walks
            // the object-start registry the slide has just rewritten. A large
            // `vacated_bytes` is the measure of what that cost.
            let (vac_spans, vac_bytes) = h.vacated_publication();
            eprintln!(
                // KEYS PREFIXED `high_`, and that is not cosmetic. This line
                // used to print `cycles=` and `objects_relocated=`, which are
                // the SAME KEYS the whole-heap compaction line above emits for
                // an unrelated population -- the large-object end, whose counts
                // are tiny beside it (12 against 601233 on a measured H2 run).
                // A reader grepping the summary for `objects_relocated=` gets
                // two matches with no way to tell which is which, and the
                // obvious `| tail -1` picks THIS one. That is not a
                // hypothetical: it produced a wrong figure in an H2 analysis on
                // 2026-09-05, and the sibling collision on `cycles=` made a GC
                // control read a vacuous zero in the same session.
                //
                // A summary is an interface. Its keys have to be unique across
                // the whole summary or it is not greppable, which is the only
                // way anyone consumes it.
                "[GC] zgc-high-compaction: high_cycles={hi_cycles} high_declined={hi_declined}                  high_objects_relocated={hi_moved} high_bytes_copied={hi_bytes}                  high_vacated_spans={vac_spans} high_vacated_bytes={vac_bytes}"
            );
            // THE SLIDE VERIFIER'S OWN ENGAGEMENT, so that a clean run under
            // `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` is a READING rather than an
            // absence of output.
            //
            // `verify_no_dangling_slots_after_slide` reports a finding at
            // `error!` and a pass at `debug!`, and `release_max_level_info`
            // deletes the `debug!` from a release build. So on the binary
            // anybody actually reproduces with, "it printed nothing" covered
            // both "every reference slot resolved to a live base" and "the flag
            // was misspelled / no slide ran / the gate returned early" — and
            // only the first is evidence. `slides_verified` and
            // `survivors_walked` are the denominator that separates them.
            let (sv_runs, sv_survivors, sv_missed, sv_unreg) = h.slide_verification_stats();
            eprintln!(
                "[GC] zgc-slide-verify: slides_verified={sv_runs} survivors_walked={sv_survivors}                  missed_rewrites={sv_missed} unregistered_targets={sv_unreg} slide_verify_enabled={}",
                crate::zgc::zgc_verify_slide_enabled(),
            );
            // CONCURRENT marking, on its own line and with five fields rather
            // than one, because four different runs look identical in any
            // smaller summary:
            //
            //   started=0                 the threshold was never crossed --
            //                             this run says NOTHING about
            //                             concurrent marking
            //   started>0, completed=0    every cycle opened and then failed to
            //                             certify; the collector fell back to a
            //                             stop-the-world mark each time
            //   started>0, replayed=0     the barrier saw no reference
            //                             overwrites, so the SATB half is
            //                             untested by this workload
            //   started>0, phase_ms~0     the cycle opened and the collection
            //                             arrived immediately, so there was no
            //                             concurrent phase to speak of
            //
            // `black` is the allocate-black count: objects born marked because
            // a cycle was in flight. Zero of those with a non-zero `phase_ms`
            // means the mutators allocated nothing while the marker ran.
            let (started, completed, black, replayed, phase_nanos) =
                self.zgc_concurrent_mark_stats();
            eprintln!(
                "[GC] zgc-concurrent: cycles_started={started} cycles_completed={completed} \
                 black_allocations={black} satb_replayed={replayed} \
                 concurrent_phase_ms={}",
                phase_nanos / 1_000_000
            );
            // PHASE G. `old_retained` is the one that says whether the phase
            // did anything: it counts the objects a young cycle kept WITHOUT
            // tracing, i.e. the tracing it did not do. A run with
            // `young_cycles>0` and `old_retained=0` did full-heap work under a
            // generational name -- which is precisely the vacuous green a
            // "generational is on" claim would otherwise be built on. Printed
            // unconditionally, so a run that never engaged the phase says so
            // instead of printing nothing.
            let (young, since_major, retained, remembered, promoted, recards) =
                h.generational_stats();
            eprintln!(
                "[GC] zgc-generational: enabled={} young_cycles={young} \
                 minors_since_major={since_major} old_retained={retained} \
                 remembered_roots={remembered} promotions={promoted} \
                 recards_after_relocation={recards}",
                h.generational_enabled(),
            );
            // THE NURSERY, beside the split it bounds. `sweep_skipped` is the
            // engagement counter: a run with `young_cycles>0` and
            // `sweep_skipped=0` swept the whole registry on every young cycle, so
            // the floor never moved and the O(young) sweep is not happening.
            let (skipped, floor, old_live) = h.nursery_stats();
            let (nursery_fired, nursery_budget) = h.nursery_trigger_stats();
            eprintln!(
                "[GC] zgc-nursery-trigger: fired={nursery_fired}                  budget_bytes={nursery_budget} promotions_by_slide={} \
                 overshoot_max={}",
                h.promotions_by_slide(),
                // BESIDE THE BUDGET, because it is meaningless without it: this
                // is how far past `budget_bytes` the nursery got before a
                // safepoint arrived, and it prices "a hard ceiling rather than a
                // trigger" -- an open item that has had no number attached. See
                // `ZgcRealHeap::gen_nursery_overshoot_max`.
                h.nursery_overshoot_max(),
            );
            eprintln!(
                "[GC] zgc-nursery: sweep_skipped={skipped} floor={floor}                  old_live_bytes={old_live}",
            );
            // WHAT THE YOUNG SWEEP STOPPED DOING PER DEAD OBJECT.
            //
            // Read the two together and against `young_cycles` above.
            // `zero_bytes_skipped=0` with `young_cycles>0` means every dead
            // object was still memset in full, so `CRATONVM_ZGC_GEN_HEADER_ZERO`
            // is on and inert; `dead_runs == dead_objects` means no two dead
            // objects were ever adjacent, so the run merge is. Neither number
            // says it alone -- a small `dead_runs` is equally consistent with a
            // cycle that found almost no garbage.
            let (zero_skipped, dead_runs, dead_objects) = h.gen_sweep_cost_stats();
            eprintln!(
                "[GC] zgc-sweep-cost: zero_bytes_skipped={zero_skipped} \
                 dead_runs={dead_runs} dead_objects={dead_objects}",
            );
            // `ZGC_UNSIZABLE_OBJECTS` had no reader anywhere but a unit test.
            // It is the sweep's own count of registered objects whose header it
            // could not size -- i.e. of heap corruption the collector has
            // already met and silently worked around, one warning per process.
            // A run that ends with a nonzero here has corrupt headers whatever
            // else it reports.
            // A NOTIFICATION THAT IS MISSING IS A SLOWDOWN, NOT A FAILURE.
            // The mark driver's fixed-point wait is a `wait_for`, so a lost
            // notification costs a poll interval and is otherwise
            // indistinguishable from a working one -- which is how the driver
            // came to poll a never-notified condvar on a 5 ms grid for months.
            // Nonzero here means it is back. A count, so it reads the same on a
            // loaded host as on a quiet one.
            // THE OVERLAY GATE, asked of the provider rather than measured here
            // -- see `ExternalRootProvider::gate_stats`. `roots_for_owner` runs
            // once per marked object and was 20-29% of mark samples on both the
            // serial and the parallel arm (2026-08-17 `perf record`). A gate that
            // works and a gate that is inert return the same empty `Vec`, and the
            // previous attempt at this optimisation WAS inert, so `hits` is the
            // only thing that separates them. `disabled=true` means an owner was
            // registered with no class id and the gate has failed safe.
            for (name, hits, misses, disabled) in crate::external_roots::provider_gate_stats() {
                eprintln!(
                    "[GC] zgc-overlay-gate: provider={name} hits={hits}                      misses={misses} disabled={disabled}",
                );
            }
            eprintln!(
                "[GC] zgc-mark-wait: park_timeouts={}",
                h.mark_park_timeouts(),
            );
            eprintln!(
                "[GC] zgc-integrity: unsizable_registered_objects={}",
                crate::zgc::ZGC_UNSIZABLE_OBJECTS.load(std::sync::atomic::Ordering::Relaxed),
            );
            let g = h.frag_gauge();
            match g.worst_permille {
                Some(worst) => eprintln!(
                    "[GC] zgc-frag: frag_samples={} worst_largest_free_permille={}                      free_permille_at_worst={} at_cycle={}",
                    g.samples, worst, g.free_permille, g.worst_cycle,
                ),
                None => eprintln!(
                    "[GC] zgc-frag: frag_samples=0 worst_largest_free_permille=n/a                      (no collection left >=25% of the heap free; this run is not                      evidence either way)"
                ),
            }
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
        // Young non-moving-sweep health, UNCONDITIONALLY (the H2-CID0 rule: a
        // line printed only when non-zero cannot tell "clean" from "never
        // ran", and here that is the whole question).
        //
        // `par_accepts` far below `par_attempts` means the parallel sweep
        // prefix is being discarded and the entire arena is re-swept
        // sequentially. Until 2026-08-12 that was the state on EVERY JIT-warm
        // workload — `attempts=5 accepts=0` on the hibernate repro,
        // every abort the benign empty-object zero run — and these counters
        // said so the whole time with nobody to read them.
        //
        // `zero_empty_runs` is that benign shape, now stepped over on-grid: it
        // is normal and often large, and is deliberately NOT summed with
        // `zero_spans`, the residue that still forces an unwind.
        // `phantom_extents` and `live_in_dead` are the two corruption guards —
        // non-zero on either is a finding, not tuning.
        if let VmHeap::Generational(_) = self {
            use std::sync::atomic::Ordering as O;
            eprintln!(
                "[GC] young_sweep: par_attempts={} par_accepts={} zero_spans={} \
                 zero_empty_runs={} phantom_extents={} phantom_nonbase_marks={} \
                 raw_interior_cleared={} live_in_dead={} walk_overshoot={} \
                 anchor_not_a_base={}",
                crate::gen_heap::PAR_SWEEP_ATTEMPTS.load(O::Relaxed),
                crate::gen_heap::PAR_SWEEP_ACCEPTS.load(O::Relaxed),
                crate::gen_heap::SWEEP_ZERO_SPAN_HITS.load(O::Relaxed),
                crate::gen_heap::SWEEP_ZERO_SPAN_EMPTY_RUNS.load(O::Relaxed),
                crate::gen_heap::SWEEP_PHANTOM_EXTENTS.load(O::Relaxed),
                crate::gen_heap::SWEEP_PHANTOM_INTERIOR_MARKS.load(O::Relaxed),
                crate::gen_heap::LATE_RESOLVE_RAW_INTERIOR_CLEARED.load(O::Relaxed),
                crate::gen_heap::LIVE_IN_DEAD_SPANS.load(O::Relaxed),
                crate::gen_heap::SWEEP_WALK_OVERSHOOT_HITS.load(O::Relaxed),
                crate::gen_heap::SWEEP_ANCHOR_NOT_A_BASE.load(O::Relaxed),
            );
            eprintln!(
                "[GC] young_sweep_empty_runs: last_cycle_bytes={} young_used={} no_header_flag={}",
                crate::gen_heap::EMPTY_RUN_BYTES_LAST.load(O::Relaxed),
                crate::gen_heap::EMPTY_RUN_YOUNG_USED_LAST.load(O::Relaxed),
                crate::gen_heap::SWEEP_NO_HEADER_FLAG.load(O::Relaxed),
            );
            let l = &crate::gen_heap::LATE_WALK_ZERO_RUNS;
            eprintln!(
                "[GC] late_walk_zero_runs: zr_mark_y2o={} zr_fixup_yo={} zr_walk_young={}",
                l[0].load(O::Relaxed),
                l[1].load(O::Relaxed),
                l[2].load(O::Relaxed),
            );
            // Which check abandoned a chunk. `par_accepts` alone cannot say,
            // and one `None` from any chunk discards the whole cycle's
            // attempt. Legend is on `PAR_CHUNK_BAILS`; printed as a bare array
            // so a soak log can be diffed without parsing seven key=value
            // pairs, and unconditionally for the same reason as the line above.
            let b = &crate::gen_heap::PAR_CHUNK_BAILS;
            eprintln!(
                "[GC] young_sweep_chunk_bails: overshoot={} gap_filler={} zero_span={} \
                 bad_size={} hole_crossing={} phantom={} anchor_miss={}",
                b[0].load(O::Relaxed),
                b[1].load(O::Relaxed),
                b[2].load(O::Relaxed),
                b[3].load(O::Relaxed),
                b[4].load(O::Relaxed),
                b[5].load(O::Relaxed),
                b[6].load(O::Relaxed),
            );
            // …and of the zero-span bails, which of the predicate's three
            // conditions did the refusing. See `ZERO_RUN_REFUSALS`.
            let z = &crate::gen_heap::ZERO_RUN_REFUSALS;
            eprintln!(
                "[GC] young_sweep_zero_refusals: misaligned={} live_inside={} \
                 implausible_next={} live_resumes={}",
                z[0].load(O::Relaxed),
                z[1].load(O::Relaxed),
                z[2].load(O::Relaxed),
                // NOT a refusal: runs stepped over by resuming at a PROVED live
                // base inside them (`zero_run_verdict`). `live_inside` beside it
                // stays the genuine refusals — an UNRESOLVED mark, which may be
                // an object interior rather than a base, or a caller with no
                // unresolved set to judge against. Both nonzero is the expected
                // reading: the gate is meant to accept only what it can prove.
                crate::gen_heap::ZERO_RUN_LIVE_RESUMES.load(O::Relaxed),
            );
            // Did the five walks that still carry the old rule even RUN? A
            // zero anomaly count above means nothing without this. Legend on
            // `YOUNG_WALK_ENTRIES`; `sp_*` is the selective-promotion census,
            // which says whether the two passes inside it were reachable at
            // all (`sp_selective` is the gate).
            let w = &crate::gen_heap::YOUNG_WALK_ENTRIES;
            let (sw, sel, defrag, cand, pin, unaged, evac, ofull) =
                crate::gen_heap::selective_promotion_census();
            eprintln!(
                "[GC] young_walk_entries: evac_prepass={} fixup_3a={} mark_y2o={} \
                 fixup_yo={} walk_young={} | sp_sweeps={sw} sp_selective={sel} \
                 sp_defrag={defrag} sp_candidates={cand} sp_pinned={pin} \
                 sp_unaged={unaged} sp_evacuated={evac} sp_old_full={ofull}",
                w[0].load(O::Relaxed),
                w[1].load(O::Relaxed),
                w[2].load(O::Relaxed),
                w[3].load(O::Relaxed),
                w[4].load(O::Relaxed),
            );
            // …and when the evacuation pre-pass DID run, what stopped it.
            let e = &crate::gen_heap::EVAC_UNWIND_REASONS;
            eprintln!(
                "[GC] evac_unwind: unwind_overshoot={} unwind_zero_span={} unwind_bad_size={} \
                 unwind_hole_crossing={} candidates_dropped={}",
                e[0].load(O::Relaxed),
                e[1].load(O::Relaxed),
                e[2].load(O::Relaxed),
                e[3].load(O::Relaxed),
                crate::gen_heap::EVAC_UNWIND_CANDIDATES.load(O::Relaxed),
            );
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
        // H2-CID0 — the marking fail-open in `compact_oop_scan`. Printed only
        // when non-zero, because zero is the expected reading and a line that
        // is always there stops being read.
        {
            let n = crate::heap::COMPACT_OOP_MAP_MISSING.load(std::sync::atomic::Ordering::Relaxed);
            if n != 0 {
                eprintln!(
                    "[GC] compact_oop_map_missing={n} — MARKING FAIL-OPEN: that many \
                     GC_FLAG_COMPACT objects were scanned as legacy `Value` cells, so \
                     every reference they hold was invisible to the collector"
                );
            }
        }
        // The read-side companion to the line above: that one is a marking
        // FAIL-OPEN, this one is a read FAIL-SILENT. Every path that hands Java
        // a `null` for a slot that held bits — the interpreter's tagged decodes,
        // the JIT read helpers, `read_prim_element`'s reference arm — feeds one
        // process-wide per-source table in `cratonvm_types::compact_value`, and
        // this prints the breakdown rather than the total because the sources do
        // not mean the same thing:
        //
        //   * `interpreter` reads a TAGGED slot, so a count there can be a
        //     primitive `long` whose verbatim bits collided into the object
        //     sub-tag. That is benign and is exactly what the degrade exists for.
        //   * `jit` and `array-element` read UNTAGGED reference words — the JVM
        //     type system already says the slot is a reference — so there is no
        //     long/object ambiguity to absorb. A non-zero count there means a
        //     word that should have been a live pointer was handed to Java as
        //     `null`, i.e. the GC root-coverage gap of commit `6a04b0e3c1` is
        //     live in THIS run. `jit` is the most diagnostic of the three: a
        //     JIT'd frame is the frame a deposited root snapshot misses.
        //
        // Printed only when non-zero, for the same reason as the line above.
        //
        // The total is summed from THIS snapshot rather than read via
        // `object_degradation_count()`: that accessor is defined as the same sum
        // but takes its own set of relaxed loads, so under concurrency the two
        // could disagree and the printed line would not add up.
        {
            let breakdown = cratonvm_types::compact_value::object_degradation_breakdown();
            let total: u64 = breakdown.iter().sum();
            if total != 0 {
                let rendered = cratonvm_types::compact_value::DegradationSource::ALL
                    .iter()
                    .map(|s| format!("{}={}", s.name(), breakdown[s.index()]))
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!(
                    "[GC] object_degradations={total} ({rendered}) — READ FAIL-SILENT: \
                     that many reference-shaped slots decoded to `null` instead of the \
                     object they named; a non-zero `jit` or `array-element` count is a \
                     live root-coverage failure, not a long/object collision"
                );
            }
        }
        // What the collector actually did on the last cycle and why. This is
        // the line that settles the `docs/GC.md` ("young collections run
        // non-moving whenever any JIT frame is active") vs `ARCHITECTURE.md`
        // ("per-cycle coverage proof, moving is possible") disagreement for
        // THIS run — see `tlab-and-card-audit.md` §3.
        //
        // Under G1 this used to be uninformative by construction:
        // `G1Collector::collect_garbage` passed the constant
        // `incomplete_reason::NONE`, so the record said "the backend always
        // evacuates" and nothing else, whatever the root scan had found. It now
        // carries the obligation that actually failed, and
        // `collector_decision_report` appends the `[GC] g1 root coverage:` rate
        // — which is what makes "was this pause's root set complete?" a
        // question a log answers instead of a crash dump.
        //
        // NOT PRINTED HERE, and that is the fix rather than an omission.
        // `vm-cli`'s `maybe_dump_shutdown_reports` emits the decision report on
        // BOTH exit arms and this function is now reached from that same hook,
        // so printing it here as well put the whole report on stderr TWICE on
        // the normal-return arm. The report is the one census that must survive
        // a `System.exit`, so the hook keeps it and this function does not.
        // Card / remembered-set costs, raw and normalized per allocated object
        // and per live byte.
        eprintln!("{}", crate::gc_metrics::gc_metrics_report());
        // Parallel young evacuation. Printed UNCONDITIONALLY, including the
        // all-zero line: the path is gated four ways over (moving cycle,
        // `CRATONVM_GC_PAR_EVAC`, the worker policy, and `ParEvac::plan`'s
        // to-space slack), so "never engaged" is the common outcome and a
        // counter that only appears when non-zero would make it
        // indistinguishable from "the report is missing".
        //
        // `helper_scans` is the one to read second. `cycles > 0` only says the
        // copy phase dispatched; `helper_scans == 0` beside it says the driver
        // did all of it, which is a load-balancing regression every
        // correctness test in the suite passes (see `gen_evac`).
        {
            let c = crate::gen_evac::par_evac_census();
            eprintln!(
                "[GC] par_evac: par_evac_cycles={} helper_scans={} cas_losses={} \
                 declined_for_slack={} filler_bytes={} par_evac_promotions={} \
                 deferred_cards={}",
                c.cycles,
                c.helper_scans,
                c.cas_losses,
                c.declined_for_slack,
                c.filler_bytes,
                c.promotions,
                c.deferred_cards,
            );
        }
        let fallbacks = crate::gc_quiescence::moving_young_coverage_fallback_count();
        if crate::gc_quiescence::moving_young_enabled() || fallbacks > 0 {
            // Both numbers, always. A correct answer while `cycles == 0` means
            // the young generation never actually copied anything, which is the
            // exact way the 2026-07-01 validation declared moving-young working
            // while it was inert (see
            // `moving-young-corruption-rootcause.md`
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

    /// Carve out a TLAB from the young generation (generational), the Eden
    /// region (G1), or the low arena (ZGC, since 2026-09-02 -- `zgc/vm_tlab.rs`,
    /// kill switch `CRATONVM_ZGC_JIT_TLAB=0`). Returns `Some((ptr, size))` on
    /// success; the chunk is zeroed.
    ///
    /// Every object the VM lays out in the chunk must be reported through
    /// [`Self::note_tlab_object`] the moment its header is complete: on ZGC
    /// that is how the object enters the start registry the sweep, the SATB
    /// barrier and the conservative scans all consult.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        let chunk = match self {
            VmHeap::Generational(h) => h.refill_tlab(requested_size),
            VmHeap::G1(h) => h.refill_tlab(requested_size),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.refill_tlab(requested_size),
        };
        // `CRATONVM_DBG_VACATED_FRAMES`: the chunk is bump-allocated from
        // without any further call into the heap, so this is the only door that
        // can tell the vacated ledger those addresses are being re-issued.
        if let Some((ptr, size)) = chunk {
            let lo = ptr as usize;
            crate::gc_quiescence::note_allocated_range(lo, lo.saturating_add(size));
        }
        chunk
    }

    /// An object the VM just finished laying out at `ptr` inside a chunk from
    /// [`Self::refill_tlab`]; `footprint` is what the buffer's cursor advanced
    /// by. Registers it with the backend that needs to know (ZGC's start
    /// registry, plus allocate-black and the young grain); a no-op on the
    /// backends whose sweeps parse the chunk linearly.
    #[inline]
    pub fn note_tlab_object(&self, ptr: *mut u8, footprint: usize) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = (ptr, footprint);
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_tlab_object(ptr, footprint),
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
            // `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
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
            // ZGC: the EXACT registry-base test (`zgc.rs:1715` — one hash
            // probe), deliberately NOT the loose `is_heap_addr` this arm used
            // to call. The two differ only in `is_heap_addr`'s interior
            // fallback (`zgc.rs:1731-1743`), and that fallback is exactly what
            // made this arm unsound.
            //
            // The ZGC sweep zeroes each dead object, returns its span to the
            // arena free list, and then COALESCES adjacent free spans into
            // maximal blocks. A later allocation carved from the head of a
            // coalesced block therefore covers the interior of what used to be
            // several dead objects — so a *dead* object's pre-GC base becomes
            // an interior address of an innocent LIVE object, and the extent
            // walk answered `true` for it. Reference processing then read that
            // as "the referent survived", and because ZGC's `pointer_map`
            // was always empty when this arm was written — the collector was
            // non-moving until 2026-08-13, and `relocate_stw` now returns a
            // NON-EMPTY map on a default run — the consumer at
            // `interpreter/gc_and_alloc.rs:2287` falls back to the stale
            // address and does `set_field(obj, 0, Value::Object(None))` on it
            // — a null written into the middle of a live object, and at the
            // weak/phantom restore site a non-null reference written there.
            // That is precisely the HIB-CV-32 stale-referent-write corruption
            // shape the guard chain around `watched_pre_gc_addr_survived`
            // exists to prevent, reproduced on this backend through a
            // predicate whose own consumer doc (below, "registry lookup")
            // already believed it was exact. It is exact now.
            //
            // Not merely a correctness fix: `is_heap_addr`'s fallback is
            // O(live) *under the registry mutex*, and this predicate runs once
            // per tracked reference per collection. `is_object_address` is one
            // locked hash probe.
            //
            // Contrast G1's arm above, which IS deliberately loose
            // (region-granular): G1 emits identity `pointer_map` entries for
            // every self-forwarded live object, so the map hit fires first and
            // the loose predicate is only a fallback. ZGC has no such map, so
            // its predicate is load-bearing alone and must be exact.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_object_address(addr).is_some(),
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
    /// `bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`).
    ///
    /// Both old-gen paths now emit an identity `pointer_map` entry for every
    /// watched address that survived without moving, so once
    /// `gc_quiescence::old_gen_reclaimed_last_cycle()` is set, map membership
    /// is a complete and exact proof.
    pub fn watched_pre_gc_addr_survived(
        &self,
        addr: usize,
        pointer_map: &cratonvm_types::PointerMap,
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
    /// H2-CID0 — see [`GenerationalHeap::live_holders_of`]. Empty for every
    /// non-generational backend.
    /// ZGC's relocation ledger: what the slide moved AWAY from `addr`, as
    /// `(from, moved_to, class_id, size, still_live_at_target)`.
    ///
    /// `None` on every other backend, and `None` on ZGC unless
    /// `CRATONVM_DBG_ZGC_CORPSE` armed the run -- the ledger costs a map insert
    /// per relocated object and a cycle relocates hundreds of thousands, so it
    /// cannot be flag-free. It is asked anyway because it is the one thing that
    /// separates "the holder was never rewritten when its referent moved" from
    /// "the object died later": `still_live_at_target` answers exactly that.
    pub fn zgc_corpse_lookup(&self, addr: usize) -> Option<(usize, usize, u32, usize, bool)> {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.corpse_lookup(addr),
            _ => {
                let _ = addr;
                None
            }
        }
    }

    pub fn live_holders_of(&self, addr: usize, cap: usize) -> Vec<(usize, u32, usize)> {
        match self {
            VmHeap::Generational(h) => h.live_holders_of(addr, cap),
            // See `ZgcRealHeap::live_holders_of`: the slot ordinal it reports
            // is the object's own reference-slot ordinal, not a field index.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.live_holders_of(addr, cap),
            _ => Vec::new(),
        }
    }

    pub fn reclaimed_hole_at(&self, addr: usize) -> Option<(&'static str, usize, usize)> {
        match self {
            VmHeap::Generational(h) => h.reclaimed_hole_at(addr),
            VmHeap::G1(_) => None,
            // ZGC answers this now (2026-08-17). The arm said `None` on the
            // grounds that "their liveness is region/registry based and
            // `is_addr_live` already answers exactly, so there is no free-list
            // view to consult" -- true of G1, but ZGC's sweep zeroes each dead
            // object and returns its span to an arena free list, which is
            // precisely the view this predicate wants. While it answered
            // `None`, the DEFAULT collector reported nothing at all for a
            // reclaimed receiver.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.reclaimed_hole_at(addr),
        }
    }

    /// Generational: `[base, base + capacity)` of the INACTIVE young
    /// semispace — the arena a moving cycle has just evacuated and zeroed.
    ///
    /// This is [`reclaimed_hole_at`](Self::reclaimed_hole_at)'s
    /// inactive-semispace arm hoisted into a plain range, so a caller that
    /// must test many addresses at once (the blocked-thread root audit in
    /// `ThreadRegistry::fold_pointer_map_into_blocked`) pays one arena lock
    /// instead of three per address. No live object is ever in here: a live
    /// young object is in the active semispace or in old gen.
    ///
    /// G1/ZGC have no semispace pair, hence `None`.
    pub fn young_inactive_semispace_range(&self) -> Option<(usize, usize)> {
        match self {
            VmHeap::Generational(h) => Some(h.young_inactive_semispace_range()),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Publish the young semispace geometry for
    /// `gen_heap::dead_young_ref_reason_global`. No-op on the backends that
    /// have no semispace pair.
    pub fn publish_young_geometry(&self) {
        if let VmHeap::Generational(h) = self {
            h.publish_young_geometry();
        }
    }

    /// Generational: is `addr` a young reference naming no live object, and
    /// why? See `GenerationalHeap::dead_young_ref_reason`.
    ///
    /// `None` on every other backend — the predicate is defined in terms of a
    /// semispace pair, and G1/ZGC have none.
    pub fn dead_young_ref_reason(&self, addr: usize) -> Option<&'static str> {
        match self {
            VmHeap::Generational(h) => h.dead_young_ref_reason(addr),
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
    /// type. See `spb1-springframework-util-investigation-FIXED.md`'s
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
    /// - ZGC: dead means gone from the registry (`is_addr_live` false). The
    ///   moved case never reaches that arm: the `pointer_map` test at the top
    ///   of this function answers first. That is what keeps the arm correct
    ///   now that ZGC compacts by default (`CRATONVM_ZGC_RELOCATE`, on since
    ///   2026-08-13); the bullet gave "non-moving" as its reason until
    ///   2026-09-01, and the reason had expired even though the answer had
    ///   not. The R6 audit note further down this file reaches the same
    ///   finding by reading the two arms rather than the collector.
    pub fn pre_gc_addr_did_not_survive(
        &self,
        addr: usize,
        pointer_map: &cratonvm_types::PointerMap,
    ) -> bool {
        if pointer_map.contains_key(&addr) {
            return false;
        }
        match self {
            // "Young and unmapped" is NOT a death certificate. It proves death
            // only for a MOVING young collection, where every survivor gets a
            // `pointer_map` entry. The non-moving sweep keeps survivors in
            // place and produces NO map entries at all, so this arm condemned
            // every live young object the moment the moving collector fell
            // back — and it falls back on every collection in any workload
            // with a live JIT frame it cannot map
            // (`reason=innermost-rbp-belongs-to-unguarded-callee`).
            //
            // What that cost: `process_references_after_gc` skips the enqueue
            // when either the `Reference` or its `ReferenceQueue` "did not
            // survive", so NO reference was ever enqueued — a `WeakReference`
            // was cleared but never delivered, and no `Cleaner` action ever
            // ran. `EnqProbe` reports `gc enqueued it = false` where HotSpot
            // enqueues; H2 then grows without bound, and the UPDATE workload
            // that used to run in `--Xmx 1g` dies with `Out of memory` at 4g.
            //
            // `is_live_young_survivor` is the discriminator built for exactly
            // this (see its soundness argument: STW-window-only, zeroed-span
            // discriminator, moving-collection compatible), and it is already
            // what the strict sibling `watched_pre_gc_addr_survived` uses for
            // its young arm — these two must not disagree about the same
            // address. The conjunction keeps the original verdict everywhere
            // it was right: a genuinely dead young address, and the abandoned
            // old address of an object a moving collection relocated, both
            // still answer "did not survive", which is the `bc math-ec 0x4`
            // protection this predicate exists for.
            VmHeap::Generational(h) => {
                h.is_in_young_either(addr as *const u8) && !h.is_live_young_survivor(addr)
            }
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

    /// Hold a direct token on a worker thread for 200 ms; the drain on
    /// the main thread waits its budget, then GIVES UP and vetoes
    /// relocation for the cycle instead of spinning until the token
    /// drops.
    ///
    /// AUDIT 2026-09-02: this used to assert the drain blocked for at
    /// least 150 ms — that the wait was unbounded. Unbounded was the
    /// defect: a token nobody released was a collector that never ran
    /// again. The bound is `collector_wait_budget()` (50 ms unless
    /// `CRATONVM_GPU_CRITICAL_WAIT_MS` says otherwise).
    #[test]
    fn drain_gives_up_after_its_budget_and_vetoes_relocation() {
        let heap = std::sync::Arc::new(VmHeap::new(GcBackend::Generational, 1 << 20));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let heap2 = heap.clone();
        let holder = std::thread::spawn(move || {
            let _t = heap2.enter_gpu_critical();
            started_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        started_rx.recv().unwrap();
        let budget = cratonvm_cuda_bridge::critical::collector_wait_budget();
        let start = Instant::now();
        wait_for_gpu_critical_drain();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= budget,
            "drain returned in {elapsed:?}, before its {budget:?} budget"
        );
        assert!(
            elapsed < Duration::from_millis(190),
            "drain waited {elapsed:?} for a token held 200 ms: the wait is still unbounded"
        );
        assert!(
            gpu_relocation_forbidden(),
            "a wait that gave up must veto relocation for the cycle"
        );
        holder.join().unwrap();
        // What the collector does at the end of the cycle: clear the veto.
        let cycle = gpu_coordination::before_collection();
        cycle.after_collection(&[]);
        assert!(!gpu_relocation_forbidden());
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
mod gc_summary_key_tests {
    /// Every `key=` in the `[GC]` summary must be unique across the WHOLE
    /// summary, not merely within its own line.
    ///
    /// # Why this is a test and not a style note
    ///
    /// The summary is an interface, and the only way anyone consumes it is
    /// `grep`. A key that appears on two lines therefore has no answer: the
    /// reader gets two matches for unrelated quantities and the obvious
    /// `| tail -1` silently picks whichever comes last.
    ///
    /// That is not hypothetical. Both of these cost real analysis time on
    /// 2026-09-05:
    ///
    /// * `objects_relocated=` was printed by whole-heap compaction AND by
    ///   `zgc-high-compaction`, whose large-object counts are tiny beside it.
    ///   An H2 measurement read **12** where the run had relocated **601233**.
    /// * `cycles=` was printed by `par_evac` and `moving_young`. A GC control
    ///   in the same session matched `par_evac`'s — which is the GENERATIONAL
    ///   evacuator and reads zero under ZGC whatever ZGC did — and reported a
    ///   vacuous zero as if the heap had never collected.
    ///
    /// Ten keys collided when this test was written. The fix is a per-line
    /// prefix (`high_`, `unwind_`, `zr_`, `par_evac_`, `refill_`/`retry_`);
    /// the rule is that a new summary field may not reuse a name another line
    /// already owns.
    #[test]
    fn every_gc_summary_key_is_unique_across_the_whole_summary() {
        let src = include_str!("vm_heap.rs");

        // Establish the corpus before concluding anything from it: a file that
        // stopped carrying the summary would make this pass vacuously.
        let lines: Vec<&str> = src
            .match_indices("\"[GC]")
            .filter_map(|(i, _)| {
                let rest = &src[i + 1..];
                rest.find('"').map(|end| &rest[..end])
            })
            .collect();
        assert!(
            lines.len() > 20,
            "expected the [GC] summary to have many lines, found {} -- this scan              is reading the wrong text and its verdict means nothing",
            lines.len()
        );

        // `key={` is the format-string form; that is what a reader greps for.
        let keys_of = |line: &str| -> Vec<String> {
            let mut out = Vec::new();
            let b = line.as_bytes();
            for (i, _) in line.match_indices("={") {
                let mut s = i;
                while s > 0 {
                    let c = b[s - 1];
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        s -= 1;
                    } else {
                        break;
                    }
                }
                if s < i {
                    let k = &line[s..i];
                    if !k.chars().next().unwrap_or('0').is_ascii_digit()
                        && !out.contains(&k.to_string())
                    {
                        out.push(k.to_string());
                    }
                }
            }
            out
        };

        let mut owner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut collisions: Vec<String> = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            for k in keys_of(line) {
                match owner.get(&k) {
                    Some(&first) if first != idx => collisions.push(k),
                    _ => {
                        owner.entry(k).or_insert(idx);
                    }
                }
            }
        }
        collisions.sort();
        collisions.dedup();
        assert!(
            collisions.is_empty(),
            "these [GC] summary keys appear on more than one line, so grepping              the summary for them returns unrelated quantities and `| tail -1`              picks an arbitrary one: {collisions:?}. Give the newer line's field              a prefix of its own."
        );
    }
}

#[cfg(test)]
mod concurrent_mark_controller_tests {
    use super::*;
    use crate::g1::G1CollectorConfig;

    /// Test-only `StopTheWorldToken` (I-17). Single-threaded test harness, so
    /// the STW invariant the token witnesses is trivially satisfied.
    #[inline]
    fn stw() -> crate::collector::StopTheWorldToken {
        // SAFETY: single-threaded test harness; no mutator is running.
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

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

    /// **R6, the `VmHeap::Zgc` arm audit, verified against a cycle that
    /// actually moves an object.**
    ///
    /// The production plan's R6 row says: *"58 arms plus a macro; the
    /// affirmative ones (`true`, `(0,0)`) assert facts that are only true for
    /// a non-moving collector, and none of them will fail loudly when they
    /// become wrong."* Compaction (`CRATONVM_ZGC_RELOCATE=1`, 2026-08-13) is
    /// the change that can make them wrong, so the audit is no longer
    /// theoretical.
    ///
    /// The audit's finding is that the two arms which genuinely take a
    /// **pre-GC address** — `watched_pre_gc_addr_survived` and
    /// `pre_gc_addr_did_not_survive` — are already correct, because both
    /// consult the `pointer_map` *before* they reach their ZGC arm. That is a
    /// property worth a test rather than a reading: the ZGC arms themselves
    /// (`is_addr_live`, a registry lookup) answer about the address as it is
    /// *now*, and after a slide the old address holds zeroed bytes. Without
    /// the map check, a survivor that moved would be reported dead at its old
    /// address, and `process_references_after_gc` would drop every reference
    /// to it — the exact H2/HIB-CV-32 shape those predicates were written for.
    ///
    /// The exact edit that trips this: delete either
    /// `pointer_map.contains_key(&addr)` early return.
    /// The `Zgc` arm of [`VmHeap::refill_tlab`] returned `None` until
    /// 2026-09-02, which left the JIT's inline allocator dead on the default
    /// collector: `thread.tlab` stayed empty, the inline bump missed every
    /// time, and every compiled `new` took the helper. It carves a chunk now,
    /// and retiring the buffer hands the unused tail back rather than leaving
    /// a filler object this collector would never reclaim (it sweeps a
    /// registry, not memory).
    #[cfg(feature = "zgc")]
    #[test]
    fn the_zgc_arm_of_refill_tlab_carves_a_chunk_and_takes_its_tail_back() {
        let heap = VmHeap::Zgc(crate::zgc::ZgcRealHeap::new_shared(16 * 1024 * 1024));
        if let VmHeap::Zgc(z) = &heap {
            z.set_vm_tlab_enabled(true);
        }
        let (ptr, size) = heap
            .refill_tlab(64 * 1024)
            .expect("ZGC must hand the VM thread's TLAB a chunk");
        assert!(!ptr.is_null());
        assert!(
            size >= crate::tlab::min_tlab_size() && size <= 64 * 1024,
            "chunk of {size} bytes is outside the requested bounds"
        );
        let mut tlab = unsafe { crate::Tlab::new(ptr, size) };
        assert!(tlab.alloc(128, 8).is_some());
        // The chunk is charged whole at refill; the retire credits the unused
        // tail back, so `allocated` never counts bytes nobody can reach.
        let allocated_before = heap.allocated_bytes();
        tlab.retire();
        assert!(tlab.is_retired());
        assert_eq!(
            allocated_before - heap.allocated_bytes(),
            size - 128,
            "the retired tail must be credited back to the heap"
        );
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        assert_eq!(
            z.vm_tlab_engagement(),
            (1, size, 1, size - 128),
            "(refills, refill_bytes, tails_returned, tail_bytes_returned)"
        );
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn the_pre_gc_address_predicates_are_correct_for_an_object_compaction_moved() {
        let heap = VmHeap::Zgc(crate::zgc::ZgcRealHeap::new_shared(256 * 1024));
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        z.set_tlab_enabled(false);
        // Garbage below, so the survivor has somewhere to slide to.
        for _ in 0..8 {
            z.alloc_object(ClassId::new(1), 4);
        }
        let survivor = z.alloc_object(ClassId::new(1), 0);
        let old_addr = survivor.as_ptr() as usize;

        let live = [old_addr];
        let (moved, _reclaimed, map) = z.relocate_stw_for_test(&live);
        assert_eq!(moved, 1, "the fixture must actually move the survivor");
        assert!(map.contains_key(&old_addr));

        // The address as it is NOW: nothing live is there any more.
        assert!(
            !heap.is_addr_live(old_addr),
            "the old address must not read as live once the object has left it"
        );

        // ...and yet both pre-GC predicates must get the answer RIGHT, because
        // each consults the pointer map before its ZGC arm.
        assert!(
            heap.watched_pre_gc_addr_survived(old_addr, &map),
            "a survivor that MOVED must still count as having survived; \
             without the pointer-map check this reads as death and every \
             reference to it is dropped"
        );
        assert!(
            !heap.pre_gc_addr_did_not_survive(old_addr, &map),
            "and its negation must agree — these two must never disagree \
             about the same address"
        );
    }

    /// The same two predicates must still report a genuinely dead address as
    /// dead when a compacting cycle ran.
    ///
    /// Without this, the test above is satisfied by a predicate that answers
    /// "survived" for everything -- which would disable the stale-pointer
    /// protection entirely rather than fix it.
    ///
    /// This one goes through the real `collect_garbage` rather than calling
    /// the relocator directly, and it has to: an object is only *dead* once
    /// the sweep has removed it from the registry, and `is_addr_live` is a
    /// registry lookup. Driving the relocator alone leaves every allocation
    /// still registered, so a "dead" address reads as live and the test fails
    /// for a fixture reason that says nothing about the predicate. That was
    /// this test's first draft.
    /// Monitor-cleanup stub for the compaction fixtures.
    #[cfg(feature = "zgc")]
    struct R6NoMonitors;
    #[cfg(feature = "zgc")]
    impl MonitorCleanup for R6NoMonitors {
        fn remap_after_gc(&self, _map: &cratonvm_types::PointerMap) {}
        fn prune_dead(&self, _dead: &[usize]) {}
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn a_genuinely_dead_address_is_still_dead_after_a_compacting_cycle() {
        let _serialise = crate::zgc::tests::tests_overlay_lock();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("1"))],
            || {
                let heap = VmHeap::Zgc(crate::zgc::ZgcRealHeap::new_shared(256 * 1024));
                let VmHeap::Zgc(z) = &heap else {
                    unreachable!("constructed as Zgc")
                };
                z.set_tlab_enabled(false);
                let mut dead_addrs = Vec::new();
                for _ in 0..8 {
                    dead_addrs.push(z.alloc_object(ClassId::new(1), 4).as_ptr() as usize);
                }
                let survivor = z.alloc_object(ClassId::new(1), 0);
                let dead = *dead_addrs.last().expect("fixture allocated garbage");

                let mut roots = [survivor];
                // SAFETY: these unit tests run the heap single-threaded.
                let stw = unsafe { crate::collector::StopTheWorldToken::new() };
                let result = z.collect_garbage(&stw, &mut roots, &R6NoMonitors);
                let map = result.pointer_map;

                assert!(
                    !map.contains_key(&dead),
                    "a dead object is not relocated, so it gets no map entry"
                );
                assert!(
                    !heap.watched_pre_gc_addr_survived(dead, &map),
                    "a dead address must still read as dead -- otherwise the \
                     stale-pointer protection is off rather than fixed"
                );
                assert!(heap.pre_gc_addr_did_not_survive(dead, &map));
            },
        );
    }

    /// **The `VmHeap::satb_barrier` ZGC arm actually reaches the collector.**
    ///
    /// This is the test that separates a wired barrier from an inert one, and
    /// it is the only one that can: `ZgcRealHeap`'s own barrier tests call
    /// `satb_pre_barrier` directly, so every one of them would still pass with
    /// this arm back to the `{}` it was until 2026-08-13. The dispatch is the
    /// subject here, not the barrier.
    ///
    /// The exact edit that trips it: empty the `VmHeap::Zgc` arm of
    /// `satb_barrier`.
    #[cfg(feature = "zgc")]
    #[test]
    fn the_vm_heap_satb_arm_reaches_the_zgc_barrier() {
        let heap = VmHeap::Zgc(crate::zgc::ZgcRealHeap::new_shared(64 * 1024));
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        let obj = z.alloc_object(ClassId::new(1), 4);
        z.set_mark_active(true);

        // Through the VM-facing funnel every reference store already calls.
        heap.satb_barrier(Value::Object(Some(obj)));

        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        assert_eq!(
            z.mark_ingress_pushes(),
            1,
            "VmHeap::satb_barrier must reach ZgcRealHeap::satb_pre_barrier"
        );
    }

    /// `set_field_suppress_satb` really suppresses on ZGC, and plain
    /// `set_field` really does not.
    ///
    /// # The bug this exists to have caught
    ///
    /// That method was a plain `set_field` on this backend, and correctly so
    /// for as long as ZGC had no armed pre-write barrier. Concurrent marking
    /// gave it one. `weakref_null_referents_pre_gc` nulls EVERY registered
    /// referent immediately before `collect_garbage` -- i.e. while the cycle is
    /// still armed -- so the unsuppressed arm would have published every active
    /// referent into the ingress, `finish_concurrent_mark` would have replayed
    /// them as mark roots, and no weak, soft, phantom or cleaner reference
    /// could ever have been cleared again.
    ///
    /// It would have been invisible. Every reference test in this tree is
    /// satisfied by "the referent survived", which is exactly what the bug
    /// produces; the only symptom is a leak.
    ///
    /// Both directions are asserted. A suppression that suppressed everything
    /// -- including the ordinary store path -- would pass the half of this test
    /// that matters most and disable the barrier wholesale.
    #[cfg(feature = "zgc")]
    #[test]
    fn zgc_set_field_suppress_satb_suppresses_and_plain_set_field_does_not() {
        let heap = VmHeap::new(GcBackend::Zgc, 1024 * 1024);
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        let holder = heap.alloc_object(ClassId::new(1), 2);
        let a = heap.alloc_object(ClassId::new(1), 0);
        let b = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(holder, 0, Value::Object(Some(a)));
        heap.set_field(holder, 1, Value::Object(Some(b)));
        z.set_mark_active(true);

        let before = z.mark_ingress_pushes();
        heap.set_field_suppress_satb(holder, 0, Value::Object(None));
        assert_eq!(
            z.mark_ingress_pushes(),
            before,
            "the referent-protocol write must NOT reach the concurrent marker"
        );

        let before = z.mark_ingress_pushes();
        heap.set_field(holder, 1, Value::Object(None));
        assert_eq!(
            z.mark_ingress_pushes(),
            before + 1,
            "...while an ordinary store still must, or the suppression has \
             disabled the barrier rather than exempted one caller"
        );

        z.set_mark_active(false);
    }

    /// **The card barrier is reached through `VmHeap`, by BOTH store channels.**
    ///
    /// # Why through the enum and not through `ZgcRealHeap`
    ///
    /// The card barrier's whole history is of being wired to something nothing
    /// calls. Until 2026-08-17 it hung off `GarbageCollector::write_barrier`,
    /// which this backend's `set_field` never invokes -- so `remembered_roots`
    /// had no non-test caller and the remembered set was empty on every real
    /// workload, while the collector-level tests were green. The tests in
    /// `zgc.rs` cannot see that: they call the accessor directly. This one goes
    /// through the dispatch the interpreter goes through.
    ///
    /// Both channels, because `set_field_suppress_satb` exists to skip the OTHER
    /// barrier and must not skip this one: SATB is about a reference being LOST,
    /// a card is about one now being HELD. A single implementation change
    /// (routing the suppression channel around `set_field_no_satb`) would break
    /// exactly one of the two assertions below.
    #[cfg(feature = "zgc")]
    #[test]
    fn the_vm_heap_store_channels_both_reach_the_zgc_card_barrier() {
        let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        z.set_generational_enabled(true);
        z.set_gen_promotion_age(1);
        z.set_relocation_enabled(false);

        let plain = heap.alloc_object(ClassId::new(1), 2);
        let suppressed = heap.alloc_object(ClassId::new(1), 2);
        let target = heap.alloc_object(ClassId::new(1), 0);

        // Promote all three, then let a young cycle clean the promotion cards --
        // otherwise the assertions read a set that is dirty for a reason they
        // did not cause.
        let mut roots = [plain, suppressed, target];
        {
            // SAFETY: single-threaded test.
            let stw = unsafe { crate::collector::StopTheWorldToken::new() };
            let _ = z.collect_garbage(&stw, &mut roots, &R6NoMonitors);
            let _ = z.collect_garbage(&stw, &mut roots, &R6NoMonitors);
        }
        let [plain, suppressed, target] = roots;
        assert!(
            !z.is_carded_for_test(plain.as_ptr() as usize)
                && !z.is_carded_for_test(suppressed.as_ptr() as usize),
            "the young cycle must have cleaned the promotion cards first"
        );

        heap.set_field(plain, 0, Value::Object(Some(target)));
        assert!(
            z.is_carded_for_test(plain.as_ptr() as usize),
            "an ordinary store through VmHeap must card its receiver"
        );

        heap.set_field_suppress_satb(suppressed, 0, Value::Object(Some(target)));
        assert!(
            z.is_carded_for_test(suppressed.as_ptr() as usize),
            "...and so must the SATB-suppressed channel: suppressing the \\
             snapshot barrier must not suppress the card, or every referent \\
             write silently drops an old-to-young edge"
        );
    }

    /// Every `zgc_*_concurrent_mark` arm of `VmHeap` reaches the collector.
    ///
    /// # Why this is a separate test from the ones in `zgc.rs`
    ///
    /// Those exercise `ZgcRealHeap` directly. This exercises the **dispatch**,
    /// and in this enum the dispatch is where a feature goes quietly missing:
    /// every one of these methods has two arms that are a literal `false` /
    /// `{}` / `(0, 0, 0, 0, 0)`, and a fifth arm that reads
    /// `VmHeap::Zgc(_) => false` compiles, passes every collector-level test,
    /// and turns the whole feature off. This tree has shipped exactly that
    /// shape before — an inert registration is indistinguishable from a
    /// missing feature from anywhere except the call site.
    ///
    /// The exact edit that trips it: change any `VmHeap::Zgc(h) => h.…` arm in
    /// the ZGC concurrent-marking block to the neutral value its siblings use.
    #[cfg(feature = "zgc")]
    #[test]
    fn the_vm_heap_zgc_concurrent_arms_reach_the_collector() {
        let heap = VmHeap::new(GcBackend::Zgc, 8 * 1024 * 1024);
        assert!(!heap.zgc_concurrent_mark_active());
        assert_eq!(heap.zgc_concurrent_mark_stats(), (0, 0, 0, 0, 0));

        let holder = heap.alloc_object(ClassId::new(1), 2);
        let child = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(holder, 0, Value::Object(Some(child)));
        let garbage = heap.alloc_object(ClassId::new(1), 0);
        let garbage_addr = garbage.as_ptr() as usize;

        // SAFETY: this test is the only mutator.
        let stw = unsafe { crate::collector::StopTheWorldToken::new() };
        assert!(
            heap.zgc_start_concurrent_mark(&stw, &[holder]),
            "the VmHeap arm must open a cycle, not return a neutral false"
        );
        assert!(heap.zgc_concurrent_mark_active());

        // The mutator ingress, through the funnel every reference store in the
        // VM already reaches.
        let extra = heap.alloc_object(ClassId::new(1), 0);
        heap.set_field(holder, 1, Value::Object(Some(extra)));
        heap.satb_barrier(Value::Object(Some(child)));

        let mut roots = [holder];
        let _ = heap.collect_garbage(&stw, &mut roots, &R6NoMonitors);

        let (started, completed, black, _replayed, _ns) = heap.zgc_concurrent_mark_stats();
        assert_eq!(
            (started, completed),
            (1, 1),
            "the cycle must have been opened AND certified through the VmHeap arms"
        );
        assert!(black >= 1, "the object allocated mid-cycle was born marked");
        assert!(!heap.zgc_concurrent_mark_active());

        assert!(
            heap.is_object_address(child.as_ptr() as usize).is_some(),
            "the live child survived a concurrently-marked collection"
        );
        assert!(
            heap.is_object_address(garbage_addr).is_none(),
            "...and the garbage did not, so this is not just 'nothing was freed'"
        );
    }

    /// ...and the same funnel is inert on ZGC while no cycle is marking, which
    /// is what makes it free to leave wired in every build.
    #[cfg(feature = "zgc")]
    #[test]
    fn the_vm_heap_satb_arm_is_inert_on_zgc_while_not_marking() {
        let heap = VmHeap::Zgc(crate::zgc::ZgcRealHeap::new_shared(64 * 1024));
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        let obj = z.alloc_object(ClassId::new(1), 4);

        heap.satb_barrier(Value::Object(Some(obj)));

        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        assert_eq!(z.mark_ingress_pushes(), 0);
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
        heap.g1_start_concurrent_mark(&stw());
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
        heap.g1_signal_marking_complete(&stw());
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
            heap.g1_start_concurrent_mark(&stw());
            heap.g1_signal_marking_complete(&stw());
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

        heap.g1_start_concurrent_mark(&stw());
        let first_arc_count = Arc::strong_count(&g1_state(&heap).unwrap().collector);

        // Second start without complete in between — must not leak.
        heap.g1_start_concurrent_mark(&stw());
        assert!(
            g1_state(&heap).unwrap().has_active_controller(),
            "a controller must still be parked after the second start",
        );

        // Single signal-complete drains everything.
        heap.g1_signal_marking_complete(&stw());
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
        heap.g1_signal_marking_complete(&stw()); // no-op path
        assert!(!heap.g1_is_marking_active());
        assert!(!g1_state(&heap).unwrap().has_active_controller());

        // Calling it twice (after a real cycle then a stray call) must
        // also be a no-op.
        heap.g1_start_concurrent_mark(&stw());
        heap.g1_signal_marking_complete(&stw());
        heap.g1_signal_marking_complete(&stw()); // stray second call
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

        heap.g1_start_concurrent_mark(&stw());

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
        // The pre-barrier writes through the thread-local SATB buffer, so the
        // global queue may not see it yet. Flush this thread's buffer.
        heap.flush_thread_satb();

        // FLAKE FIX: `after > before` alone is a race, and it is the same
        // under-approximation `g1_concurrent::satb_captures_mutator_writes_during_concurrent_mark`
        // already documents. `g1_start_concurrent_mark()` above spawns a real
        // background marker, and since G1MARK-6 `concurrent_mark_step` drains
        // the queue shards on every step — so the worker can consume the entry
        // between the flush and this read, leaving `after == before == 0` on a
        // queue that did exactly what it was supposed to. Measured at roughly
        // one failure in twenty full-suite runs on a loaded host; it needs the
        // worker scheduled inside a sub-millisecond window.
        //
        // Assert the SATB *guarantee* instead of the transient: the overwritten
        // reference reached the marker either by still being queued, or by
        // already having been pulled into the gray set / marked. Both routes
        // are the barrier working; only neither is a bug.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        let mut condition = false;
        let mut final_after = 0;
        let mut final_grayed = false;
        while std::time::Instant::now() < deadline {
            let after = satb_queue.len();
            let grayed_or_marked = g1_state(&heap)
                .unwrap()
                .collector
                .dbg_is_grayed_or_marked(obj.as_ptr() as usize);
            final_after = after;
            final_grayed = grayed_or_marked;
            if after > before || grayed_or_marked {
                condition = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            condition,
            "satb_barrier during concurrent mark must deliver the old reference \
             to the marker (before={before}, after={final_after}, \
             grayed_or_marked={final_grayed})",
        );

        // Cleanup so we don't strand the worker.
        heap.g1_signal_marking_complete(&stw());
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
            unsafe { small.as_ptr().add(ARRAY_DATA_OFFSET) },
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
            unsafe { large.as_ptr().add(ARRAY_DATA_OFFSET) },
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

    /// `committed_bytes` is a CAPACITY, and `Runtime.totalMemory()` rests on
    /// that: allocating must move `allocated_bytes` and leave `committed_bytes`
    /// alone. A version that tracked occupancy would report a `totalMemory()`
    /// that changes on every collection, which callers written as "gc until
    /// totalMemory settles" read as "the heap is still resizing".
    #[test]
    fn committed_bytes_is_capacity_not_occupancy() {
        for backend in [GcBackend::Generational, GcBackend::G1] {
            let heap = VmHeap::new(backend, 64 * 1024 * 1024);
            let committed_before = heap.committed_bytes();
            let allocated_before = heap.allocated_bytes();
            assert!(
                committed_before > 0,
                "{backend:?}: committed heap must be positive"
            );
            for _ in 0..4000 {
                let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
            }
            // Non-vacuity: if the allocations did not register, the equality
            // below would hold for the wrong reason.
            assert!(
                heap.allocated_bytes() > allocated_before,
                "{backend:?}: the fixture must actually allocate"
            );
            assert_eq!(
                heap.committed_bytes(),
                committed_before,
                "{backend:?}: committed heap moved while only occupancy changed"
            );
            assert!(
                heap.committed_bytes() >= heap.allocated_bytes(),
                "{backend:?}: committed heap is below what is allocated in it"
            );
        }
    }

    /// The old `Runtime.totalMemory()` answered a hardcoded 64 MiB regardless
    /// of `-Xmx`. Two heaps sized an order of magnitude apart must not report
    /// the same committed bytes.
    #[test]
    fn committed_bytes_follows_the_configured_heap_size() {
        for backend in [GcBackend::Generational, GcBackend::G1] {
            let small = VmHeap::new(backend, 8 * 1024 * 1024).committed_bytes();
            let large = VmHeap::new(backend, 128 * 1024 * 1024).committed_bytes();
            assert!(
                large > small,
                "{backend:?}: committed heap did not follow the configured size \
                 ({small} vs {large})"
            );
        }
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

    /// KINDOF-SENTINEL (2026-08-04): `kind_of`/`element_type_of`/
    /// `identity_hash_code`/`class_id_of`/`load_and_forward` all used to
    /// dereference `obj` unconditionally, trusting the caller. Reproduced in
    /// the wild as the all-ones sentinel `0xFFFFFFFFFFFFFFFF` reaching each
    /// of these from a JIT bail-to-interpreter transition — a hard
    /// `EXCEPTION_ACCESS_VIOLATION`, not a wrong answer. Every one of these
    /// accessors must now fall back to a safe default instead of faulting.
    #[test]
    fn heap_accessors_reject_an_invalid_object_pointer_instead_of_faulting() {
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // 8-byte aligned so only the region-bounds check (not the alignment
        // check) is exercised — the observed all-ones sentinel fails both,
        // and either failure mode must be rejected the same way.
        let sentinel = unsafe {
            ObjectRef::from_raw_nonnull(
                std::ptr::NonNull::new(0xFFFF_FFFF_FFFF_FFF8u64 as *mut u8).unwrap(),
            )
        };

        assert_eq!(heap.kind_of(sentinel), ObjectKind::Object);
        assert_eq!(heap.element_type_of(sentinel), ArrayElementType::Reference);
        assert_eq!(heap.identity_hash_code(sentinel), 0);
        assert_eq!(heap.class_id_of(sentinel), cratonvm_types::ClassId::new(0));
        assert_eq!(
            heap.load_and_forward(sentinel).as_ptr(),
            sentinel.as_ptr(),
            "an invalid obj has nothing valid to forward to; must return unchanged, not fault"
        );
    }

    /// The forwarding-target half of the same fix: a live, validly-addressed
    /// object whose header bytes are corrupted can have its `forwarded` bit
    /// spuriously set with a garbage `forwarding_ptr` — `load_and_forward`
    /// must not hand that garbage address back to the caller.
    #[test]
    fn load_and_forward_rejects_a_forwarding_target_outside_every_region() {
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap
            .try_alloc_object(cratonvm_types::ClassId::new(0), 8)
            .unwrap();
        // SAFETY: test-only corruption of a live header to simulate the
        // observed implausible-header family (see `old_gen::scan_region`'s
        // `validate_header_tags_or_desync`) reaching the forwarding word.
        unsafe {
            let header = &mut *(obj.as_ptr() as *mut ObjectHeader);
            // Raw bits: `set_forwarding_address` asserts a plausible target,
            // and an implausible one is exactly what this test installs.
            header.mark_word.store(
                0xFFFF_FFFF_FFFF_FFF8u64 | cratonvm_types::MARK_FORWARDED,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        assert_eq!(
            heap.load_and_forward(obj).as_ptr(),
            obj.as_ptr(),
            "an implausible forwarding target must fall back to the original pointer"
        );
    }
}

#[cfg(test)]
mod pin_capability_tests {
    use super::{GcBackend, VmHeap};

    /// `honours_conservative_pins` is spent by the cross-thread JIT coverage
    /// handshake: `conservative_roots::refresh_moving_young_coverage_for_collection`
    /// passes it to `pinned_credit_admissible`, and a `true` there lets a moving
    /// cycle proceed while a peer thread holds compiled frames nobody proved
    /// rewritable — on the promise that the objects those frames name will not
    /// move. Only a backend that can WITHHOLD an object can keep that promise.
    ///
    /// The generational young collector cannot, and the reason is structural
    /// rather than a policy choice: it is Cheney copying, from-space is
    /// reclaimed wholesale, so every live object in it moves by construction and
    /// there is no "withhold this one" to implement. `gen_heap.rs` and
    /// `gen_evac.rs` accordingly contain no reader of
    /// `gc_quiescence::pinned_jit_roots_snapshot()` at all.
    ///
    /// That arm read `true` until 2026-09-06, and the cost was a wrong ANSWER,
    /// not a slow one:
    /// `docs/internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
    /// (19 netty classes, Generational only, an NPE on a live JUnit object) and
    /// the ten-second H2 SIGSEGV in `70c486744`'s call-site comment. The rule
    /// itself is tested next to `pinned_credit_admissible`; this is the other
    /// half of it — the input — and without this test a "simplification" of the
    /// match below fails a netty suite instead of a unit test.
    #[test]
    fn honours_conservative_pins_is_a_capability_not_a_policy() {
        // Small heaps: this asks a question about the backend, not about
        // capacity, and the sizes match the ones the sibling tests in this file
        // already construct.
        const BYTES: usize = 8 * 1024 * 1024;
        assert!(
            !VmHeap::new(GcBackend::Generational, BYTES).honours_conservative_pins(),
            "the Cheney young collector reclaims from-space wholesale, so it \
             cannot honour a pin and must not let one discharge a peer's \
             coverage obligation"
        );
        assert!(
            VmHeap::new(GcBackend::G1, BYTES).honours_conservative_pins(),
            "G1 withholds the pinned regions from the collection set"
        );
        #[cfg(feature = "zgc")]
        assert!(
            VmHeap::new(GcBackend::Zgc, BYTES).honours_conservative_pins(),
            "ZGC withholds the pinned pages; `relocate_stw` reads the same snapshot"
        );
    }
}
