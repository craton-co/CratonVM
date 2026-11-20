#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;


/// The most fields [`VmHeap::copy_compact_object_fields`] plans in one copy.
pub(in crate::vm_heap) const COMPACT_COPY_MAX_SLOTS: usize = 64;
/// The largest instance body (bytes) [`VmHeap::copy_compact_object_fields`]
/// copies; G1's humongous backing keeps its scalar door above it.
pub(in crate::vm_heap) const COMPACT_COPY_MAX_BYTES: usize = 1024;

/// One copy plan: `(byte offset, storage)` per field slot.
pub(in crate::vm_heap) type CompactCopyPlan = [(usize, cratonvm_types::FieldStorageKind); COMPACT_COPY_MAX_SLOTS];

/// G1 collector + slot for its current concurrent-mark controller.
///
/// Task #56: `VmHeap::g1_start_concurrent_mark` and
/// `VmHeap::g1_signal_marking_complete` are the spawn/join boundary
/// for the background marker. The controller's lifetime is one mark
/// cycle, parked in `Mutex<Option<…>>`. `Deref<Target = G1Collector>`
/// keeps existing `VmHeapInner::G1(h) => h.method()` dispatch sites compiling
/// unchanged.
pub struct G1State {
    /// `Arc` so the controller's worker thread can hold a clone for
    /// the duration of its cycle.
    pub collector: Arc<G1Collector>,
    /// Active controller, or `None` between cycles.
    pub(in crate::vm_heap) concurrent_mark: Mutex<Option<ConcurrentMarkController>>,
    /// HotSpot G1's JMX beans (`G1 Young Generation` / `G1 Old Generation` /
    /// `G1 Concurrent GC`, three `G1 ...` pools) and their GC-notification
    /// queue, per heap and so per VM. Fed by the common GC event plumbing
    /// (`vm/src/runtime/interpreter/gc_events.rs`), not by the collector.
    /// `Arc` so a marking pause's timer can hold it without borrowing the
    /// heap. gc-common w7-f.
    pub(in crate::vm_heap) gc_beans: Arc<cratonvm_gc_core::gc_metrics::BackendGcBeans>,
}

impl G1State {
    pub fn new(config: G1CollectorConfig) -> Self {
        Self {
            collector: Arc::new(G1Collector::new(config)),
            concurrent_mark: Mutex::new(None),
            gc_beans: Arc::new(cratonvm_gc_core::gc_metrics::BackendGcBeans::new(
                cratonvm_gc_core::gc_metrics::BackendBeanShape::G1,
            )),
        }
    }

    /// This heap's JMX bean state. gc-common w7-f; see the field.
    pub fn gc_beans(&self) -> &Arc<cratonvm_gc_core::gc_metrics::BackendGcBeans> {
        &self.gc_beans
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

/// Samples a G1 or ZGC heap's JMX pools ([`VmHeap::jmx_pool_sampler`]).
/// gc-common w7-f.
pub enum JmxPoolSampler {
    G1(Arc<G1Collector>),
    #[cfg(feature = "zgc")]
    Zgc(Arc<ZgcRealHeap>),
}

impl JmxPoolSampler {
    /// The pools' usage now, in [`cratonvm_gc_core::gc_metrics::BackendBeanShape::pools`]
    /// order:
    ///
    /// * G1 -- `G1 Eden Space` (Eden regions), `G1 Survivor Space` (Survivor
    ///   regions), `G1 Old Gen` (Old + humongous regions' bytes), from ONE
    ///   [`G1Collector::region_census`] walk. The committed prefix is split as
    ///   HotSpot's `G1MonitoringSupport` splits it (`g1_pool_committed_split`,
    ///   gci i18/sw: eden up to the young target, old the rest; it used to be
    ///   eden = its current regions, so eden's committed read ~0 after every
    ///   pause). `max` is undefined for the young pools and the heap's capacity
    ///   for the old one, as HotSpot reports. The same USED partition as
    ///   [`VmHeap::young_gen_stats`] / [`VmHeap::old_gen_stats`].
    /// * ZGC -- HotSpot 25's `ZGC Young Generation` and `ZGC Old Generation`
    ///   (gcd d10/o retirement; one `ZHeap` until then). This ZGC keeps no
    ///   separate young space, so the young pool is empty (0 used, 0
    ///   committed) and the old pool carries the heap: allocated bytes,
    ///   committed granules. Both pools' `max` is the heap's capacity, as on
    ///   HotSpot 25, where either generation may grow to the whole heap.
    ///
    /// `init` (gcd d10/o retirement; it was undefined, `-1`, everywhere) is
    /// the heap's startup commit, attributed to the pool that holds the heap
    /// -- `G1 Old Gen`, `ZGC Old Generation` -- and `0` for the young pools,
    /// so the pools' `init` sum to the heap's initial size as HotSpot's do.
    /// (HotSpot splits its initial size by its own young sizing, which these
    /// collectors do not have.) Reporting call; takes the G1 region-table read lock or
    /// the ZGC arena lock, so never call it while holding either.
    pub fn sample(&self) -> Vec<cratonvm_gc_core::gc_metrics::JmxPoolSample> {
        use cratonvm_gc_core::gc_metrics::JmxPoolSample;
        // Widening: usize to u64 on every supported target.
        let b = |n: usize| n as u64;
        match self {
            JmxPoolSampler::G1(c) => {
                let initial = Some(b(c.initial_heap_bytes()));
                let census = c.region_census();
                let old_used = census.old.0 + census.humongous.0;
                // gci i18/sw (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
                // item 5): HotSpot's committed split, see `g1_pool_committed_split`.
                let (eden_committed, survivor_committed, old_committed) = g1_pool_committed_split(
                    c.committed_bytes(),
                    census.region_size,
                    census.eden.1,
                    census.survivor.1,
                    old_used,
                    c.young_target_regions(),
                );
                vec![
                    JmxPoolSample {
                        init: Some(0),
                        used: b(census.eden.0),
                        committed: b(eden_committed),
                        max: None,
                    }
                    .normalized(),
                    JmxPoolSample {
                        init: Some(0),
                        used: b(census.survivor.0),
                        committed: b(survivor_committed),
                        max: None,
                    }
                    .normalized(),
                    JmxPoolSample {
                        init: initial,
                        used: b(old_used),
                        committed: b(old_committed),
                        max: Some(b(c.heap_capacity())),
                    }
                    .normalized(),
                ]
            }
            #[cfg(feature = "zgc")]
            JmxPoolSampler::Zgc(z) => {
                let max = Some(b(z.heap_capacity()));
                // zr z2/t2 (`CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE`, default on):
                // what has not survived a collection is young, what has is
                // old, old `committed == used` -- HotSpot 25 ZGC's pools.
                // `init` stays on the old pool, as below.
                // zr z3/t2 (host, `Zr3T2HeapVsPools` on HotSpot 25): the young
                // pool's committed is the heap's commit less the old pool's
                // (`Young used=2097152 committed=264241152` beside `Old
                // used=committed=4194304` at `-Xmx256m`), so the pools'
                // committed sum to the heap's -- `getHeapMemoryUsage()`.
                if let Some((young, old)) = z.jmx_young_old_split() {
                    return vec![
                        JmxPoolSample {
                            init: Some(0),
                            used: b(young),
                            committed: b(zgc_young_pool_committed(
                                z.os_committed_bytes(),
                                young,
                                old,
                            )),
                            max,
                        },
                        JmxPoolSample {
                            init: Some(b(z.initial_committed_bytes())),
                            used: b(old),
                            committed: b(old),
                            max,
                        },
                    ];                }
                vec![
                    JmxPoolSample {
                        init: Some(0),
                        used: 0,
                        committed: 0,
                        max,
                    },
                    JmxPoolSample {
                        init: Some(b(z.initial_committed_bytes())),
                        used: b(z.allocated_bytes()),
                        committed: b(z.os_committed_bytes()),
                        max,
                    }
                    .normalized(),
                ]
            }
        }
    }
}

/// gci i18/sw (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 5): the committed bytes of `G1 Eden Space`, `G1 Survivor Space` and
/// `G1 Old Gen`, split as HotSpot 25's `G1MonitoringSupport::recalculate_sizes`
/// splits the heap's capacity: survivor = its regions; old = its used bytes
/// rounded up to whole regions; eden = what is left, up to the young target
/// minus the survivors (HotSpot's `young_list_max_length - survivors`); old
/// takes the remainder. So eden's committed is its TARGET, not its current
/// regions (HotSpot's eden committed stays well above eden's used right after
/// a pause), and the three still sum to `committed`. Two departures, both to
/// keep `used <= committed` without the sample's `normalized` widening a pool
/// past the heap: eden's bound is never below its current regions (this
/// collector's eden may run past a lowered adaptive target), and a census read
/// against a committed figure that moved in between leaves eden 0 and old at
/// its floor rather than underflowing. Pure.
pub(in crate::vm_heap) fn g1_pool_committed_split(
    committed: usize,
    region_size: usize,
    eden_regions: usize,
    survivor_regions: usize,
    old_used: usize,
    young_target_regions: usize,
) -> (usize, usize, usize) {
    let rs = region_size.max(1);
    let survivor = survivor_regions.saturating_mul(rs);
    let old_floor = old_used.div_ceil(rs).saturating_mul(rs);
    let mut rest = committed.saturating_sub(survivor.saturating_add(old_floor));
    // gci i19 (orchestrator, vi18-sw U `sw18_cu1_g1`): HotSpot's
    // `young_list_max_length` is sized from the regions committed NOW
    // (`G1MaxNewSizePercent` of them), never from the maximum heap, so old
    // keeps at least the other 40 % of the committed heap. Bounding eden by
    // the max-heap target alone gave eden the whole committed prefix after a
    // `System.gc()` that left the live data young: old committed read 0.
    let committed_young_max =
        (committed / rs).saturating_mul(cratonvm_gc_g1::g1::G1_MAX_NEW_PERCENT) / 100;
    let eden_bound = young_target_regions
        .min(committed_young_max.max(1))
        .saturating_sub(survivor_regions)
        .max(eden_regions)
        .saturating_mul(rs);
    let eden = rest.min(eden_bound);
    rest -= eden;
    (eden, survivor, old_floor.saturating_add(rest))
}


/// zr z3/t2: HotSpot 25 ZGC's young-pool `committed` -- the heap's commit
/// less the old pool's committed (which equals old used), never below young
/// used. With it the two ZGC pools' committed sum to `max(heap committed,
/// used)`, which is what `getHeapMemoryUsage().getCommitted()` answers after
/// its own `[used, max]` clamp (host: HotSpot `committed_equal=true`).
#[cfg_attr(not(feature = "zgc"), allow(dead_code))]
pub(in crate::vm_heap) fn zgc_young_pool_committed(heap_committed: usize, young_used: usize, old: usize) -> usize {
    heap_committed.saturating_sub(old).max(young_used)
}

/// Which GC backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcBackend {
    /// Generational semi-space young generation + free-list old generation.
    /// The default only in a `--no-default-features` build; with the default
    /// feature set the default is [`GcBackend::Zgc`] (`VmConfig::default`).
    /// This doc said "(default)" until 2026-09-23, six weeks after the flip.
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
    /// ZGC-real memory-backed stop-the-world collector — the DEFAULT backend
    /// of a default build (the `zgc` Cargo feature is default-on).
    #[cfg(feature = "zgc")]
    Zgc,
}

/// Explicit huge-page request carried by heap sizing options in every feature set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZgcExplicitPages {
    /// Bytes HotSpot's code cache takes from the pool first (`0` for none).
    pub code_heap_bytes: usize,
    /// Run HotSpot's uncommit-support probe (`ZUncommit` on and the minimum
    /// heap below the maximum, as `try_enable_uncommit` asks it only then):
    /// commit one granule and give it back; a refusal is one more `Failed to
    /// commit memory` line and `Uncommit: Implicitly Disabled (Not supported
    /// by operating system)` (host row `zki_h3`).
    pub uncommit_probe: bool,
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
pub(in crate::vm_heap) const G1_TARGET_REGION_COUNT: usize = 2048;

/// Round `requested` to a power of two inside
/// `[MIN_REGION_SIZE, G1_MAX_REGION_SIZE]`.
///
/// Power-of-two because the collector's address→region lookup is a shift (see
/// `g1::normalize_region_size`); clamped because both ends of the range are
/// operator-facing policy rather than arithmetic.
pub fn clamp_region_size(requested: usize) -> usize {
    let clamped = requested.clamp(cratonvm_gc_core::region::MIN_REGION_SIZE, G1_MAX_REGION_SIZE);
    let rounded = cratonvm_gc_g1::g1::normalize_region_size(clamped);
    // Rounding UP can leave the ceiling; rounding down to the ceiling keeps it
    // a power of two because `G1_MAX_REGION_SIZE` is one.
    rounded.min(G1_MAX_REGION_SIZE)
}

/// gci i11/gm finding 13 — HotSpot 25's `G1HeapRegionBounds::max_size()`, the
/// ceiling of an EXPLICIT `-XX:G1HeapRegionSize` (`range(0, 512M)` on 64-bit;
/// the launcher refuses a larger value with HotSpot's range error).
pub const G1_MAX_EXPLICIT_REGION_SIZE: usize = 512 * 1024 * 1024;

/// gci i11/gm finding 13 — an explicit `-XX:G1HeapRegionSize` as HotSpot 25
/// applies it (measured on 25.0.3: `512k` → `1M`, `1m` → `1M`, `64m` → `64M`):
/// raised to the 1 MiB minimum (`G1HeapRegionBounds::min_size()`), rounded UP to
/// a power of two (`G1HeapRegion::setup_heap_region_size`), at most 512 MiB.
/// Only the ERGONOMIC value is held to 32 MiB ([`G1_MAX_REGION_SIZE`]).
///
/// One deviation, to keep the collector's "the heap holds at least one region"
/// invariant: the result never exceeds the largest power of two not above
/// `total_bytes` (HotSpot would instead align the heap up to the region size).
pub fn explicit_region_size(requested: usize, total_bytes: usize) -> usize {
    let min = cratonvm_gc_core::region::DEFAULT_REGION_SIZE;
    let rounded = cratonvm_gc_g1::g1::normalize_region_size(
        requested.clamp(min, G1_MAX_EXPLICIT_REGION_SIZE),
    )
    .min(G1_MAX_EXPLICIT_REGION_SIZE);
    let heap_cap = if total_bytes == 0 {
        min
    } else {
        1usize << (usize::BITS - 1 - total_bytes.leading_zeros())
    };
    rounded.min(heap_cap.max(min))
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
        (total_bytes / G1_TARGET_REGION_COUNT).max(cratonvm_gc_core::region::DEFAULT_REGION_SIZE),
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
    /// `-XX:G1MixedGCLiveThresholdPercent=<n>` (clamped to 1..=100) — an Old
    /// region at or above this percent live is never a mixed candidate.
    pub mixed_gc_live_threshold_percent: Option<u8>,
    /// `-XX:G1HeapWastePercent=<n>` (clamped to 0..=100) — the mixed phase
    /// ends once the candidates' garbage is below this percent of the heap.
    pub heap_waste_percent: Option<u8>,
    /// zki (page `zgc-zt23hunt4-*` item 4): HotSpot's `Explicit` large-page
    /// state (`-XX:+UseLargePages` with a usable hugetlbfs pool), decided by
    /// the VM: `Some` (the code cache's share of the pool, whether to run the
    /// uncommit probe), and the ZGC heap is backed by hugetlbfs pages
    /// (`ZgcRealHeap::with_capacity_initial_and_large_pages`). Read by the ZGC
    /// arm only.
    pub zgc_explicit_large_pages: Option<cratonvm_gc_zgc::zgc::ZgcExplicitPages>,
}

/// What a backend did with `-Xms` — the number of bytes the operator asked the
/// VM to COMMIT at startup. Reported by [`VmHeap::new_with_heap_sizing`].
///
/// # Why `-Xms` is a parameter with a reported disposition
///
/// Until 2026-09-20 `-Xms` travelled as a `G1ConfigOverrides` field, and the
/// struct's name is exactly why the bug lasted: only the `GcBackend::G1` arm of
/// the constructor ever read it, so `-Xms512m -Xmx8g` on the DEFAULT collector
/// was accepted and dropped with no diagnostic
/// (`docs/feature-designs/generative-gc-architecture.md`).
///
/// Two halves fix that, and only together. Moving the value to a PARAMETER puts
/// it in scope in all three arms — an arm can no longer fail to see it. Making
/// the disposition part of the RETURN TYPE is what makes an arm that ignores it
/// a compile error rather than a silent drop: every arm must name what it did,
/// and a fourth backend cannot be added without answering the question.
///
/// # `Dropped` is unreachable today, and stays
///
/// *2026-09-21.* All three backends now commit: G1 through
/// `G1CollectorConfig::initial_heap_size`, ZGC through
/// `ZgcRealHeap::with_capacity_and_initial`, Generational through
/// `GenerationalHeap::commit_initial_heap`. Nothing in the tree constructs
/// [`Self::Dropped`] any more.
///
/// It is kept because the variant is the *question*, not the bug: a fourth
/// backend, or a future one whose store cannot commit on command, has to be
/// able to say so, and deleting the variant would leave it with only the two
/// answers that claim a commit it did not make. An unconstructed variant is a
/// cheap thing to carry; a backend forced to lie is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmsDisposition {
    /// No `-Xms` was supplied, or it was zero. Nothing to honour or drop.
    NotSupplied,
    /// The backend will commit this many bytes at startup.
    Committed(usize),
    /// The backend cannot size its startup commit separately from `-Xmx`, so
    /// the request had no effect. Carries the bytes that were asked for, so the
    /// caller can name them in a diagnostic instead of staying quiet.
    Dropped(usize),
}

// ─── GPU-offload coordination (Phase 6 item 1) ───────────────────────────
//
// Process-wide counter tracking the number of live SafepointTokens
// (see `cratonvm_gc_core::safepoint`). While non-zero, every collect_garbage
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

pub use cratonvm_gc_core::gpu_critical::{
    GPU_CRITICAL_COUNT, GpuCriticalDrain, gpu_relocation_forbidden, wait_for_gpu_critical_drain,
};



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
    pub(in crate::vm_heap) static PENDING_PRE_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}



/// Opt-out for the young-mirror deferral in [`VmHeap::mirror_pin_deferrable`]
/// (`CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1` restores the old old-gen-only
/// behaviour). Read once — consulted per class mirror per root scan.
#[inline]
pub(in crate::vm_heap) fn mirror_pin_young_defer_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !false
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
pub(in crate::vm_heap) fn validate_once_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !false
    })
}

#[cfg(debug_assertions)]
#[inline]
pub(in crate::vm_heap) fn arm_pending_pre_barrier() {
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
pub(in crate::vm_heap) fn clear_pending_pre_barrier() {
    PENDING_PRE_BARRIER.with(|f| f.set(false));
}

/// A [`VmHeap::reclaimed_hole_at`] prepared for many addresses in one window in
/// which nothing allocates (gc-common w16-x). Only the Generational arm has
/// anything to capture. G1 answers `None` for free, and ZGC keeps its
/// per-address answer (see the note on the handoff's follow-ups).
pub enum ReclaimedHoleProbe {
    Generational(cratonvm_gc_gen::gen_heap::YoungReclaimProbe),
    PerAddress,
}

/// Unified heap wrapping one of the three backends: `GenerationalHeap`,
/// `G1Collector` or (default build) `ZgcRealHeap`.
///
/// Provides the same API surface as `GenerationalHeap` so existing call sites
/// work unchanged. Where a method does not apply to a backend the arm returns a
/// neutral value. New arms of that kind should be spelled out per backend
/// rather than left to a `_ =>` catch-all, so a backend that grows the
/// capability gets a compile error here instead of a silent constant (see
/// [`Self::bytes_promoted_total`] for the incident that rule comes from). The
/// hard-coded arms that remain — several diagnostic-only ones still use `_ =>`
/// — are tabulated in `docs/feature-designs/generative-gc-architecture.md`.
pub struct VmHeap {
    pub(crate) inner: VmHeapInner,
}

impl VmHeap {
    #[inline]
    pub(crate) fn from_inner(inner: VmHeapInner) -> Self {
        Self { inner }
    }
}

pub(crate) enum VmHeapInner {
    Generational(GenerationalHeap),
    G1(G1State),
    /// # Why an `Arc` and not the heap by value
    ///
    /// Genuine concurrent marking (2026-08-16) needs the marking engine's
    /// worker threads to keep tracing **after** the mark-start safepoint
    /// returns, and [`cratonvm_gc_zgc::zgc::mark::ZMarkCoordinator::new`] takes an
    /// `Arc<dyn ZMarkContext>`. `ZgcRealHeap` *is* that context, so the only
    /// two ways to hand it over are an `Arc` or a raw-pointer bridge whose
    /// soundness argument degrades from "cannot outlive one `&self` call" to
    /// "the heap is never moved", which nothing enforces. This is the honest
    /// one, and `Arc<T>: Deref<Target = T>` keeps every existing
    /// `VmHeapInner::Zgc(h) => h.method()` call site compiling unchanged --
    /// `ZgcRealHeap` has no `&mut self` method.
    #[cfg(feature = "zgc")]
    Zgc(std::sync::Arc<ZgcRealHeap>),
}

// Safety: both inner types are already Send + Sync.
unsafe impl Send for VmHeap {}
unsafe impl Sync for VmHeap {}

impl Drop for VmHeap {
    fn drop(&mut self) {
        #[cfg(feature = "gpu-offload")]
        {
            let spans = self.backing_arena_spans();
            self.gpu_coord().run_heap_unpin_hooks_for_spans(&spans);
        }
    }
}

impl VmHeap {
    /// Per-heap (per-VM) GPU coordination context (`gpu_coord`).
    #[cfg(feature = "gpu-offload")]
    #[inline]
    pub fn gpu_coord(&self) -> &std::sync::Arc<gpu_coordination::GpuCoordContext> {
        match &self.inner {
            VmHeapInner::Generational(h) => &h.external_gpu.gpu_coord,
            VmHeapInner::G1(h) => &h.collector.external_gpu.gpu_coord,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => &h.external_gpu.gpu_coord,
        }
    }
}

/// HotSpot's `arrayOopDesc::max_array_length` for every element type,
/// `Integer.MAX_VALUE - 2`; `vm/src/runtime/interpreter/gc_and_alloc.rs::
/// MAX_JAVA_ARRAY_LENGTH` reads this constant. Enforced HERE, below every front
/// end (the fallible array allocators refuse a longer array), because
/// `jit/src/ir_check_elim.rs` (`VM_MAX_ARRAY_LENGTH`) removes bounds checks on
/// the strength of it (round 12 wave 3, BCE for strides 2 and 3).
pub const VM_MAX_ARRAY_LENGTH: usize = (i32::MAX as usize) - 2;

/// `CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL` (zt t29/g1, G1 carry page item 10a;
/// default ON, `=0` kill): `-XX:+AlwaysPreTouch` touches the G1 and
/// generational heaps' committed pages and their `gc,init` block says
/// `Pre-touch: Enabled`, as HotSpot 25 G1 / Serial
/// ([`VmHeap::set_always_pre_touch_non_zgc`]). `=0`: the option is accepted
/// and ignored on those two collectors, as before. ZGC keeps its own
/// `CRATONVM_ZGC_ALWAYS_PRE_TOUCH`. Read once per VM start, only when the
/// option is given.
pub fn gc_always_pre_touch_all_enabled() -> bool {
    true
}

impl GarbageCollector for VmHeap {
    #[inline]
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_object(class_id, num_fields)
    }

    #[inline]
    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        self.alloc_array(class_id, element_type, length)
    }

    #[inline]
    fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        self.try_alloc_object(class_id, num_fields)
    }

    #[inline]
    fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        self.try_alloc_array(class_id, element_type, length)
    }

    #[inline]
    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        self.get_header(obj)
    }

    #[inline]
    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.class_id_of(obj)
    }

    #[inline]
    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.kind_of(obj)
    }

    #[inline]
    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.element_type_of(obj)
    }

    #[inline]
    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        self.identity_hash_code(obj)
    }

    #[inline]
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field(obj, index)
    }

    #[inline]
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value)
    }

    #[inline]
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field_volatile(obj, index)
    }

    #[inline]
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field_volatile(obj, index, value)
    }

    #[inline]
    fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        self.get_field_as(obj, index, desc_byte)
    }

    #[inline]
    fn get_field_volatile_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        self.get_field_volatile_as(obj, index, desc_byte)
    }

    #[inline]
    fn set_field_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        self.set_field_as(obj, index, value, desc_byte)
    }

    #[inline]
    fn set_field_volatile_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        self.set_field_volatile_as(obj, index, value, desc_byte)
    }

    #[inline]
    fn array_length(&self, obj: ObjectRef) -> usize {
        self.array_length(obj)
    }

    #[inline]
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        self.get_array_element(obj, index)
    }

    #[inline]
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.set_array_element(obj, index, value)
    }

    #[inline]
    fn needs_gc(&self) -> bool {
        self.needs_gc()
    }

    #[inline]
    fn collect_garbage(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        self.collect_garbage(stw, roots, monitors)
    }

    #[inline]
    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        self.write_barrier(obj, stored_value)
    }

    #[inline]
    fn write_barrier_pre(&self, slot: *mut ObjectRef, old: ObjectRef) {
        self.write_barrier_pre(slot, old)
    }

    #[inline]
    fn allocated_bytes(&self) -> usize {
        self.allocated_bytes()
    }
}

