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

/// The most fields [`VmHeap::copy_compact_object_fields`] plans in one copy.
const COMPACT_COPY_MAX_SLOTS: usize = 64;
/// The largest instance body (bytes) [`VmHeap::copy_compact_object_fields`]
/// copies; G1's humongous backing keeps its scalar door above it.
const COMPACT_COPY_MAX_BYTES: usize = 1024;

/// One copy plan: `(byte offset, storage)` per field slot.
type CompactCopyPlan = [(usize, cratonvm_types::FieldStorageKind); COMPACT_COPY_MAX_SLOTS];

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
    /// HotSpot G1's JMX beans (`G1 Young Generation` / `G1 Old Generation` /
    /// `G1 Concurrent GC`, three `G1 ...` pools) and their GC-notification
    /// queue, per heap and so per VM. Fed by the common GC event plumbing
    /// (`vm/src/runtime/interpreter/gc_events.rs`), not by the collector.
    /// `Arc` so a marking pause's timer can hold it without borrowing the
    /// heap. gc-common w7-f.
    gc_beans: Arc<crate::gc_metrics::BackendGcBeans>,
}

impl G1State {
    pub fn new(config: G1CollectorConfig) -> Self {
        Self {
            collector: Arc::new(G1Collector::new(config)),
            concurrent_mark: Mutex::new(None),
            gc_beans: Arc::new(crate::gc_metrics::BackendGcBeans::new(
                crate::gc_metrics::BackendBeanShape::G1,
            )),
        }
    }

    /// This heap's JMX bean state. gc-common w7-f; see the field.
    pub fn gc_beans(&self) -> &Arc<crate::gc_metrics::BackendGcBeans> {
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
    /// The pools' usage now, in [`crate::gc_metrics::BackendBeanShape::pools`]
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
    pub fn sample(&self) -> Vec<crate::gc_metrics::JmxPoolSample> {
        use crate::gc_metrics::JmxPoolSample;
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
                    ];
                }
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
fn g1_pool_committed_split(
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
        (committed / rs).saturating_mul(crate::g1::G1_MAX_NEW_PERCENT) / 100;
    let eden_bound = young_target_regions
        .min(committed_young_max.max(1))
        .saturating_sub(survivor_regions)
        .max(eden_regions)
        .saturating_mul(rs);
    let eden = rest.min(eden_bound);
    rest -= eden;
    (eden, survivor, old_floor.saturating_add(rest))
}

#[cfg(test)]
mod gci_i18sw_g1_pool_split_tests {
    use super::g1_pool_committed_split;

    const MB: usize = 1024 * 1024;

    /// Right after a pause: no eden region, two survivors, 10.5 MiB old in a
    /// 64 MiB committed heap with a 20-region young target. HotSpot: survivor
    /// 2 regions, old 11 regions plus the remainder, eden 18 regions.
    #[test]
    fn gci_i18sw_eden_committed_is_the_young_target_less_the_survivors() {
        let (eden, surv, old) = g1_pool_committed_split(64 * MB, MB, 0, 2, 10 * MB + MB / 2, 20);
        assert_eq!(surv, 2 * MB);
        assert_eq!(eden, 18 * MB);
        assert_eq!(old, 64 * MB - 20 * MB);
        assert_eq!(eden + surv + old, 64 * MB, "the split partitions the committed heap");
    }

    /// The young target is capped by what is left after survivor and old.
    #[test]
    fn gci_i18sw_eden_takes_at_most_what_is_left() {
        let (eden, surv, old) = g1_pool_committed_split(16 * MB, MB, 3, 1, 9 * MB, 40);
        assert_eq!(surv, MB);
        assert_eq!(old, 9 * MB);
        assert_eq!(eden, 6 * MB);
        assert_eq!(eden + surv + old, 16 * MB);
    }

    /// An eden that ran past a lowered target keeps at least its regions; a
    /// committed figure below the census never underflows.
    #[test]
    fn gci_i18sw_eden_never_below_its_regions_and_no_underflow() {
        let (eden, _, _) = g1_pool_committed_split(64 * MB, MB, 12, 2, 4 * MB, 5);
        assert_eq!(eden, 12 * MB);
        let (eden, surv, old) = g1_pool_committed_split(4 * MB, MB, 2, 2, 7 * MB, 10);
        assert_eq!((eden, surv, old), (0, 2 * MB, 7 * MB));
    }

    /// gci i19 (vi18-sw U `sw18_cu1_g1`): after a `System.gc()` that left
    /// the live data young, a 38-region target in a 16 MiB committed heap
    /// gives eden at most 60 % of the COMMITTED regions (9) less the
    /// survivors, so old keeps a positive committed size, as on HotSpot.
    #[test]
    fn gci_i19_old_keeps_the_committed_remainder_after_a_young_system_gc() {
        let (eden, surv, old) = g1_pool_committed_split(16 * MB, MB, 0, 1, 0, 38);
        assert_eq!(surv, MB);
        assert_eq!(eden, 8 * MB);
        assert_eq!(old, 7 * MB);
        assert!(old > 0);
    }
}

/// zr z3/t2: HotSpot 25 ZGC's young-pool `committed` -- the heap's commit
/// less the old pool's committed (which equals old used), never below young
/// used. With it the two ZGC pools' committed sum to `max(heap committed,
/// used)`, which is what `getHeapMemoryUsage().getCommitted()` answers after
/// its own `[used, max]` clamp (host: HotSpot `committed_equal=true`).
#[cfg_attr(not(feature = "zgc"), allow(dead_code))]
fn zgc_young_pool_committed(heap_committed: usize, young_used: usize, old: usize) -> usize {
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
    let min = crate::region::DEFAULT_REGION_SIZE;
    let rounded = crate::g1::normalize_region_size(
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
    pub zgc_explicit_large_pages: Option<crate::zgc::ZgcExplicitPages>,
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
/// `G1Collector` before starting a collection cycle, and for ZGC from
/// `VmHeap::zgc_recording_decision` (both ZGC doors). The registry-backed
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
/// Scoped verdict of a direct-token drain. A bare collector call owns the
/// veto until it returns; no thread-local veto can leak into a later cycle.
#[must_use = "hold this guard until collection finishes"]
pub struct GpuCriticalDrain {
    #[cfg(feature = "gpu-offload")]
    timed_out: bool,
}

#[cfg(feature = "gpu-offload")]
static GPU_DRAIN_VETOES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for GpuCriticalDrain {
    fn drop(&mut self) {
        #[cfg(feature = "gpu-offload")]
        if self.timed_out {
            GPU_DRAIN_VETOES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

#[cfg(feature = "gpu-offload")]
pub fn wait_for_gpu_critical_drain() -> GpuCriticalDrain {
    use std::sync::atomic::Ordering;
    let start = std::time::Instant::now();
    let budget = cratonvm_cuda_bridge::critical::collector_wait_budget();
    while GPU_CRITICAL_COUNT.load(Ordering::Acquire) != 0 {
        if start.elapsed() >= budget {
            tracing::warn!(
                "GC waited {:?} for {} direct GPU critical token(s); proceeding non-moving",
                budget, GPU_CRITICAL_COUNT.load(Ordering::Acquire),
            );
            GPU_DRAIN_VETOES.fetch_add(1, Ordering::AcqRel);
            gpu_coordination::registry().record_forced_non_moving_collection();
            return GpuCriticalDrain { timed_out: true };
        }
        std::thread::yield_now();
    }
    GpuCriticalDrain { timed_out: false }
}

#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub fn wait_for_gpu_critical_drain() -> GpuCriticalDrain {
    GpuCriticalDrain {}
}

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
        || GPU_DRAIN_VETOES.load(std::sync::atomic::Ordering::Acquire) != 0
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
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Set for the duration of a cycle that must not relocate. Under
    /// `CRATONVM_GPU_VETO_PER_CYCLE=0` (the pre-zr-z4 process flag), and under
    /// the per-cycle veto only by a cycle whose thread-local slot is gone
    /// (`forbid_relocation_without_this_threads_slot`, gci i46/ts).
    static RELOCATION_FORBIDDEN: AtomicBool = AtomicBool::new(false);

    /// zr z4/ab: cycles in progress (on any thread, of any VM) that vetoed
    /// relocation. A cycle adds itself once and removes ONLY itself at its
    /// end, so VM B's cycle end can no longer clear VM A's veto while A is
    /// still deciding whether to relocate
    /// (`docs/feature-designs/generative-gc-architecture.md` §4).
    /// Readers stay thread-agnostic: any cycle running while a veto stands
    /// stays non-moving, which is the conservative direction for a registry
    /// (one device) that is genuinely process-wide.
    static VETOING_CYCLES: AtomicUsize = AtomicUsize::new(0);

    std::thread_local! {
        /// Whether the cycle running on this (collecting) thread has counted
        /// itself in [`VETOING_CYCLES`]. The veto is decided and cleared on the
        /// collecting thread (`before_collection`, the drains inside the
        /// dispatch, `CycleGuard::after_collection`).
        static THIS_CYCLE_VETOED: Cell<bool> = const { Cell::new(false) };
    }

    /// `CRATONVM_GPU_VETO_PER_CYCLE` (zr z4/ab): scope the relocation veto
    /// to the cycle that raised it. **Default ON**; `=0` restores the process
    /// flag any cycle end cleared. Cached.
    fn veto_per_cycle() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GPU_VETO_PER_CYCLE")
        })
    }

    /// The process-wide registry.
    pub fn registry() -> &'static Arc<Registry> {
        critical::global()
    }

    /// See [`super::gpu_relocation_forbidden`].
    #[inline]
    pub fn relocation_forbidden() -> bool {
        relocation_forbidden_from(
            veto_per_cycle(),
            VETOING_CYCLES.load(Ordering::Acquire),
            RELOCATION_FORBIDDEN.load(Ordering::Acquire),
        )
    }

    /// The rule of [`relocation_forbidden`], apart. Pure.
    ///
    /// gci i46/ts (`docs/feature-designs/generative-gc-architecture.md` R2):
    /// under the per-cycle veto the process flag is read too. Nothing raises
    /// it on that arm but [`forbid_relocation_without_this_threads_slot`] (a
    /// cycle whose thread-local slot is gone), so every other answer is the
    /// counter's, as before.
    fn relocation_forbidden_from(
        per_cycle: bool,
        vetoing_cycles: usize,
        process_flag: bool,
    ) -> bool {
        if per_cycle {
            vetoing_cycles != 0 || process_flag
        } else {
            process_flag
        }
    }

    /// Veto relocation for the cycle in progress. Idempotent per cycle;
    /// withdrawn by that cycle's [`CycleGuard::after_collection`].
    pub fn forbid_relocation_this_cycle() {
        if veto_per_cycle() {
            match THIS_CYCLE_VETOED.try_with(|c| !c.replace(true)) {
                Ok(true) => {
                    VETOING_CYCLES.fetch_add(1, Ordering::AcqRel);
                    registry().record_forced_non_moving_collection();
                }
                Ok(false) => {}
                Err(_) => {
                    let _ = forbid_relocation_without_this_threads_slot();
                }
            }
            return;
        }
        if !RELOCATION_FORBIDDEN.swap(true, Ordering::AcqRel) {
            registry().record_forced_non_moving_collection();
        }
    }

    /// gci i46/ts (`i45-bh-report.md` R2): the veto of a cycle whose
    /// collecting thread's `THIS_CYCLE_VETOED` slot is gone (its
    /// thread-locals already torn down, so `try_with` fails). Before, that
    /// answered "not the first veto" and recorded NONE, so the cycle relocated
    /// although the drain had timed out -- fail-open. Now it falls back to the
    /// process flag (`RELOCATION_FORBIDDEN`, which [`relocation_forbidden`]
    /// reads on both arms and every [`CycleGuard::after_collection`] clears):
    /// fail-safe. The residual is the pre-zr-z4 one, for this case only: any
    /// cycle's end may withdraw it early. A collector does not run on an
    /// exiting thread today, and a `const` slot of a type without drop glue is
    /// never torn down on the native-TLS targets, so this is defence in depth.
    /// `CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE=0` (default ON; read here only,
    /// no static) restores the fail-open answer. `true` when the veto was
    /// raised.
    fn forbid_relocation_without_this_threads_slot() -> bool {
        if !tls_gone_veto_fails_safe() {
            return false;
        }
        if !RELOCATION_FORBIDDEN.swap(true, Ordering::AcqRel) {
            registry().record_forced_non_moving_collection();
        }
        true
    }

    /// gci i46/ts: `CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE` (default ON, `=0`
    /// kills), read per call (only on the slot-gone path; no static).
    fn tls_gone_veto_fails_safe() -> bool {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE")
    }

    /// Withdraw this thread's cycle veto, if it raised one.
    fn withdraw_this_cycles_veto() {
        let had = THIS_CYCLE_VETOED
            .try_with(|c| c.replace(false))
            .unwrap_or(false);
        if had {
            VETOING_CYCLES.fetch_sub(1, Ordering::AcqRel);
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
        // A veto this thread raised outside any guarded cycle (a drain called
        // from a bare collector entry point) belongs to no cycle: drop it here
        // so it cannot outlive the cycle it was meant for.
        withdraw_this_cycles_veto();
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
            withdraw_this_cycles_veto();
            RELOCATION_FORBIDDEN.store(false, Ordering::Release);
        }
    }

    /// gci i46/ts (`i45-bh-report.md` R2): the slot-gone veto fails safe.
    /// `cargo test -j 5 -p cratonvm-gc --features gpu-offload --lib ts46_`.
    /// No test here writes the process-wide veto state (the other
    /// `gpu_coordination_tests` read it): the reader rule is pure, and the kill
    /// arm returns before touching anything.
    #[cfg(test)]
    mod gci_i46ts_slot_gone_veto_tests {
        use super::*;

        /// A raised process flag is honoured on the per-cycle arm too; with
        /// it down, the per-cycle answer is the counter's, as before.
        #[test]
        fn ts46_the_per_cycle_reader_honours_the_slot_gone_veto() {
            assert!(relocation_forbidden_from(true, 0, true), "the fallback veto");
            assert!(!relocation_forbidden_from(true, 0, false));
            assert!(relocation_forbidden_from(true, 2, false));
            assert!(relocation_forbidden_from(false, 0, true));
            assert!(!relocation_forbidden_from(false, 3, false), "the process-flag arm");
        }

        /// `=0` restores the fail-open answer (no veto) without touching the
        /// process flag; the switch reads ON by default.
        #[test]
        fn ts46_the_kill_arm_restores_the_fail_open_answer() {
            let killed = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE", Some("0"))],
                forbid_relocation_without_this_threads_slot,
            );
            assert!(!killed);
            let on = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE", None)],
                tls_gone_veto_fails_safe,
            );
            assert!(on);
            let src = include_str!("vm_heap.rs").replace("\r\n", "\n");
            let at = src
                .find("pub fn forbid_relocation_this_cycle() {")
                .expect("forbid_relocation_this_cycle");
            let end = src[at..]
                .find("fn forbid_relocation_without_this_threads_slot() -> bool {")
                .expect("the slot-gone helper follows");
            let body = &src[at..at + end];
            assert!(body.contains("Err(_) => {"), "the slot-gone arm");
            assert!(body.contains("forbid_relocation_without_this_threads_slot()"));
            assert!(!body.contains(".unwrap_or(false)"), "the fail-open default is gone");
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

/// `CRATONVM_GEN_SATB_DIRTY_THREAD_WALK` (gcj j2/cm2, opt-in): does the
/// Generational heap's SATB queue keep a dirty-thread list for its remark /
/// cleanup walks (`SatbQueue::enable_dirty_thread_walk`)? Read once per heap,
/// by [`VmHeap::enable_concurrent_gc`].
fn gen_satb_dirty_thread_walk_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_GEN_SATB_DIRTY_THREAD_WALK")
}

/// Opt-out for the young-mirror deferral in [`VmHeap::mirror_pin_deferrable`]
/// (`CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1` restores the old old-gen-only
/// behaviour). Read once — consulted per class mirror per root scan.
#[inline]
fn mirror_pin_young_defer_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER")
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
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_NO_VALIDATE_ONCE")
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

/// A [`VmHeap::reclaimed_hole_at`] prepared for many addresses in one window in
/// which nothing allocates (gc-common w16-x). Only the Generational arm has
/// anything to capture. G1 answers `None` for free, and ZGC keeps its
/// per-address answer (see the note on the handoff's follow-ups).
pub enum ReclaimedHoleProbe {
    Generational(crate::gen_heap::YoungReclaimProbe),
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL")
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
        stw: &crate::collector::StopTheWorldToken,
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

impl VmHeap {
    /// Borrow the underlying collector behind the common [`GarbageCollector`] trait.
    #[inline]
    pub fn as_collector(&self) -> &dyn GarbageCollector {
        match self {
            VmHeap::Generational(h) => h,
            VmHeap::G1(h) => &**h,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => &**h,
        }
    }

    /// zt t29/g1 (G1 carry page item 10a): HotSpot's `-XX:+AlwaysPreTouch`
    /// on G1 ([`G1Collector::set_always_pre_touch`]: the startup commit now,
    /// every region commit as it is made) and on the generational heap
    /// ([`GenerationalHeap::pre_touch_committed`]: the startup commit now,
    /// every later commit as it is made since g1gen), per
    /// VM, before the first allocation. Answers whether the option is in
    /// force (the `gc,init` block's `Pre-touch:` fact): `false` without the
    /// option, under `CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL=0`, and on ZGC, whose
    /// heap applies it itself (`ZgcRealHeap::set_always_pre_touch`).
    pub fn set_always_pre_touch_non_zgc(&self, on: bool) -> bool {
        if !on || !gc_always_pre_touch_all_enabled() {
            return false;
        }
        match self {
            VmHeap::G1(state) => {
                let _ = state.collector.set_always_pre_touch(true);
                true
            }
            VmHeap::Generational(heap) => {
                let _ = heap.pre_touch_committed();
                true
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item
    /// 5(c)): HotSpot 25's `-XX:+UseTransparentHugePages` on G1 and Serial --
    /// the heap is advised `MADV_HUGEPAGE` (the VM decided the THP mode
    /// supports it). `false` on ZGC (its own decision,
    /// `ZgcRealHeap::advise_transparent_huge_pages`).
    pub fn advise_transparent_huge_pages_non_zgc(&self) -> bool {
        match self {
            VmHeap::G1(state) => state.collector.advise_transparent_huge_pages(),
            VmHeap::Generational(heap) => heap.advise_transparent_huge_pages(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// Create a new VmHeap with the specified backend and total capacity.
    pub fn new(backend: GcBackend, total_bytes: usize) -> Self {
        Self::new_with_overrides(backend, total_bytes, G1ConfigOverrides::default())
    }

    /// Like [`Self::new`] but applies explicit G1 tuning overrides (wired from
    /// the `-XX:` knobs: `InitiatingHeapOccupancyPercent`, `G1HeapRegionSize`,
    /// `MaxGCPauseMillis`, `±UseStringDeduplication`). Each `None` keeps the
    /// collector default; an explicit value wins over the heap-size-based
    /// region-size ergonomic. No effect on the generational backend.
    ///
    /// `-Xms` does **not** travel through here. [`Self::new_with_heap_sizing`]
    /// is the door that carries it; this one supplies `None`, which is the
    /// right answer for every caller that has only one size.
    pub fn new_with_overrides(
        backend: GcBackend,
        total_bytes: usize,
        overrides: G1ConfigOverrides,
    ) -> Self {
        Self::new_with_heap_sizing(backend, total_bytes, None, overrides).0
    }

    /// [`Self::new_with_overrides`] plus the second heap size: `initial_bytes`
    /// is `-Xms`, the bytes to COMMIT at startup, and the returned
    /// [`XmsDisposition`] says what this backend actually did with it.
    ///
    /// An `-Xms` above `-Xmx` is clamped rather than refused — the two are
    /// specified separately and a user who oversizes one should still get a VM
    /// — and a zero or absent value is [`XmsDisposition::NotSupplied`], never a
    /// commit of nothing.
    ///
    /// Callers that can tell whether the operator actually typed `-Xms` should
    /// warn on [`XmsDisposition::Dropped`]. This constructor deliberately does
    /// not warn itself: `VmConfig::initial_heap_size` carries a 16 MiB default
    /// that arrives here indistinguishable from an explicit flag, so a
    /// diagnostic raised here would fire on every run.
    pub fn new_with_heap_sizing(
        backend: GcBackend,
        total_bytes: usize,
        initial_bytes: Option<usize>,
        overrides: G1ConfigOverrides,
    ) -> (Self, XmsDisposition) {
        let initial = initial_bytes.filter(|n| *n > 0).map(|n| n.min(total_bytes));
        match backend {
            GcBackend::Generational => Self::new_generational(
                total_bytes,
                initial,
                &crate::gen_young_sizing::YoungGenSizing::default(),
                None,
            ),
            GcBackend::G1 => {
                let mut config = G1CollectorConfig::default();
                config.heap_size = total_bytes;
                config.region_size = g1_ergonomic_region_size(total_bytes);
                // Explicit -XX: overrides take precedence over the ergonomic,
                // but are held to the same shape — a region size is a power of
                // two no matter who chose it (explicit: `[1 MiB, 512 MiB]`, see
                // `explicit_region_size`). `G1Collector::new` would round a non-power-of-two
                // up anyway (see `normalize_region_size`); doing it here as
                // well is what keeps the value an operator reads back out of
                // the config equal to the one the collector is using.
                if let Some(rs) = overrides.region_size {
                    if rs > 0 {
                        // gci i11/gm finding 13: HotSpot's explicit bounds
                        // (1 MiB floor, 512 MiB ceiling), not the ergonomic ones.
                        config.region_size = explicit_region_size(rs, total_bytes);
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
                // An explicit count is honoured by `parallel_worker_count`;
                // 0 would mean "auto" there, so a
                // `-XX:ParallelGCThreads=0` is rejected by the CLI rather than
                // being silently reinterpreted.
                if let Some(n) = overrides.parallel_gc_threads {
                    if n > 0 {
                        config.gc_worker_threads = n;
                    }
                }
                // F-16: `-Xms`. Also clamped to the reservation by
                // `G1Collector::new`, so an `-Xms` above `-Xmx` yields a heap
                // rather than a refusal.
                if let Some(n) = initial {
                    config.initial_heap_size = n;
                }
                (
                    VmHeap::G1(G1State::new(config)),
                    match initial {
                        None => XmsDisposition::NotSupplied,
                        Some(n) => XmsDisposition::Committed(n),
                    },
                )
            }
            #[cfg(feature = "zgc")]
            GcBackend::Zgc => {
                let heap = ZgcRealHeap::new_shared_with_initial_and_large_pages(
                    total_bytes,
                    initial.unwrap_or(0),
                    overrides.zgc_explicit_large_pages,
                );
                // zr z7/rs: the same `[GC] zgc-footprint-spans:` line the
                // summary prints, once at construction too, so
                // `tools/rss-by-mapping.sh` knows the ranges WHILE the VM runs
                // (it counts their resident pages from `/proc/<pid>/pagemap`
                // at its snapshot, which a line printed at exit is too late
                // for). Behind `CRATONVM_GC_STATS`, like the summary.
                if cratonvm_types::flags::runtime_var_os("CRATONVM_GC_STATS").is_some() {
                    eprintln!(
                        "[GC] zgc-footprint-spans: {} at=init",
                        heap.footprint_spans().render()
                    );
                }
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
                //
                // zr z3/cs: NOT ANY MORE, by default. HotSpot's ZGC does not
                // read `-XX:MaxGCPauseMillis` (a G1 / Parallel ergonomic), so
                // there the flag leaves the collection frequency unchanged;
                // here it resized `alloc_trigger_bytes`. The CratonVM knob
                // `CRATONVM_ZGC_PAUSE_TARGET_MS` keeps the loop.
                // `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS=0` restores the door.
                if let Some(ms) = overrides.max_gc_pause_ms {
                    if !crate::zgc::zgc_ignore_max_gc_pause_millis() {
                        heap.set_pause_target_ms(ms.max(1));
                    }
                }
                // HONOURED as a PREFIX COMMIT, as of 2026-09-21.
                // `ZgcRealHeap::with_capacity_and_initial` builds its single
                // `Arena` once and NEVER grows it — the object-start bitmap and
                // `conservative_addr_span` are both gridded over the envelope
                // captured there — so `-Xms` for this collector can only ever
                // mean "commit this prefix", never "reserve less". Sizing the
                // arena from `-Xms` instead would make `-Xms512m -Xmx8g` a heap
                // that can never reach 8g, which is the "alias for `-Xmx`"
                // spelling the known-issues page above forbids.
                (
                    VmHeap::Zgc(heap),
                    match initial {
                        None => XmsDisposition::NotSupplied,
                        Some(n) => XmsDisposition::Committed(n),
                    },
                )
            }
        }
    }

    /// [`Self::new_with_heap_sizing`] plus the operator's young-generation
    /// sizing (`-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize`, `-XX:NewRatio`) —
    /// gen r4w2/alloc2, 2026-09-23.
    ///
    /// Honoured by the generational backend only
    /// ([`GenerationalHeap::with_capacity_and_young`]). G1 sizes its young
    /// generation adaptively from region counts and ZGC has none, so for those
    /// two this is exactly `new_with_heap_sizing` — the launcher, which is the
    /// layer that knows the flags were typed, prints the one-time "ignored on
    /// this collector" note. A default (`YoungGenSizing::default()`) request is
    /// exactly `new_with_heap_sizing` on every backend.
    pub fn new_with_heap_and_young_sizing(
        backend: GcBackend,
        total_bytes: usize,
        initial_bytes: Option<usize>,
        young: &crate::gen_young_sizing::YoungGenSizing,
        overrides: G1ConfigOverrides,
    ) -> (Self, XmsDisposition) {
        Self::new_with_heap_young_and_survivor_sizing(
            backend,
            total_bytes,
            initial_bytes,
            young,
            None,
            overrides,
        )
    }

    /// [`Self::new_with_heap_and_young_sizing`] plus the `-XX:SurvivorRatio`
    /// the VM will apply to a Generational heap
    /// (`GenerationalHeap::set_survivor_ratio`), which gcj yg3-residuals R3's
    /// Serial geometry needs at construction
    /// (`GenerationalHeap::with_capacity_young_and_survivor_ratio`). `None`
    /// (not typed) is [`Self::new_with_heap_and_young_sizing`] exactly; the
    /// other backends ignore it here.
    pub fn new_with_heap_young_and_survivor_sizing(
        backend: GcBackend,
        total_bytes: usize,
        initial_bytes: Option<usize>,
        young: &crate::gen_young_sizing::YoungGenSizing,
        survivor_ratio: Option<u64>,
        overrides: G1ConfigOverrides,
    ) -> (Self, XmsDisposition) {
        if (young.is_default() && survivor_ratio.is_none())
            || !matches!(backend, GcBackend::Generational)
        {
            return Self::new_with_heap_sizing(backend, total_bytes, initial_bytes, overrides);
        }
        let initial = initial_bytes.filter(|n| *n > 0).map(|n| n.min(total_bytes));
        Self::new_generational(total_bytes, initial, young, survivor_ratio)
    }

    /// The `GcBackend::Generational` arm of the heap constructors, split out
    /// (gen r4w2/alloc2) so the young sizing reaches it without touching the
    /// other two arms. `initial` is the already-clamped `-Xms`.
    fn new_generational(
        total_bytes: usize,
        initial: Option<usize>,
        young: &crate::gen_young_sizing::YoungGenSizing,
        survivor_ratio: Option<u64>,
    ) -> (Self, XmsDisposition) {
        // gci i22/ed: `CRATONVM_GEN_SERIAL_EDEN_INITIAL` (default off) starts
        // the Serial-shaped young trigger at HotSpot's eden for this initial
        // heap; a no-op when off or when there is no initial heap.
        let heap = GenerationalHeap::with_capacity_young_and_survivor_ratio(
            total_bytes,
            young,
            survivor_ratio,
        )
            .follow_initial_heap_eden(initial.unwrap_or(0), young);
        // HONOURED, as of 2026-09-21. `commit_initial_heap` commits the
        // request across the two young semi-spaces, which sit on
        // `Arena` and so on `HeapStore` — the store that reserves the
        // address space and commits it in 2 MiB granules.
        //
        // gen r4w2/oldgen2 (2026-09-23): the OLD generation reserves too now
        // (`OldGen` on `HeapStore`), and holds whatever the young pair leaves
        // of the heap (half of it by default; `-Xmx` minus the young pair
        // under `-Xmn` / `NewRatio`). `commit_initial_heap` commits whatever
        // the young pair could not absorb as an old-gen prefix — so the
        // startup commit is at least `-Xms` for every `-Xms <= -Xmx`, and
        // `Committed(n)` stays the honest answer. (Before that date the old
        // generation was a `Vec` wholly committed at construction, which
        // covered the remainder by over-committing it.)
        if let Some(n) = initial {
            heap.commit_initial_heap(n);
        }
        (
            VmHeap::Generational(heap),
            match initial {
                None => XmsDisposition::NotSupplied,
                Some(n) => XmsDisposition::Committed(n),
            },
        )
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

    /// Legacy collection-free allocation; fatal on exhaustion. Java paths
    /// must use `try_alloc_object_full` and a rooted collecting ladder.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        Self::note_alloc(dispatch!(self, alloc_object(class_id, num_fields)))
    }

    /// Legacy collection-free allocation; fatal on exhaustion or invalid length.
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
        // Round 12 wave 3 (lane iropt2, `r12w3-iropt2-array-length-cap-at-the-heap-patch`):
        // enforced below every front end; see [`VM_MAX_ARRAY_LENGTH`].
        if length > VM_MAX_ARRAY_LENGTH {
            return None;
        }
        let obj = Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_array_full(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        });
        self.g1_check_old_usage_on_growth(obj);
        obj
    }

    /// gci i14/mp finding 4 (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`):
    /// a G1 humongous array grows `G1 Old Gen` outside any collection, and
    /// HotSpot's allocator then runs
    /// `LowMemoryDetector::detect_low_memory_for_collected_pools`, so a usage
    /// threshold on the old pool fires without a collection. The G1 twin of
    /// Generational's `check_tenured_usage_on_growth` (gcf f4/r); the VM's
    /// allocation slow path (`maybe_gc`'s occupancy poll) delivers the request
    /// as it does that one's. One relaxed load unless a threshold is armed;
    /// then the region-table read lock for the humongous test and one pool
    /// sample. No lock is held here (the allocation has returned).
    fn g1_check_old_usage_on_growth(&self, obj: Option<ObjectRef>) {
        let VmHeap::G1(h) = self else {
            return;
        };
        let Some(obj) = obj else {
            return;
        };
        let thresholds = h.gc_beans().notifications().pool_thresholds();
        if !thresholds.is_armed() || !h.is_humongous(obj) {
            return;
        }
        let pools = h.gc_beans().pools(&self.backend_pool_samples());
        if let Some(old) = pools
            .iter()
            .find(|p| p.name == crate::gc_metrics::JMX_POOL_G1_OLD)
        {
            let _ = thresholds.check_usage_on_growth(old);
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
        // Round 12 wave 3 (lane iropt2, `r12w3-iropt2-array-length-cap-at-the-heap-patch`):
        // enforced below every front end; see [`VM_MAX_ARRAY_LENGTH`].
        if length > VM_MAX_ARRAY_LENGTH {
            return None;
        }
        let obj = Self::note_alloc_opt(match self {
            VmHeap::Generational(h) => h.try_alloc_array(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        });
        // gci i14/mp finding 4.
        self.g1_check_old_usage_on_growth(obj);
        obj
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
        if !self.is_object_start(obj.as_ptr() as usize) {
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
        // No counter here any more (2026-09-23: this comment used to open with
        // "TOTAL walk counter, every caller" above a body that counts nothing).
        // The per-backend census that does exist is G1's
        // `g1_object_address_census`; the JIT-helper per-site census lives in
        // `vm/src/jit/helpers.rs`. A whole-VM total would belong here, behind a
        // gate, if one is ever needed again.
        match self {
            VmHeap::Generational(h) => h.is_object_address(addr),
            VmHeap::G1(h) => h.is_object_address(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_object_address(addr),
        }
    }

    /// [`Self::is_object_address`]'s yes/no, for the header guards
    /// (KINDOF-SENTINEL) that drop the `ObjectRef` it builds: the same
    /// predicate, without the construction's two thread-local accesses (the
    /// provenance record and the single-OS-thread diagnostic), which the
    /// caller's own reference already paid when it was built. Proposal
    /// `interpreter-L7-proposal-the-header-guards-validate-without-building-an-objectref-FIXED-20261013`
    /// (interpreter round i1 wave 49, lane L7).
    ///
    /// `#[inline(always)]` (wave 50, lane L7): the fat-LTO build left this
    /// three-way dispatch a call of its own, in front of the backend's
    /// `object_start` call, on the invoke doors' receiver check and the
    /// cast-site hit; `is_object_address`, which they asked before, had its
    /// dispatch inlined into them.
    #[inline(always)]
    pub fn is_object_start(&self, addr: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.is_object_start(addr),
            VmHeap::G1(h) => h.is_object_start(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.is_object_address(addr).is_some(),
        }
    }

    /// The header of the object at `addr`, when [`Self::is_object_start`]
    /// admits it: ONE validation and one dispatch, the kind, class id and
    /// element type then read through the one reference, where
    /// `kind_of_validated` followed by `class_id_of_validated` tested the
    /// validate-once gate and dispatched over the backends once per field.
    /// For a caller holding a raw address that its context types as a
    /// reference (the interpreter's cast-site hit reads its operand without
    /// decoding a `Value`; proposal
    /// `interpreter-L7-proposal-the-cast-hit-reads-the-receiver-header-in-one-call-FIXED-20261015`,
    /// interpreter round i1 wave 51, lane L7).
    ///
    /// `None` when the validator refuses the address, and also under
    /// `CRATONVM_GC_NO_VALIDATE_ONCE=1`: the caller then takes its
    /// re-validating path, as every `_validated` twin re-validates. The
    /// generational heap's header diagnostics (`CRATONVM_DBG_STALE_OBJREF`,
    /// the blocked-access gate) run as its `get_header` runs them.
    #[inline(always)]
    pub fn header_if_object_start(&self, addr: usize) -> Option<&ObjectHeader> {
        if !validate_once_enabled() || !self.is_object_start(addr) {
            return None;
        }
        match self {
            // SAFETY: `is_object_start` admitted `addr` on this heap.
            VmHeap::Generational(h) => Some(unsafe { h.header_at_object_start(addr) }),
            // SAFETY: `is_object_start` admitted `addr`: a live header of this
            // heap, which outlives `&self`. These backends' `get_header` is the
            // same cast.
            _ => Some(unsafe { &*(addr as *const ObjectHeader) }),
        }
    }

    /// [`Self::is_object_address`] for the conservative JIT stack-band root
    /// scans, whose candidates must be object STARTS.
    ///
    /// On G1 it adds a region-end size screen
    /// (`G1Collector::is_object_address_for_root_scan`): a header-shaped
    /// interior word whose decoded object would cross its region end is
    /// rejected, because only a humongous object crosses one, and those are
    /// accepted at their base alone. The Generational heap's check is already
    /// exact, and ZGC's answers from its live-base registry, so both keep the
    /// plain answer. It accepts a subset of what `is_object_address`
    /// accepts, and drops only words that are not objects. It is kept off the
    /// read barrier's path (`load_and_forward`), which does not need the screen.
    pub fn is_object_address_for_root_scan(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::G1(h) => h.is_object_address_for_root_scan(addr),
            _ => self.is_object_address(addr),
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

    /// The collection-overlay owners the last collection found dead in its
    /// collection set, by pre-collection address, taken (cleared) by the call.
    /// G1 only — the other backends follow owner edges in a marker whose
    /// verdict the post-collection prune already reads. See
    /// `G1Collector::take_overlay_condemned_owners` for why the prune's
    /// generic liveness answer is not enough on G1.
    pub fn take_overlay_condemned_owners(&self) -> rustc_hash::FxHashSet<usize> {
        match self {
            VmHeap::G1(h) => h.take_overlay_condemned_owners(),
            _ => rustc_hash::FxHashSet::default(),
        }
    }

    /// `[lo, hi)` covering every address THIS heap has reserved for Java
    /// objects, or `None` when it has published none. Per heap -- read from
    /// the collector itself, not from the process-global
    /// [`crate::heap_geometry`] table, whose slots a heap another VM built a
    /// moment later may have overwritten.
    ///
    /// Built for [`crate::compressed_oops::admit_heap`], which fit-checks a
    /// joining VM against the process's narrow-oop window (gc-common w8-f), and
    /// that use needs the RESERVATION, not the committed prefix: the window
    /// must cover every address the heap can ever produce. Today each arm's
    /// [`Self::conservative_addr_span`] is exactly that, and this delegates to
    /// it: Generational's three arena `[base, base + capacity)` bounds (the
    /// same values it publishes into `heap_geometry`; a young arena that later
    /// moves is caught by `compressed_oops::assert_region_encodable` on
    /// republish), G1's `arena_base .. arena_base + reserved_len()`, ZGC's
    /// never-grown arena. If an arm's filter span is ever tightened to the
    /// committed prefix, this must stop delegating to it.
    pub fn reserved_address_envelope(&self) -> Option<(usize, usize)> {
        self.conservative_addr_span()
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

    /// DIAGNOSTIC-ONLY (cce0079): collection epoch for the `[SETFIELD-GC]`
    /// assertion ("did a GC complete inside this store?") and for test
    /// messages that count collections.
    ///
    /// Generational: its minor-GC count. G1 and ZGC: their whole collection
    /// count (every G1 pause and every ZGC cycle can move objects, which is
    /// the only thing the epoch is asked about). Until 2026-09-23 those two
    /// arms were a `_ => 0` catch-all, so the `[SETFIELD-GC]` probe could never
    /// fire on the default collector and every "minor GCs so far: 0" in a
    /// failing test's message was a constant, not a measurement.
    pub fn debug_minor_gc_count(&self) -> u64 {
        match self {
            VmHeap::Generational(h) => h.debug_minor_gc_count(),
            VmHeap::G1(_) => self.collection_count(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => self.collection_count(),
        }
    }

    /// The epoch the TLAB share sizer closes its sampling windows on
    /// (`Tlab::share_refill_request`): one tick per completed YOUNG pause,
    /// HotSpot's sample point for `TLABAllocationWeight`. gen r5w4/defaults8
    /// names the policy use of what [`Self::debug_minor_gc_count`] already
    /// returns (gen r5w2/alloc6 moved the sizer onto it), so a later change to
    /// the diagnostic cannot silently re-scale the sizer.
    ///
    /// Re-checked with the Generational tail sink on (its default since gen
    /// r5w2): `minor_gc_count` moves exactly once per completed young pause —
    /// at the end of the moving path, and inside the non-moving sweep for a
    /// diverted pause (`sweep_young_non_moving`, which the divert returns
    /// through) — including a pause that also collected the old generation
    /// (`major_gc_count` is separate, which is why `collection_count` was the
    /// wrong epoch there). A refused cycle moves neither. The tail sink
    /// retracts cursors and free-lists tails; it runs no collection and moves
    /// no counter, and the sizer's numerator
    /// (`Tlab::thread_allocated_bytes`) counts consumed bytes, not returned
    /// tails, so the sink leaves the sample exact. G1 and ZGC: their
    /// collection count, as `debug_minor_gc_count`.
    #[inline]
    pub fn young_pause_epoch(&self) -> u64 {
        self.debug_minor_gc_count()
    }

    /// Is the HotSpot-shaped TLAB share sizer (`CRATONVM_TLAB_SHARE_SIZER`)
    /// on for THIS heap? gen r5w4/defaults8: its default is per backend, and
    /// an explicit setting wins on all three
    /// (`GcFlags::tlab_share_sizer_for_backend`). Generational ON since gcg
    /// g6/jn, G1 ON since gci i48/zl3 (`alloc_policy_defaults::TLAB_SHARE_SIZER_G1`,
    /// which says why a G1 carve cannot overlap live memory), ZGC OFF. The first
    /// Generational flip
    /// was reverted at the r5w4 merge (`alloc_policy_defaults::TLAB_SHARE_SIZER`; see
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// — gen r5w5/sizer9 corrected this line, which still said "ON on the
    /// Generational heap"). Per VM: the
    /// answer is the flag snapshot every other arm reads plus this heap's own
    /// variant, no process state. Read once per TLAB refill (never per
    /// allocation) by `tlab_alloc_shaped_inner`, which sizes the request and
    /// arms the new buffer's refill-waste limit with the same answer.
    #[inline]
    pub fn tlab_share_sizer_enabled(&self) -> bool {
        crate::gc_flags().tlab_share_sizer_for_backend(self.is_generational(), self.is_g1())
    }

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

    /// Loose validity check: alignment + heap-region containment.
    ///
    /// Unlike [`Self::is_object_address`] this does NOT read the object
    /// header. Used by operand-stack root scanning to root ambiguous
    /// JVM-long-vs-jobject slots without rejecting legitimate references
    /// whose header is transiently unreadable (interior pointers, mid-
    /// initialisation slots, or stale-bit-pattern Long slots that the GC
    /// is well-equipped to ignore via its own size-sanity guard).
    //
    // Doc placement, 2026-09-20: this block had slid onto
    // `resolve_interior_for_pin` above, which meant the method that is
    // DEFINED as "alignment + containment" carried no contract at all while
    // its more-permissive sibling carried two. The three ZGC-arm comments in
    // the body below are the load-bearing part and are unchanged.
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
    /// Every `VmHeap` backend lays objects out with the 16-byte
    /// `cratonvm_types::ObjectHeader`; a forwarded object carries its target in
    /// the mark word (`MARK_FORWARDED`), read through
    /// `ObjectHeader::forwarding_address`. No backend uses
    /// `crate::compact_header::CompactHeader`. (This paragraph described a
    /// 32-byte header with a separate `forwarding_ptr` field until 2026-09-23;
    /// that layout has not existed since 2026-08-07.)
    //
    // The two doc blocks above describe the public `load_and_forward` family;
    // the one below is this private helper's own.
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
    //
    // DOC PLACEMENT, 2026-09-20. The two blocks above had slid onto
    // the `stale_barrier_site_is_new` helper: it was inserted between them
    // and their function, so that private rate limiter carried the Brooks-barrier
    // contract and this function carried none. The `#[inline]` that used to sit
    // BETWEEN the two doc blocks went with them and landed on a
    // `#[cold] #[inline(never)]` helper, where the two attributes contradict;
    // it is dropped rather than moved, because this function already has one.
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
                        "load_and_forward was handed a reference the collector MOVED, and cannot repair it: this collector leaves no forwarding word at the vacated address. The caller in the backtrace is holding a stale ObjectRef that nothing else will fix.",
                    );
                }
            }
        }
        if !pre_validated && !self.is_object_start(obj.as_ptr() as usize) {
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
        if !self.is_object_start(addr as usize) {
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

    /// zt t18/zb1 (zr1br A2a): is this a ZGC heap whose read barrier is armed?
    /// `false` on every other backend and on every default run. For a raw
    /// reader outside `gc/` that holds a `&VmHeap` and must decline to a
    /// barriered accessor while armed (`jit_getfield`'s legacy-cell arm).
    #[inline]
    pub fn zgc_load_barrier_armed(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            return h.load_barrier_armed();
        }
        false
    }

    /// zt t19/zb2 (zr1br A2b): is this a ZGC heap armed with COLORED stores
    /// (`ZgcRealHeap::arm_load_barrier_colored_stores`, tests only)? A raw
    /// reference writer outside `gc/` that holds a `&VmHeap` delegates its
    /// store to the heap accessor while this holds (the JIT's `jit_aastore` /
    /// `jit_putfield_object`), because only the accessor encodes the word.
    /// `false` on every other backend and on every production heap.
    #[inline]
    pub fn zgc_load_barrier_colored_stores(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            return h.load_barrier_colored_stores();
        }
        false
    }

    /// A reference compare-and-swap on one slot of an ARMED ZGC heap, as
    /// heal-then-CAS (nio21 wave 9 L3; `ZgcRealHeap::cas_ref_slot_healed` has
    /// the loop and its assumptions). `expected` / `new` are machine addresses,
    /// `0` for null. `Some((witness_address, swapped))`; `None` -- the caller
    /// keeps its own path -- on every other backend, on an unarmed ZGC heap
    /// (whose slots hold plain pointers, so a plain CAS is right), under
    /// compressed oops (a coloured word does not fit a 4-byte slot), and for an
    /// unencodable `new`.
    ///
    /// # Safety
    ///
    /// As [`Self::load_ref_slot_barriered`]: `slot` is a live, 8-aligned
    /// reference slot inside a live object, valid for writes.
    #[inline]
    pub unsafe fn cas_ref_slot_barriered(
        &self,
        slot: *mut u8,
        expected: u64,
        new: u64,
    ) -> Option<(u64, bool)> {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            if cratonvm_types::narrow_oop::narrow_oops_enabled() || slot as usize % 8 != 0 {
                return None;
            }
            return h
                .cas_ref_slot_healed(slot as usize, expected as usize, new as usize)
                .map(|(w, ok)| (w as u64, ok));
        }
        let _ = (slot, expected, new);
        None
    }

    /// The raw reference read of declared field `slot` of a COMPACT instance,
    /// as the machine address the field holds (`0` for null), or `None`: a
    /// legacy (16-byte cell) body, an array, a slot the current compact layout
    /// does not have, a non-reference row, or a slot whose width / alignment is
    /// not the heap's reference width.
    ///
    /// JIT round 23 wave 1, lane L4 (`jit-r21-nio-proposals-w17-L2.md` item 2):
    /// the one copy of "compact displacement + barriered reference load" that
    /// the VM-side Rust helpers reading a known JDK field share, instead of each
    /// growing its own (`tl_load_ref_field`, `jit_getfield`'s compact arm,
    /// `field_fast`'s `load_ref`). The load goes through
    /// [`Self::load_ref_slot_barriered`] (plain `read_ref_slot` on Generational,
    /// G1 and an unarmed ZGC heap; the healing barrier while ZGC is armed; the
    /// compressed-oop width either way). Unlike `get_field` it does NOT unbox an
    /// `AUTOBOX_CLASS_ID` wrapper: a caller that may meet one screens the word
    /// itself.
    ///
    /// # Safety
    ///
    /// `obj` is a live, already-forwarded object of this heap that cannot move
    /// before the caller stops using the answer (no safepoint in between); on
    /// an armed ZGC heap the slot is also written (the heal), which every
    /// reference slot of a live object allows.
    #[inline]
    pub unsafe fn load_ref_field_compact(&self, obj: ObjectRef, slot: usize) -> Option<u64> {
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let domain = match self {
            VmHeap::Generational(h) => h.layout_domain(),
            VmHeap::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.layout_domain(),
        };
        // Domain-screened: a class id is per-`ClassStore`, so the blind form
        // could answer another VM's layout for the same number.
        let (disp, storage) = cratonvm_types::field_layout::compact_field_disp_storage_in(
            domain,
            header.class_id.as_u32(),
            slot,
        )?;
        let width = cratonvm_types::narrow_oop::ref_field_size();
        if !storage.is_reference() || storage.size_runtime() as usize != width {
            return None;
        }
        let addr = (obj.as_ptr() as usize).checked_add(disp)?;
        if addr % width != 0 {
            return None;
        }
        // SAFETY: `disp` is the compact displacement of reference field
        // `slot` in the layout of `obj`'s class, so `addr` is a naturally
        // aligned reference slot of the heap's width inside the live body.
        Some(unsafe { self.load_ref_slot_barriered(addr as *const u8) })
    }

    /// Does this heap's layout domain own `class_id`'s registered compact
    /// layout? The domain screen of [`Self::load_ref_field_compact`], asked
    /// once per object by the readers that then take the whole layout from the
    /// current-layout cache, which is not screened. `false` for a class with no
    /// fields, which those readers have no use for anyway.
    fn owns_compact_layout(&self, class_id: ClassId) -> bool {
        let domain = match self {
            VmHeap::Generational(h) => h.layout_domain(),
            VmHeap::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.layout_domain(),
        };
        cratonvm_types::field_layout::compact_field_disp_storage_in(domain, class_id.as_u32(), 0)
            .is_some()
    }

    /// The reference fields of a compact instance, from field `from_slot` on,
    /// pushed onto `out` in slot order as the addresses [`Self::get_field`]
    /// hands back as references: nulls are left out, and a word the slot's
    /// wrapper screen unboxes into a primitive is left out too, exactly as
    /// `get_field` would have answered a primitive there.
    ///
    /// One layout lookup for the object, and only the oop-map's slots are read,
    /// through [`Self::load_ref_slot_barriered`]. The G1 remark's reference
    /// tracer walks millions of objects (a dead `ZipFileSystem`'s entry index)
    /// and paid a full `get_field` -- validation, layout lookup, coercion -- for
    /// every field, primitives included
    /// (`docs/internal/fixed-suite-bugs/spring/in-process-javac-throughput-aot-test-classes-RETIRED-20261027.md`).
    ///
    /// `false`, with nothing pushed: not a compact instance, a reference width
    /// other than the heap's, more than one layout domain in the process (the
    /// current-layout cache is not domain-screened), or a field-access
    /// diagnostic armed. The caller then reads slot by slot.
    ///
    /// # Safety
    ///
    /// As [`Self::load_ref_field_compact`]: `obj` is a live, already-forwarded
    /// object of this heap that cannot move before the caller is done with the
    /// answer.
    pub unsafe fn push_compact_reference_words(
        &self,
        obj: ObjectRef,
        from_slot: usize,
        out: &mut Vec<usize>,
    ) -> bool {
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return false;
        }
        let header = self.get_header(obj);
        if usize::BITS < 64
            || header.kind() != ObjectKind::Object
            || !cratonvm_types::is_compact_object(header)
            || !self.owns_compact_layout(header.class_id)
        {
            return false;
        }
        let class_id = header.class_id;
        let width = cratonvm_types::narrow_oop::ref_field_size();
        let start = out.len();
        // `(slot << 32) | displacement` of each reference field, then each
        // entry replaced by the word it holds: no second buffer.
        let planned = cratonvm_types::with_current_class_layout(class_id.as_u32(), |layout| {
            for slot in from_slot..layout.field_count() {
                let storage = layout.field_storage(slot)?;
                if !storage.is_reference() {
                    continue;
                }
                let disp = layout.field_disp(slot)? as usize;
                if storage.size_runtime() as usize != width || disp % width != 0 {
                    return None;
                }
                out.push((slot << 32) | disp);
            }
            Some(())
        })
        .flatten()
        .is_some();
        if !planned {
            out.truncate(start);
            return false;
        }
        let mut kept = start;
        for i in start..out.len() {
            let (slot, disp) = (out[i] >> 32, out[i] & 0xffff_ffff);
            // SAFETY: `disp` is the compact displacement of reference field
            // `slot` of `obj`'s class, at the heap's reference width and
            // alignment, inside the live body.
            let word = unsafe { self.load_ref_slot_barriered(obj.as_ptr().add(disp) as *const u8) }
                as usize;
            if word == 0 {
                continue;
            }
            // SAFETY: only the tag is read unless the screen validates it.
            let value = Value::Object(Some(unsafe { ObjectRef::from_raw(word as *mut u8) }));
            let value = crate::autobox::unbox_reference_field(
                value,
                class_id,
                slot,
                |r| {
                    let r = self.is_object_address(r.as_ptr() as usize)?;
                    Some(self.class_id_of(r))
                },
                |r| self.get_field(r, 0),
            );
            if let Value::Object(Some(r)) = value {
                out[kept] = r.as_ptr() as usize;
                kept += 1;
            }
        }
        out.truncate(kept);
        true
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
                 the one production arming (arm_load_barrier_plain_stores) refuses compressed \
                 oops, so reaching here means one of those two facts changed without this \
                 function being revisited. Refusing \
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
        // zt t19/zb2: the JIT-seam engagement count (census only).
        h.note_load_barrier_jit_slot();
        // Delegates the color test, the slow-path heal, the null
        // disambiguation (P3) and the OFFSET -> ADDRESS conversion. `None` is
        // null, which this signature spells `0`, matching `read_ref_slot`.
        h.load_barrier_slot(slot as usize).map_or(0, |a| a as u64)
    }

    // `get_compact_header` was deleted 2026-09-23 (gc-common w2-e, lane G's
    // handoff `handoff-g-vm-heap-stale-header-docs.md`): it had no caller, and
    // on every backend it would have decoded the 16-byte header's
    // `class_id`/`shape` word as a `CompactHeader`.

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
        if !self.is_object_start(obj.as_ptr() as usize) {
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
        if !self.is_object_start(obj.as_ptr() as usize) {
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
        if !self.is_object_start(obj.as_ptr() as usize) {
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

    /// Both halves of the vacated-address instrument, fired on one accessor's
    /// receiver. `site` is what separates "who read through it" from "who wrote
    /// through it" in the report, so every call site passes a distinct string.
    ///
    /// A stale receiver makes every field read off it return whatever now
    /// occupies that memory — a perfectly VALID object of the wrong class,
    /// which is why the value being pushed looks clean and the `checkcast` one
    /// instruction later does not. That is the consumption point neither the
    /// stack-push nor the frame-local detector can see.
    ///
    /// # The write side (gen r4w2/obs, 2026-09-23)
    ///
    /// Until round 4 wave 2 only `get_field` called this. A write through a
    /// stale receiver is strictly worse than a read through one: it corrupts
    /// the live, unrelated object that now occupies the address, whose owner
    /// has no idea a field was overwritten, and the damage surfaces arbitrarily
    /// later in a third place with no backtrace — so `set_field`,
    /// `set_field_volatile`, `set_field_as`, `set_field_volatile_as`,
    /// `set_array_element` and `get_array_element` now call it too, each with
    /// its own site string.
    ///
    /// What held it back for three rounds was the default-path price on
    /// **every reference store in the VM**: the two probes each did their own
    /// `vacated_frames_enabled()` load and branch. The gate is now tested ONCE
    /// here, inline, and both probes live behind it in a `#[cold]`
    /// out-of-line call — so a store pays one relaxed load of a read-mostly
    /// `AtomicU8` and a never-taken branch, and `get_field` pays half what it
    /// paid before. The A/B the page asks for (`bench/HashMapOnly.java`, flag
    /// OFF, interleaved) is in the round-4 wave-2 obs review's probe list; if
    /// it shows a cost, drop the four `set_*` sites and keep the cold ones.
    /// See `docs/feature-designs/generative-gc-architecture.md`.
    #[inline]
    fn check_receiver(&self, obj: ObjectRef, site: &'static str, receiver_site: &'static str) {
        if crate::gc_quiescence::vacated_frames_enabled() {
            Self::check_receiver_armed(obj.as_ptr() as usize, site, receiver_site);
        }
    }

    /// The armed half of [`Self::check_receiver`]; kept out of line so the
    /// accessors inline only the gate.
    #[cold]
    #[inline(never)]
    fn check_receiver_armed(addr: usize, site: &'static str, receiver_site: &'static str) {
        // `CRATONVM_DBG_VACATED_FRAMES`: is the RECEIVER of this access an
        // address the collector moved an object away from?
        crate::gc_quiescence::report_vacated_receiver(addr, site);
        // The re-issue-proof half of the same question — see
        // `gc_quiescence::stale_use_verdict` for why the exact ledger above
        // cannot answer it.
        crate::gc_quiescence::check_stale_use(addr, receiver_site);
    }

    pub fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.check_receiver(obj, "get_field", "get_field receiver");
        dispatch!(self, get_field(obj, index))
    }

    /// zt t23/st1 (item 29a): a reference-field read that never keeps its
    /// target alive -- ZGC's `ZBarrierKind::Weak` load
    /// (`ZgcRealHeap::get_field_weak`), for the VM's reference round reading a
    /// referent slot to decide its fate. Identical to [`Self::get_field`]
    /// whenever ZGC's read barrier is unarmed and on every other backend
    /// (their `get_field` marks nothing).
    pub fn get_field_weak(&self, obj: ObjectRef, index: usize) -> Value {
        self.check_receiver(obj, "get_field", "get_field receiver");
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.get_field_weak(obj, index),
            other => dispatch!(other, get_field(obj, index)),
        }
    }

    /// zt t32 (2f, `CRATONVM_ZGC_LOAD_BARRIER_MARKING`): is load-barrier
    /// marking armed? Then the store side logs only a PLAIN word's deletion
    /// ([`Self::satb_barrier_slot`]); the ZGC accessors apply the rule
    /// themselves. `false` off ZGC.
    #[inline]
    pub fn zgc_load_barrier_marking(&self) -> bool {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.load_barrier_marking(),
            _ => false,
        }
    }

    /// zt t32 (2f): the SATB pre-barrier of a store about to overwrite the
    /// 8-byte reference word at `slot`, whose decoded value is `old`. Under
    /// load-barrier marking the word is read raw and logged only when plain
    /// (`ZgcRealHeap::lbm_pre_barrier_slot`); otherwise exactly
    /// [`Self::satb_barrier`]`(old)`.
    #[inline]
    pub fn satb_barrier_slot(&self, slot: *const u8, old: Value) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            if h.load_barrier_marking() {
                if h.mark_active() {
                    h.lbm_pre_barrier_slot(slot);
                }
                return;
            }
        }
        let _ = slot;
        self.satb_barrier(old);
    }

    /// zt t32: [`Self::get_field_weak`] for a reference-array element
    /// (`ZgcRealHeap::get_array_element_weak`); the plain element read on
    /// every other backend.
    pub fn get_array_element_weak(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.get_array_element_weak(obj, index),
            other => dispatch!(other, get_array_element(obj, index)),
        }
    }

    pub fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.check_receiver(obj, "set_field", "set_field receiver");
        // Task #25: this path fires the inherent write_barrier inside
        // `set_field`, which doesn't go through `VmHeap::write_barrier`
        // and would leave the triad sentinel armed. Clear it here so
        // the (pre, store-via-set_field) sequence closes cleanly.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field(obj, index, value))
    }

    /// Fallible twin of the descriptor-less [`set_field`](Self::set_field),
    /// for a store that may BOX a primitive into a compact reference slot
    /// (`autobox::box_for_reference_slot`).
    ///
    /// Returns `true` when the store happened exactly as `set_field` would
    /// have made it. Returns `false` when the value needed an
    /// `AUTOBOX_CLASS_ID` wrapper and the heap could not hold one without a
    /// collection: the slot then holds null and no wrapper exists. The caller
    /// must treat the object as unusable (the class-mirror populator drops the
    /// unpublished mirror and declines).
    ///
    /// gen r5w1/oom5, `gengc-r4w6-review6-class-mirror-creation-aborts-on-a-full-heap-outside-ldc-FIXED-20260927.md`
    /// item 2: the mirror's slot-0 class-id store used the backend's panicking
    /// `alloc_object(AUTOBOX_CLASS_ID, 1)` and aborted on a full heap even on
    /// the fallible mirror path. A store that does not box (a reference, or a
    /// legacy slot) is plain `set_field` and allocates nothing either way.
    pub fn try_set_field(&self, obj: ObjectRef, index: usize, value: Value) -> bool {
        if !crate::autobox::needs_reference_box(value) {
            self.set_field(obj, index, value);
            return true;
        }
        // SAFETY: `self` outlives `scope`: both live in this frame, and the
        // scope is dropped before this function returns.
        let scope = unsafe {
            crate::autobox::arm_fallible_wrapper(
                Self::try_alloc_wrapper_erased,
                (self as *const Self).cast::<()>(),
            )
        };
        self.set_field(obj, index, value);
        let stored = !scope.declined();
        drop(scope);
        stored
    }

    /// The [`crate::autobox::TryAllocWrapper`] of [`Self::try_set_field`]:
    /// the backend's non-aborting object entry, then the wrapper's own slot 0
    /// (a legacy cell: `AUTOBOX_CLASS_ID` never has a compact layout, so this
    /// store cannot box again).
    fn try_alloc_wrapper_erased(heap: *const (), value: Value) -> Option<ObjectRef> {
        // SAFETY: armed only by `try_set_field` with `self`, which outlives
        // the scope this runs inside.
        let heap = unsafe { &*heap.cast::<VmHeap>() };
        let wrapper = heap.try_alloc_object_full(cratonvm_types::AUTOBOX_CLASS_ID, 1)?;
        heap.set_field(wrapper, 0, value);
        Some(wrapper)
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
        self.check_receiver(obj, "set_field_volatile", "set_field_volatile receiver");
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
        self.check_receiver(obj, "set_field_as", "set_field_as receiver");
        // Like `set_field`, descriptor-aware stores use the collector's
        // inherent barrier path. Close the debug SATB triad sentinel here;
        // otherwise an interpreter/JIT pre-barrier followed by putfield via
        // this typed entry point leaves it armed until the next store.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field_as(obj, index, value, desc_byte))
    }

    /// Read a small compact object's typed fields after one complete layout
    /// validation. None leaves out untouched for the scalar diagnostic path.
    /// Reference slots retain the collectors' shared wrapper-unboxing policy.
    pub fn try_get_compact_fields(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        out: &mut [Value],
    ) -> Option<ObjectRef> {
        const MAX_BATCH: usize = 16;
        let count = slots.len().min(out.len());
        if !(2..=MAX_BATCH).contains(&count) {
            return None;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return None;
        }
        #[cfg(feature = "zgc")]
        if let Self::Zgc(heap) = self {
            // zt t18/zb1 (zr1br A2a): the batch reads reference slots raw;
            // armed, the per-slot path (`get_field`, which barriers) serves.
            if !heap.compact_batch_reads_enabled() || heap.load_barrier_armed() {
                return None;
            }
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let class_id = header.class_id;
        let mut plan = [(0usize, cratonvm_types::FieldStorageKind::Boolean); MAX_BATCH];
        // Compact objects are never allocated for mutable-layout classes
        // (`field_layout::is_compact_object`), so the current layout is the
        // exact layout this object was allocated against; consulting it
        // directly avoids `header.num_slots()`'s redundant layout lookup.
        let ready = cratonvm_types::with_current_class_layout(
            class_id.as_u32(),
            |layout| {
                // Small objects have contiguous backing on every collector,
                // including G1. Humongous/sparse layouts keep their scalar doors.
                if layout.total_size > 1024 {
                    return false;
                }
                for (i, (slot, _)) in slots[..count].iter().enumerate() {
                    let Some(off) = layout.field_disp(*slot) else {
                        return false;
                    };
                    let Some(storage) = layout.field_storage(*slot) else {
                        return false;
                    };
                    plan[i] = (off as usize, storage);
                }
                true
            },
        )
        .unwrap_or(false);
        if !ready {
            return None;
        }
        // Drop the layout borrow before unboxing can consult another layout.
        // Each slot is an independent relaxed atomic read, as on get_field_as;
        // a batch is not a multi-field snapshot or a volatile access.
        for (i, ((index, desc), dst)) in slots[..count].iter().zip(&mut out[..count]).enumerate() {
            let (off, storage) = plan[i];
            // SAFETY: the entire plan was validated against this live compact
            // receiver's layout. No safepoint or Java allocation intervenes.
            let raw = unsafe {
                cratonvm_types::read_compact_field(
                    obj.as_ptr().add(off),
                    storage,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
            let raw = if storage.is_reference() {
                crate::autobox::unbox_reference_field(
                    raw,
                    class_id,
                    *index,
                    |r| {
                        let r = self.is_object_address(r.as_ptr() as usize)?;
                        Some(self.class_id_of(r))
                    },
                    |r| self.get_field(r, 0),
                )
            } else {
                raw
            };
            *dst = crate::heap::coerce_field_value_for_slot(
                raw,
                *desc,
                crate::heap::FieldCoercionSite::read(Some(class_id), *index),
            );
        }
        Some(obj)
    }

    /// Try one layout resolution for a batch of compact primitive stores.
    /// None performs no writes, so the caller can retain the normal per-slot
    /// path for references, legacy layouts, invalid slots and diagnostics.
    pub fn try_set_compact_primitive_fields(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        values: &[Value],
    ) -> Option<ObjectRef> {
        const MAX_BATCH: usize = 16;
        let count = slots.len().min(values.len());
        if !(2..=MAX_BATCH).contains(&count)
            || slots[..count].iter().any(|(_, desc)| {
                !matches!(desc, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
            })
            || values[..count]
                .iter()
                .any(|value| matches!(value, Value::Object(_)))
        {
            return None;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return None;
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let class_id = header.class_id;
        let mut plan = [(0usize, cratonvm_types::FieldStorageKind::Boolean); MAX_BATCH];
        let ready = cratonvm_types::with_current_class_layout(
            class_id.as_u32(),
            |layout| {
                // Keep G1's fragmented humongous backing on its scalar path.
                if layout.total_size > 1024 {
                    return false;
                }
                for (i, (slot, _)) in slots[..count].iter().enumerate() {
                    let Some(off) = layout.field_disp(*slot) else {
                        return false;
                    };
                    let Some(storage) = layout.field_storage(*slot) else {
                        return false;
                    };
                    // A primitive hint is not enough: the actual declared storage
                    // must be primitive too, or boxing and reference barriers apply.
                    if storage.is_reference() {
                        return false;
                    }
                    plan[i] = (off as usize, storage);
                }
                true
            },
        )
        .unwrap_or(false);
        if !ready {
            return None;
        }
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        // Release the layout-cache borrow before diagnostics/store helpers can
        // consult metadata themselves. No Java call or collection intervenes.
        for (i, ((index, desc), value)) in slots[..count].iter().zip(&values[..count]).enumerate() {
            let (off, storage) = plan[i];
            let value = crate::heap::coerce_field_value_for_slot(
                *value,
                *desc,
                crate::heap::FieldCoercionSite::store(Some(class_id), *index),
            );
            // SAFETY: the complete plan resolved each slot of this live compact
            // receiver, proved it primitive, and retained its natural alignment.
            // The coercion and stores cannot reach a safepoint or allocate Java.
            unsafe {
                dispatch!(
                    self,
                    set_resolved_compact_primitive_field(obj, *index, off, storage, value)
                );
            }
        }
        Some(obj)
    }

    /// The per-call precondition of [`Self::copy_compact_object_fields`]: no
    /// field-access diagnostic armed and, on ZGC, compact batch reads allowed
    /// (the primitive reads are raw; the punned-cell watch reads per slot).
    pub fn compact_object_copy_ready(&self) -> bool {
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return false;
        }
        #[cfg(feature = "zgc")]
        if let Self::Zgc(heap) = self {
            return heap.compact_batch_reads_enabled();
        }
        true
    }

    /// The per-class precondition of [`Self::copy_compact_object_fields`]:
    /// this heap owns `class_id`'s compact layout (a class with no fields has
    /// none) and one plan covers it ([`COMPACT_COPY_MAX_SLOTS`] fields,
    /// [`COMPACT_COPY_MAX_BYTES`] bytes, every slot resolved). `false` means
    /// the copy refuses EVERY instance of the class, so a caller can decline
    /// before allocating the copy. Fixed for the life of the class.
    pub fn compact_object_copy_admits_class(&self, class_id: ClassId) -> bool {
        let mut plan: CompactCopyPlan =
            [(0usize, cratonvm_types::FieldStorageKind::Boolean); COMPACT_COPY_MAX_SLOTS];
        self.compact_copy_plan(class_id, &mut plan).is_some()
    }

    /// The copy plan of `class_id` into `plan`: `(field count, body bytes,
    /// every reference slot at the heap's width and alignment)`, or `None`
    /// when one plan cannot cover the class. The one decision both
    /// [`Self::copy_compact_object_fields`] and
    /// [`Self::compact_object_copy_admits_class`] read.
    fn compact_copy_plan(
        &self,
        class_id: ClassId,
        plan: &mut CompactCopyPlan,
    ) -> Option<(usize, usize, bool)> {
        // The current-layout cache below is not domain-screened.
        if !self.owns_compact_layout(class_id) {
            return None;
        }
        let mut total_size = 0usize;
        // Every reference slot at the heap's reference width and alignment:
        // the precondition of the copy's barriered slot read.
        let mut words_readable = true;
        let width = cratonvm_types::narrow_oop::ref_field_size();
        // As in `try_get_compact_fields`: a compact object's class has a fixed
        // layout, so the current one is the one its objects were allocated by.
        let count = cratonvm_types::with_current_class_layout(class_id.as_u32(), |layout| {
            let count = layout.field_count();
            if layout.total_size as usize > COMPACT_COPY_MAX_BYTES || count > COMPACT_COPY_MAX_SLOTS
            {
                return None;
            }
            total_size = layout.total_size as usize;
            for (slot, step) in plan[..count].iter_mut().enumerate() {
                *step = (
                    layout.field_disp(slot)? as usize,
                    layout.field_storage(slot)?,
                );
                if step.1.is_reference()
                    && (step.1.size_runtime() as usize != width || step.0 % width != 0)
                {
                    words_readable = false;
                }
            }
            Some(count)
        })
        .flatten()?;
        Some((count, total_size, words_readable))
    }

    /// `Object.clone()`'s field copy: every instance field of `src` into
    /// `dst`, a freshly allocated object of the same class, verbatim.
    ///
    /// One validation of each object and one layout resolution serve the whole
    /// copy. Primitive slots are copied raw through the batch store door
    /// (`set_resolved_compact_primitive_field`, the one
    /// [`Self::try_set_compact_primitive_fields`] uses); every reference slot
    /// still goes through [`Self::get_field`] / [`Self::set_field`], so each
    /// backend's read and write barriers apply to it exactly as on the
    /// slot-by-slot copy. `dst` must be fresh: nothing it held before is
    /// published to a concurrent marker.
    ///
    /// The slot-by-slot copy validated and resolved the layout for every read
    /// and every write: a 14-field object cloned in ~4.5 us against HotSpot's
    /// ~60 ns, and `TimeZone.getDefault()` clones one per call
    /// (`docs/internal/fixed-suite-bugs/spring/in-process-javac-throughput-aot-test-classes-RETIRED-20261027.md`).
    ///
    /// `None` writes nothing: either object is not a live compact instance of
    /// one class, the object is large (G1's humongous backing keeps its scalar
    /// door) or has more slots than one plan holds, or a field-access
    /// diagnostic is armed. The caller then copies slot by slot.
    ///
    /// j27 wave 4 (lane CLONE2): the two refusals that do not depend on the
    /// objects are asked by name, so a caller that must allocate `dst` first
    /// can ask them BEFORE it allocates: [`Self::compact_object_copy_ready`]
    /// (per call) and [`Self::compact_object_copy_admits_class`] (per class).
    pub fn copy_compact_object_fields(&self, src: ObjectRef, dst: ObjectRef) -> Option<()> {
        if !self.compact_object_copy_ready() {
            return None;
        }
        let (src, src_valid) = self.load_and_forward_checked(src);
        let (dst, dst_valid) = self.load_and_forward_checked(dst);
        if !src_valid || !dst_valid {
            return None;
        }
        let (src_header, dst_header) = (self.get_header(src), self.get_header(dst));
        if src_header.kind() != ObjectKind::Object
            || dst_header.kind() != ObjectKind::Object
            || !cratonvm_types::is_compact_object(src_header)
            || !cratonvm_types::is_compact_object(dst_header)
            || src_header.class_id != dst_header.class_id
        {
            return None;
        }
        let mut plan: CompactCopyPlan =
            [(0usize, cratonvm_types::FieldStorageKind::Boolean); COMPACT_COPY_MAX_SLOTS];
        let (count, total_size, words_readable) =
            self.compact_copy_plan(src_header.class_id, &mut plan)?;
        // A reference slot's word is copied as it stands (the barriered read of
        // the source slot) and stored through the collector's batch store when
        // it admits one: a verbatim copy, as `Object.clone` specifies, without
        // two validated field doors per slot. Otherwise (a SATB window open)
        // through `get_field` / `set_field`.
        let batch = words_readable && self.compact_reference_batch_admitted(total_size);
        let mut stored_reference = false;
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        // Drop the layout borrow before a reference door consults metadata.
        // No safepoint or Java allocation intervenes from here on.
        for (index, &(off, storage)) in plan[..count].iter().enumerate() {
            if storage.is_reference() && batch {
                // SAFETY: the plan resolved this reference slot of both live
                // compact objects, at the heap's width and alignment, and the
                // collector admitted the batch.
                unsafe {
                    let word = self.load_ref_slot_barriered(src.as_ptr().add(off)) as usize;
                    let value =
                        Value::Object((word != 0).then(|| ObjectRef::from_raw(word as *mut u8)));
                    self.set_resolved_compact_reference(dst, index, off, storage, value);
                }
                stored_reference = true;
                continue;
            }
            if storage.is_reference() {
                let value = self.get_field(src, index);
                self.set_field(dst, index, value);
                continue;
            }
            // SAFETY: the plan resolved this primitive slot against both live
            // compact objects' shared layout, at its natural alignment.
            unsafe {
                let value = cratonvm_types::read_compact_field(
                    src.as_ptr().add(off),
                    storage,
                    std::sync::atomic::Ordering::Relaxed,
                );
                dispatch!(
                    self,
                    set_resolved_compact_primitive_field(dst, index, off, storage, value)
                );
            }
        }
        self.note_compact_reference_batch(dst, stored_reference);
        Some(())
    }

    /// [`Self::try_set_compact_primitive_fields`] for a batch that also stores
    /// REFERENCES (the stack-walker residual-costs page, item 2): one
    /// forwarding read, one header read and one layout plan for the batch,
    /// then each slot's store as the collector's own accessor makes it in the
    /// state the collector admitted: ZGC while its compact reference store is
    /// a plain write (`ZgcRealHeap::compact_reference_batch_allowed`; one
    /// card for the batch), G1 while no marking window is open and the object
    /// cannot be humongous (the flat write and its remembered-set
    /// post-barrier), the generational heap while no SATB window is open (its
    /// resolved compact store, watches and card included). `None` performs no
    /// writes. Reference values must already be forwarded (the caller's
    /// boundary forward). A reference slot given a primitive (boxed by the
    /// accessors) or a primitive slot given an object declines.
    pub fn try_set_compact_fields(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        values: &[Value],
    ) -> Option<ObjectRef> {
        const MAX_BATCH: usize = 16;
        let count = slots.len().min(values.len());
        if !(2..=MAX_BATCH).contains(&count) {
            return None;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return None;
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let class_id = header.class_id;
        let mut plan = [(0usize, cratonvm_types::FieldStorageKind::Boolean); MAX_BATCH];
        let mut total_size = 0usize;
        let ready = cratonvm_types::with_current_class_layout(
            class_id.as_u32(),
            |layout| {
                if layout.total_size > 1024 {
                    return false;
                }
                total_size = layout.total_size as usize;
                for (i, ((slot, _), value)) in slots[..count].iter().zip(&values[..count]).enumerate()
                {
                    let Some(off) = layout.field_disp(*slot) else {
                        return false;
                    };
                    let Some(storage) = layout.field_storage(*slot) else {
                        return false;
                    };
                    if storage.is_reference() != matches!(value, Value::Object(_)) {
                        return false;
                    }
                    plan[i] = (off as usize, storage);
                }
                true
            },
        )
        .unwrap_or(false);
        if !ready {
            return None;
        }
        if !self.compact_reference_batch_admitted(total_size) {
            return None;
        }
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        let mut stored_reference = false;
        for (i, ((index, desc), value)) in slots[..count].iter().zip(&values[..count]).enumerate() {
            let (off, storage) = plan[i];
            let value = crate::heap::coerce_field_value_for_slot(
                *value,
                *desc,
                crate::heap::FieldCoercionSite::store(Some(class_id), *index),
            );
            if !(storage.is_reference() && matches!(value, Value::Object(_))) {
                // SAFETY: the complete plan resolved this primitive slot of the
                // live compact receiver. No safepoint or Java allocation.
                unsafe {
                    dispatch!(
                        self,
                        set_resolved_compact_primitive_field(obj, *index, off, storage, value)
                    );
                }
                continue;
            }
            stored_reference = true;
            // SAFETY: the plan resolved this reference slot of the live
            // compact receiver, and the collector admitted the batch.
            unsafe { self.set_resolved_compact_reference(obj, *index, off, storage, value) };
        }
        self.note_compact_reference_batch(obj, stored_reference);
        Some(obj)
    }

    /// May a batch store REFERENCES into a compact object of `total_size`
    /// bytes through [`Self::set_resolved_compact_reference`]? Each
    /// collector's own terms: no SATB window open (and, on G1, an object that
    /// cannot be humongous).
    fn compact_reference_batch_admitted(&self, total_size: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.compact_reference_batch_allowed(),
            VmHeap::G1(h) => h.compact_reference_batch_allowed(total_size),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.compact_reference_batch_allowed(),
        }
    }

    /// One reference store of an admitted batch
    /// ([`Self::compact_reference_batch_admitted`]): the flat write and the
    /// collector's per-store barrier (Generational's card, G1's remembered
    /// set). ZGC cards the object once after the batch
    /// ([`Self::note_compact_reference_batch`]).
    ///
    /// # Safety
    /// `off`/`storage` must describe `obj`'s in-bounds reference slot `index`,
    /// and the batch must be admitted.
    unsafe fn set_resolved_compact_reference(
        &self,
        obj: ObjectRef,
        index: usize,
        off: usize,
        storage: cratonvm_types::FieldStorageKind,
        value: Value,
    ) {
        match self {
            VmHeap::Generational(h) => {
                h.set_resolved_compact_reference_field(obj, index, off, storage, value)
            }
            // SAFETY: the caller's contract.
            VmHeap::G1(h) => unsafe {
                h.set_resolved_compact_reference_field(obj, off, storage, value)
            },
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => unsafe {
                h.set_resolved_compact_reference_field_no_card(obj, off, storage, value)
            },
        }
    }

    /// ZGC's `set_field_no_satb` card, once for a batch that stored a
    /// reference into `obj`, after the stores (the other two collectors
    /// barrier per store).
    fn note_compact_reference_batch(&self, obj: ObjectRef, stored_reference: bool) {
        #[cfg(feature = "zgc")]
        if let (VmHeap::Zgc(h), true) = (self, stored_reference) {
            h.note_ref_store(obj.as_ptr() as usize);
        }
        let _ = (obj, stored_reference);
    }

    /// Volatile descriptor-aware set.
    pub fn set_field_volatile_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        self.check_receiver(
            obj,
            "set_field_volatile_as",
            "set_field_volatile_as receiver",
        );
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
    /// Every backend waits on [`wait_for_gpu_critical_drain`] before
    /// collecting — `GenerationalHeap` and `G1Collector` at their own entry,
    /// ZGC at this dispatcher (`zgc_recording_decision`, since 2026-09-23;
    /// before that the default collector did not wait at all) — so a kernel
    /// running under this token will not observe its inputs being moved.
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
        self.check_receiver(obj, "get_array_element", "get_array_element receiver");
        dispatch!(self, get_array_element(obj, index))
    }

    pub fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.check_receiver(obj, "set_array_element", "set_array_element receiver");
        // Task #25: closes the triad sentinel (same rationale as
        // `set_field`); ref-array stores also route through the
        // inherent write_barrier.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_array_element(obj, index, value))
    }

    // `pin_critical_region` / `unpin_critical_regions` -- the G1 region / ZGC
    // page pin `GetPrimitiveArrayCritical` used to take for its section -- were
    // deleted in gc-common w3-b (`handoff-w2c-retire-jni-pin-machinery.md`
    // item 3). Since w2-c the critical section finds its array again through a
    // remappable per-VM JNI global ref (`CriticalCopy::array_gref`), nothing
    // needs the array kept in place, and neither function had a caller. The
    // cuda-bridge critical path keeps its own pins (`cuda-bridge/src/critical.rs`),
    // and `ZgcRealHeap::pin_critical` / `G1Collector::pin_region_for_addr` stay
    // for their collectors' own callers and tests.

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
    /// `ZgcRealHeap::report_fatal_exhaustion` then `std::process::abort()`
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
            // SBR5 lane ncall: every native call asks this, and `needs_gc` is
            // ~13 ns of atomic loads (6% of a 220 ns crossing). The relief it
            // buys is not latency-critical -- a native allocation loop crosses
            // the threshold and collects a few native calls later -- so the
            // occupancy predicate is sampled on every eighth call of the
            // thread. The latch (`native_alloc_pressure`) stays exact.
            VmHeap::Zgc(h) => {
                if h.native_alloc_pressure() {
                    return true;
                }
                thread_local! {
                    static TICK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
                }
                let tick = TICK.with(|c| {
                    let v = c.get().wrapping_add(1);
                    c.set(v);
                    v
                });
                tick & 7 == 0 && h.needs_gc()
            }
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
    /// cursor never retreats under the non-moving sweep).
    ///
    /// # Why the other two arms are spelled out
    ///
    /// They answer `allocated_bytes()`, which is the same answer the `_ =>`
    /// catch-all gave — but written as a catch-all it was indistinguishable
    /// from the `bytes_promoted_total` catch-all two functions down, which was
    /// answering a constant for a backend that had already grown the counter it
    /// denied
    /// (`docs/feature-designs/zgc-architecture.md`).
    /// Here the delegation is real on both arms and the reason differs per
    /// backend, so each says its own.
    pub fn live_bytes_estimate(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.live_bytes_estimate(),
            // G1 reclaims a whole region at a time and `allocated_bytes` is
            // summed over the live regions' occupancy, so it already retreats
            // when a collection frees anything — there is no non-moving sweep
            // leaving a bump cursor high, which is the effect the generational
            // arm's estimate exists to correct for.
            VmHeap::G1(_) => self.allocated_bytes(),
            // ZGC's sweep rebuilds the arena free list and retracts the bump
            // cursor into its free tail, so `allocated_bytes` falls with the
            // live set here too.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => self.allocated_bytes(),
        }
    }

    /// Total bytes promoted young→old across all collections (selective
    /// promotion + the moving collector's tenuring). Used by the
    /// GC-overhead productivity metric: a promotion-only cycle conserves
    /// live bytes but did useful allocation-enabling work.
    ///
    /// # Why this is not a catch-all any more
    ///
    /// It was `_ => 0` until 2026-09-20, and that single arm was doing two
    /// completely different jobs: it swallowed a counter one backend already
    /// had, and it denied a counter another backend did not have yet. Both read
    /// as "this backend does not promote", which is false on all three.
    ///
    /// That is the hazard a catch-all carries in this dispatcher generally: it
    /// keeps answering a constant long after a backend grows the thing it
    /// denies, and nothing goes red. Spelling the arms out means the next
    /// backend that grows a promotion counter gets a non-exhaustive-match
    /// compile error here instead of a silent zero.
    ///
    /// # All three arms now delegate
    ///
    /// *2026-09-21.* G1 counts bytes copied into Old regions at the winning arm
    /// of both evacuators (`G1Collector::note_promoted_bytes`), and ZGC counts
    /// bytes at the sweep site that ages an object across the promotion
    /// threshold (`ZCounters::gen_promoted_bytes`). Neither is derived from an
    /// occupancy delta: on G1 an Old-region count moves for reasons that are not
    /// promotion (humongous allocation, cleanup reclaim, mixed-CSet evacuation
    /// of Old into Old), so a derived figure would be wrong in both directions
    /// and — unlike a hard zero — would be trusted.
    ///
    /// A backend that cannot promote still answers `0`, and now says so from
    /// its own counter: outside `CRATONVM_ZGC_GENERATIONAL` there is no old
    /// generation for ZGC to promote into, so its counter never moves and the
    /// zero is the truth rather than a missing consumer.
    pub fn bytes_promoted_total(&self) -> u64 {
        match self {
            VmHeap::Generational(h) => h.stats().snapshot().bytes_promoted,
            VmHeap::G1(h) => h.collector.bytes_promoted_total(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.bytes_promoted_total(),
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
    /// question. Spelled out per backend rather than left to a catch-all, for
    /// the reason [`Self::live_bytes_estimate`] states.
    pub fn old_gen_headroom(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.old_gen_capacity().saturating_sub(h.old_gen_used()),
            // G1's Old regions are drawn from the same free-region pool as
            // Eden and Survivor, so "what can old still absorb" is "what is
            // left of that pool" -- less the to-space reserve the mutator
            // never gets (gcd d10/o retirement: it was `capacity - allocated`,
            // which read 12 % at G1's `OutOfMemoryError`, so the overhead
            // limit's 2 % wedge test could never pass on G1). See
            // `G1Collector::old_absorbable_bytes`.
            VmHeap::G1(h) => h.old_absorbable_bytes(),
            // ZGC's old generation is a header label rather than a space (see
            // `zgc::generation`), so a promoted object occupies the same arena
            // it always did and whole-heap headroom is the whole question.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => self.heap_capacity().saturating_sub(self.allocated_bytes()),
        }
    }

    /// The crawl backstop's old-space budget, including growth the
    /// Generational allocator can still take after a refused request. Other
    /// backends already draw their old allocations from the whole-heap pool.
    pub fn old_gen_headroom_with_growth(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.old_gen_headroom_with_growth(),
            VmHeap::G1(_) => self.old_gen_headroom(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => self.old_gen_headroom(),
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
        // Retire every published FFM fast-path verdict — see the identical
        // call in [`Self::collect_garbage_with_finalizers`] for why a verdict
        // keyed by a carrier's ADDRESS cannot survive a collection.
        //
        // That comment claimed the bump sat "at the one dispatcher every
        // collector goes through", and it did not: THIS is a second public
        // door into the same three collectors, and it had no bump. Production
        // happens to reach the collectors only through the finalizer-aware
        // twin today (every in-tree caller of this entry point is a test or a
        // bench), so the gap is latent rather than live — which is exactly the
        // kind of gap the claim was written to prevent, and the reason to close
        // it here rather than to restate the claim. One relaxed increment per
        // cycle, never per access.
        cratonvm_types::ffm_epoch::bump_ffm_epoch();
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
        // JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish` — see
        // [`Self::collect_with_finalizers_dispatch`] for why they are fired
        // HERE and not by the collectors (Generational excepted: it fires its
        // own, see `jvmti_gc_hooks_fired_here`).
        let fire = self.jvmti_gc_hooks_fired_here();
        if fire {
            crate::gc::fire_gc_start();
        }
        let result = match self {
            VmHeap::Generational(h) => h.collect_garbage(stw, roots, monitors),
            VmHeap::G1(h) => h.collect_garbage(stw, roots, monitors),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                Self::zgc_recording_decision(h, || h.collect_garbage(stw, roots, monitors))
            }
        };
        if fire {
            crate::gc::fire_gc_finish();
        }
        result
    }

    /// Run one ZGC collection and record the collector decision it took —
    /// the ZGC arm of `gc_metrics::record_collector_decision`, which until
    /// 2026-09-23 had no ZGC caller at all, so on the DEFAULT backend
    /// `collector_decision_report()` had nothing to say and
    /// `decision_reason::NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY` was defined,
    /// labelled, tested and never recorded
    /// (`docs/feature-designs/zgc-architecture.md`).
    ///
    /// # Why the dispatcher can answer this after all
    ///
    /// That page declined "option 2" (record from here) on the ground that the
    /// dispatch point "does not know whether the slide ran", so it could only
    /// say "a ZGC collection happened". It does know, from ZGC's own
    /// monotonic, per-heap engagement counters, read before and after the one
    /// call and compared: `compaction_cycles` moves iff this cycle's slide ran,
    /// and `relocation_coverage_reasons[i]` moves iff this cycle's refusal was
    /// the coverage proof failing on obligation `i`. Both are bumped inside the
    /// collection, the world is stopped, and only one collection runs at a
    /// time, so a delta is exactly this cycle's verdict — not a re-derivation
    /// of it.
    ///
    /// The codes, in precedence order:
    ///
    /// * the slide ran → [`crate::gc_metrics::decision_reason::MOVING_BACKEND_COMPACTED`];
    /// * a GPU device vetoed relocation this cycle →
    ///   `NON_MOVING_GPU_CRITICAL` (already documented as covering "the ZGC
    ///   slide");
    /// * the per-cycle coverage proof refused →
    ///   `NON_MOVING_COVERAGE_INCOMPLETE` with the obligation that failed;
    /// * anything else (the cost gate, `CRATONVM_ZGC_RELOCATE=0`, a
    ///   generational young cycle, nothing worth sliding) →
    ///   `NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY`.
    ///
    /// Cost: two small counter snapshots and one relaxed histogram bump per
    /// COLLECTION, never per allocation.
    #[cfg(feature = "zgc")]
    fn zgc_recording_decision<R>(h: &ZgcRealHeap, collect: impl FnOnce() -> R) -> R {
        use crate::gc_metrics::decision_reason;
        use crate::gc_quiescence::incomplete_reason;
        // The direct-token GPU drain, which `GenerationalHeap` and `G1Collector`
        // run at their own collection entry and `ZgcRealHeap` never did
        // (`docs/feature-designs/zgc-architecture.md`):
        // a caller holding a `VmHeap::enter_gpu_critical` token was protected
        // from relocation on two backends and not on the default one. Here,
        // before the counter snapshots, so a drain that times out and forbids
        // relocation (`gpu_coordination::forbid_relocation_this_cycle`) is
        // recorded below as the GPU veto it is. Both ZGC doors come through
        // this function; a no-op without `gpu-offload`.
        let _gpu_drain = wait_for_gpu_critical_drain();
        let compactions_before = h.feature_engagement().1;
        let coverage_before = h.relocation_coverage_reason_counts();
        let result = collect();
        // Read BEFORE the caller's `CycleGuard::after_collection` clears it.
        let gpu_forbidden = gpu_relocation_forbidden();
        let compacted = h.feature_engagement().1 > compactions_before;
        let coverage_after = h.relocation_coverage_reason_counts();
        let coverage_refusal = coverage_after
            .iter()
            .zip(coverage_before.iter())
            .position(|(after, before)| after > before);
        let (reason, unproven) = if compacted {
            (
                decision_reason::MOVING_BACKEND_COMPACTED,
                incomplete_reason::NONE,
            )
        } else if gpu_forbidden {
            (
                decision_reason::NON_MOVING_GPU_CRITICAL,
                incomplete_reason::NONE,
            )
        } else if let Some(obligation) = coverage_refusal {
            (decision_reason::NON_MOVING_COVERAGE_INCOMPLETE, obligation)
        } else {
            (
                decision_reason::NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY,
                incomplete_reason::NONE,
            )
        };
        crate::gc_metrics::record_collector_decision("zgc", reason, unproven);
        result
    }

    /// PRE-collection addresses of every object the last
    /// [`Self::collect_garbage_with_finalizers`] marked ONLY in its
    /// finalizer-resurrection drain (evacuated, forwarded, or set the mark bit
    /// of) and that its strong closure had NOT marked -- the dead finalizable
    /// objects themselves included. Drained: a second call answers empty.
    ///
    /// The VM notes this set with the reference processor before its round
    /// (`note_resurrected_finalizables_for_reference_processing` in
    /// `vm/src/runtime/interpreter/gc_and_alloc.rs`), so a weak or soft
    /// reference to an object reachable only THROUGH a finalizable one is
    /// cleared before `finalize()` runs, as HotSpot does
    /// (`docs/feature-designs/profile-guided-inlining.md`,
    /// depth 2).
    ///
    /// gc-common w18-a: the dispatcher half of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    /// An EMPTY answer is exactly the depth-1 behaviour (the VM still derives
    /// the dead finalizables themselves from `dead_finalizers`). A collector
    /// owner fills in its arm with a `take_resurrection_closure` of its own,
    /// recorded where its drain marks, reset at the start of every collection,
    /// and holding only objects the strong closure had not marked -- that last
    /// condition is the soundness one: a strongly reachable object in this set
    /// would lose its weak references.
    ///
    /// Generational: filled in (gc-common w36-d,
    /// `GenerationalHeap::take_resurrection_closure`) by the non-moving young
    /// sweep's resurrection drain, the moving Phase 2.5b and the old
    /// generation's finalizer pass. G1: Phase 3.5's fresh evacuations
    /// (`G1Collector::take_resurrection_closure`). ZGC: every object the
    /// resurrection pass's `mark_claim` wins (`ZgcRealHeap::take_resurrection_closure`).
    pub fn take_resurrection_closure(&self) -> Vec<usize> {
        match self {
            VmHeap::Generational(h) => h.take_resurrection_closure(),
            VmHeap::G1(h) => h.take_resurrection_closure(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.take_resurrection_closure(),
        }
    }

    /// gci i17/fz (i16/fr's residual, `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`):
    /// take extra resurrection candidates that are not finalizable (the VM's
    /// stop-the-world probe referents, POST-collection addresses) back out of
    /// what the collection just published to the concurrent remark
    /// (`GenerationalHeap::retract_concurrent_finalizer_candidates`). Called
    /// inside the same pause. G1 and ZGC publish no such list: no-op.
    pub fn retract_concurrent_finalizer_candidates(&self, post_addrs: &[usize]) {
        match self {
            VmHeap::Generational(h) => h.retract_concurrent_finalizer_candidates(post_addrs),
            VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
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
        // `cratonvm_types::ffm_epoch`. Bumped at the VmHeap entry point rather
        // than inside a collector, so a future collector cannot silently miss
        // it — on BOTH public entry points, which is a correction: this comment
        // said "the one dispatcher" while `collect_garbage` was a second one
        // with no bump at all.
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
        // JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish`.
        //
        // The VM installs these two hooks at boot (`vm_init.rs`,
        // `install_gc_start_hook` / `install_gc_finish_hook`) on the stated
        // premise that "the GC driver (gc::collect / collect_with_finalizers)
        // calls these unconditionally at entry / exit". Until 2026-09-23 the
        // ONLY callers of `fire_gc_start` / `fire_gc_finish` were those two
        // functions in `gc.rs` — the Cheney collector of the vestigial
        // semi-space `Heap`, which no `GcBackend` selects. So on Generational,
        // G1 and ZGC alike a JVMTI agent that enabled either event received
        // NOTHING, for every collection of every run. Fired here, at the one
        // dispatcher both public entry points go through, so no backend can
        // opt out by omission. Zero-cost without an agent: one `Acquire` load
        // of `GC_HOOKS_ACTIVE` each (the VM installs the adapters on every run,
        // and the adapter itself returns at `is_event_enabled`).
        //
        // Pause-scoped, like HotSpot's: this runs on the collecting thread
        // with the world stopped, and a JVMTI callback for these two events is
        // restricted by the spec to raw-monitor operations for that reason.
        //
        // Generational fires its own pair inside
        // `GenerationalHeap::collect_garbage{,_with_finalizers}` (gen
        // r4/plumbing, landed on dev the same day), so it is skipped here —
        // firing at both levels delivered every event TWICE to an agent.
        let fire = self.jvmti_gc_hooks_fired_here();
        if fire {
            crate::gc::fire_gc_start();
        }
        let result = match self {
            VmHeap::Generational(h) => {
                h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
            }
            VmHeap::G1(h) => {
                // zt pl4 item 3: the collecting thread's share of a G1 pause
                // is worker work (HotSpot's G1 runs it on its parallel GC
                // workers): `gc_parallel_workers`, not the VM thread.
                let cpu = crate::thread_cpu::CpuSpan::begin();
                let result =
                    h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors);
                let ns = cpu.elapsed_ns();
                h.collector.note_driver_cpu_ns(ns);
                crate::thread_cpu::note_pause_work_cpu(ns);
                result
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                // zt pl4 item 3: HotSpot's ZGC collects on its own worker
                // threads (no published group): the collecting thread's
                // share is not the VM thread's.
                let cpu = crate::thread_cpu::CpuSpan::begin();
                let result = Self::zgc_recording_decision(h, || {
                    h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
                });
                crate::thread_cpu::note_pause_work_cpu(cpu.elapsed_ns());
                result
            }
        };
        if fire {
            crate::gc::fire_gc_finish();
        }
        result
    }

    /// Whether the dispatcher fires the JVMTI `GarbageCollectionStart` /
    /// `GarbageCollectionFinish` pair for this backend. False for
    /// Generational, whose collector fires its own at its two public entry
    /// points; true for G1 and ZGC, which fire none.
    #[inline]
    fn jvmti_gc_hooks_fired_here(&self) -> bool {
        !matches!(self, VmHeap::Generational(_))
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

    /// The heap's current COMMITTED size — HotSpot's `CollectedHeap::capacity()`,
    /// i.e. `Runtime.totalMemory()`, the JMX heap `MemoryUsage.getCommitted()`,
    /// the `(NNNM)` of an `-Xlog:gc` line and a JFR `GCHeapSummary`'s
    /// `committed`. `freeMemory()` is this minus [`Self::allocated_bytes`].
    ///
    /// # What changed on 2026-09-23, and why
    ///
    /// Until then the Generational and ZGC arms returned their RESERVATION —
    /// both young semi-spaces plus the old generation at full capacity, and
    /// ZGC's whole arena envelope — so `totalMemory()` was `-Xmx` on the
    /// default collector from the first instruction:
    /// `tools/probes/XmsProbe.java -Xms64m -Xmx512m` printed `total=512m`
    /// where HotSpot prints `total=64m` (orchestrator baseline battery,
    /// `docs/feature-designs/generative-gc-architecture.md`).
    /// An application that sizes a cache from `totalMemory()`/`freeMemory()`
    /// saw a heap eight times larger than the one it had. Every arm is now what
    /// the heap has brought into use:
    ///
    /// * Generational — the granules ONE young semi-space has committed (the
    ///   larger of the two arenas' commits; `Arena::committed_bytes`, so
    ///   `-Xms` moves it), plus the old generation's committed granules
    ///   (`OldGen::committed_bytes`; it reserves and commits on demand since
    ///   gen r4w2/oldgen2). On the wholly committed fallback, where the old
    ///   generation is committed whole, its TOUCHED prefix
    ///   (`OldGen::high_water`) is counted instead — counting the whole store
    ///   there made the old generation alone report half of `-Xmx`.
    ///   [`Self::os_committed_bytes`] counts every arena's committed bytes as
    ///   they are: that is the OS-accounting question. gen r5w2/obs6: the
    ///   second semi-space (the copy reserve) is no longer counted — HotSpot
    ///   Serial's `capacity()` leaves its to-space out too — see
    ///   `GenerationalHeap::usable_committed_parts`.
    /// * G1 — the committed prefix of its reservation (`committed_len`),
    ///   unchanged. (Until 2026-09-30 this read `-Xmx` after the first
    ///   parallel young pause, truthfully: that pause committed the whole
    ///   reservation — `docs/feature-designs/concurrent-gc-maturation.md`.)
    /// * ZGC — the arena's committed granules
    ///   ([`ZgcRealHeap::os_committed_bytes`]). Uncommit respects the startup
    ///   committed-byte floor and the per-VM ZUncommit/ZUncommitDelay policy.
    ///   See `docs/feature-designs/zgc-architecture.md`.
    ///
    /// # It must not track OCCUPANCY
    ///
    /// HotSpot's `totalMemory()` moves when the heap grows or shrinks, and
    /// callers rely on that: H2's `Utils.collectGarbage()` is written as "gc
    /// until `totalMemory()` stops changing". Every arm is a count of committed
    /// granules or regions, which an allocation inside already-committed space
    /// does not move; a collection moves it only by giving memory back, and two
    /// back-to-back collections with nothing allocated in between give back
    /// the same memory, so that loop still converges (in one extra `gc()` on
    /// ZGC). `committed_bytes_does_not_track_occupancy` pins the first half.
    ///
    /// Takes the arena lock on ZGC and the young/old locks on Generational (the
    /// same locks [`Self::allocated_bytes`] already takes on Generational). A
    /// reporting call, never an allocation-path one; do not call it while
    /// holding a collector lock.
    pub fn committed_bytes(&self) -> usize {
        match self {
            // gen r5w2/obs6 (2026-09-26): HotSpot Serial's `capacity()` — ONE
            // semi-space's committed granules plus the old generation's
            // committed size (`GenerationalHeap::usable_committed_parts`, which
            // the three JMX pools sum to). Until then this arm counted BOTH
            // semi-spaces' commits, so the empty copy reserve — never
            // allocatable while mutators run — was reported as usable heap
            // (`gengc-r4w3-hunter-runtime-memory-counts-the-copy-reserve`).
            // The old-generation term is unchanged (its derivation moved to
            // `gen_heap::reported_old_committed`).
            VmHeap::Generational(h) => h.usable_committed_bytes(),
            VmHeap::G1(h) => h.committed_bytes(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.os_committed_bytes(),
        }
    }

    /// The heap's MAXIMUM usable size — HotSpot's `CollectedHeap::max_capacity()`,
    /// i.e. `Runtime.maxMemory()` and the heap `MemoryUsage.getMax()` — where
    /// the backend knows it to differ from `-Xmx`; `None` means "report the
    /// configured `-Xmx`" (the VM's `max_heap_bytes`).
    ///
    /// gen r5w2/obs6 (2026-09-26): Generational answers
    /// `GenerationalHeap::max_usable_heap_bytes` — `-Xmx` less the copy
    /// reserve (one semi-space at its ceiling), `3/4 · -Xmx` at the default
    /// split, as HotSpot Serial reports `-Xmx` less one survivor space. A heap
    /// built without an `-Xmx` budget answers `None`. G1 and ZGC answer
    /// `None` (unchanged: their whole reservation is allocatable, and HotSpot
    /// reports `-Xmx` for both).
    pub fn max_usable_bytes(&self) -> Option<usize> {
        match self {
            VmHeap::Generational(h) => h.max_usable_heap_bytes(),
            VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// Bytes this heap's stores have actually asked the OS to BACK.
    ///
    /// [`Self::committed_bytes`] answers `Runtime.totalMemory()`. Since
    /// 2026-09-23 the two agree on G1 and ZGC; on Generational this one counts
    /// the old generation's whole `Vec` (reserved and, on Windows, charged at
    /// construction) where `committed_bytes` counts only its touched prefix,
    /// and (gen r5w2/obs6) both semi-spaces' commits where `committed_bytes`
    /// counts one.
    /// (Before that date `committed_bytes` was a sum of capacities on two
    /// backends and this was the only committed measurement.)
    ///
    /// # Why it exists
    ///
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// recorded that the three backends' documented `-Xmx`/`-Xms` semantics and
    /// their actual resident behaviour had drifted apart, and that **nothing in
    /// the tree measured which of the two a running process matched**. A flag
    /// whose only effect is on committed bytes cannot be tested — or trusted —
    /// without a number that moves when it works, and `committed_bytes` was not
    /// that number on two of three backends until 2026-09-23.
    ///
    /// Takes a lock on the Generational and ZGC arms. A diagnostic and a test
    /// instrument, never an allocation-path call.
    pub fn os_committed_bytes(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.os_committed_bytes(),
            // Already the committed prefix rather than the reservation — G1 is
            // the backend whose `committed_bytes` never had the gap.
            VmHeap::G1(h) => h.committed_bytes(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.os_committed_bytes(),
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

    /// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): compiled
    /// reference-store sites this heap's JIT emitted with no pre-store card
    /// check. Always `0` on G1 and ZGC, which publish no generational card
    /// view (so no such site exists for them).
    pub fn jit_post_only_card_sites(&self) -> u64 {
        match self {
            VmHeap::Generational(heap) => heap.jit_post_only_card_sites(),
            VmHeap::G1(_) => 0,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => 0,
        }
    }

    /// gcj j2/jt2: the BUG-03 take-over's completion of the post-only card
    /// mark for one frozen peer's register words
    /// (`GenerationalHeap::takeover_complete_post_only_card_marks`). A no-op
    /// returning `0` on G1 and ZGC, and on Generational while no post-only
    /// site was compiled.
    pub fn takeover_complete_post_only_card_marks(&self, words: &[usize]) -> usize {
        match self {
            VmHeap::Generational(heap) => heap.takeover_complete_post_only_card_marks(words),
            VmHeap::G1(_) => 0,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => 0,
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
            // gci i2/wr: two relaxed loads, not the 19-load snapshot (every
            // address-memo lookup on Generational lands here).
            VmHeap::Generational(h) => h.stats().collection_count(),
            VmHeap::G1(h) => h.collection_count(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.gc_count() as u64,
        }
    }

    /// The epoch for ADDRESS-keyed memos: it moves whenever an address may come
    /// to name a different object. Not a statistic: only equality across two
    /// reads means anything (MXBeans keep [`Self::collection_count`]).
    ///
    /// gci i1/wr and gch h4/g1 X4, lane gcI wave 9 (the same fix of
    /// `r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`, step 2). G1
    /// answers [`G1Collector::address_epoch`], which also moves at `cleanup`'s
    /// in-place reclaim of whole Old regions and dead humongous spans (it books
    /// no collection); every other backend answers [`Self::collection_count`],
    /// as the memos read before. The Generational concurrent sweep, which frees
    /// old blocks between pauses without booking a collection, is not covered
    /// here (the heap does not own the cycle state): the VM's
    /// `HeapRealm::address_epoch` adds `ConcurrentGcState::sweep_slices_begun`
    /// (gci i2/wr, page
    /// `gci-i1wr-generational-concurrent-sweep-does-not-move-the-address-memo-epoch-FIXED-20261012`).
    /// Address-keyed memos read that, not this.
    pub fn address_epoch(&self) -> u64 {
        match self {
            VmHeap::G1(h) => h.address_epoch(),
            _ => self.collection_count(),
        }
    }

    /// Accumulated collection pause time in milliseconds, for
    /// `GarbageCollectorMXBean.getCollectionTime()`, where the backend keeps
    /// one; `None` where it does not (ZGC). gc-common w5-f,
    /// `docs/feature-designs/zgc-architecture.md`.
    ///
    /// Every `Some` figure is the whole milliseconds of the backend's
    /// MICROSECOND pause sum, converted once, as HotSpot's
    /// `GCMemoryManager::_accumulated_timer` is ([`pause_sum_as_collection_time_ms`]).
    /// It does NOT move on every sub-millisecond collection; H2's
    /// `Utils.collectGarbage()` needs it to move only eventually (see
    /// `gc_metrics::serial_collectors`). gce e1/o,
    /// `docs/feature-designs/zgc-architecture.md`:
    /// G1 and this fallback added one millisecond per collection until then.
    ///
    /// * Generational — `GenerationalHeap::gc_pause_totals().1`. Only the
    ///   single-bean fallback reads it; the Serial beans this backend always
    ///   describes floor their own sums (`gc_metrics::serial_collectors`);
    /// * G1 — the collector's microsecond accumulator (`total_pause_us`, bumped
    ///   together with `collection_count` for every recorded pause).
    pub fn collection_time_ms(&self) -> Option<u64> {
        match self {
            VmHeap::Generational(h) => Some(pause_sum_as_collection_time_ms(h.gc_pause_totals().1)),
            VmHeap::G1(h) => Some(pause_sum_as_collection_time_ms(h.total_pause_us())),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// The JMX bean state of a backend whose beans the common GC event
    /// plumbing books (gc-common w7-f): G1's, held by [`G1State`]. `None` on
    /// Generational (its beans are the heap's own: `jmx_collectors`,
    /// `gc_notifications`) and on ZGC, whose state lives beside the heap in the
    /// VM (`HeapRealm::backend_gc_beans` in `vm`): a ZGC heap is an
    /// `Arc<ZgcRealHeap>`, which this dispatcher cannot add a field to.
    pub fn backend_gc_beans(&self) -> Option<&Arc<crate::gc_metrics::BackendGcBeans>> {
        match self {
            VmHeap::G1(h) => Some(h.gc_beans()),
            _ => None,
        }
    }

    /// A handle that samples this heap's JMX pools without borrowing the
    /// dispatcher (it holds the collector's `Arc`), for a pause timer that
    /// outlives the borrow it was created under. `None` on Generational, which
    /// describes its pools itself (`GenerationalHeap::jmx_memory_pools`).
    /// gc-common w7-f.
    pub fn jmx_pool_sampler(&self) -> Option<JmxPoolSampler> {
        match self {
            VmHeap::Generational(_) => None,
            VmHeap::G1(h) => Some(JmxPoolSampler::G1(Arc::clone(&h.collector))),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some(JmxPoolSampler::Zgc(Arc::clone(h))),
        }
    }

    /// The pool samples [`crate::gc_metrics::BackendGcBeans::pools`] names, in
    /// [`crate::gc_metrics::BackendBeanShape::pools`] order; empty on
    /// Generational. See [`JmxPoolSampler::sample`].
    pub fn backend_pool_samples(&self) -> Vec<crate::gc_metrics::JmxPoolSample> {
        self.jmx_pool_sampler()
            .map_or_else(Vec::new, |s| s.sample())
    }

    /// Young-only cycles completed so far: ZGC's opt-in generational mode's
    /// minor cycles (`ZgcRealHeap::generational_stats().0`), `0` on every
    /// other backend and in ZGC's default mode. One relaxed load. The GC event
    /// plumbing samples it on both edges of a collection to tell a `ZGC Minor`
    /// cycle from a `ZGC Major` one (gcd d10/o retirement).
    pub fn young_only_cycles(&self) -> usize {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => 0,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.generational_stats().0,
        }
    }

    /// Wave 9 lane G: G1's old-collecting pauses so far
    /// ([`crate::g1::G1Collector::old_collecting_pause_count`]); `None` on
    /// every other backend. Sampled on both edges of a collection to tell a
    /// young-only G1 pause from a mixed one (HotSpot's
    /// `all_memory_pools_affected`).
    pub fn g1_old_collecting_pauses(&self) -> Option<u64> {
        match self {
            VmHeap::G1(h) => Some(h.collector.old_collecting_pause_count()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// zt pl4 item 3: on G1, `(parallel workers, concurrent mark)` thread CPU
    /// in nanoseconds (`G1Collector::gc_thread_cpu_ns`), the figures behind
    /// HotSpot G1's `sun.threads.cpu_time.gc_parallel_workers` /
    /// `gc_conc_mark` and `total_gc_cpu_time`; `None` on the other
    /// collectors (HotSpot's ZGC and Serial publish neither, and read 0).
    pub fn g1_gc_thread_cpu_ns(&self) -> Option<(u64, u64)> {
        match self {
            VmHeap::G1(state) => Some(state.collector.gc_thread_cpu_ns()),
            _ => None,
        }
    }

    /// zr z1/tr: HotSpot 25 ZGC's `GCCause` for an allocation-driven
    /// collection about to run on this heap -- `Allocation Stall` when the
    /// allocation already failed (`allocation_failed`), else the rule
    /// `needs_gc` charged (`Warmup`, `Allocation Rate`, `Proactive`,
    /// `High Usage`; `Allocation Rate` when none was charged). zr z2/t2: the
    /// failure door also names a pending rule (the compiled refill trigger
    /// takes that door on an occupancy verdict), and says `Allocation Stall`
    /// only when no rule was charged. Call it BEFORE
    /// the collection: it reads the live trigger latch, which the collection
    /// clears. `None` on Generational and G1 (their door's `Allocation
    /// Failure` is HotSpot's own spelling there) and under
    /// `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`. HotSpot's ZGC never says
    /// `Allocation Failure`; this VM's ZGC said it on every allocation-driven
    /// collection until 2026-10-12.
    pub fn zgc_hotspot_gc_cause(&self, allocation_failed: bool) -> Option<&'static str> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                if !h.hotspot_gc_causes_enabled() {
                    None
                } else if allocation_failed {
                    // zr z3/t2: a failure door with no occupancy
                    // classification: `Allocation Stall` (see
                    // `zgc_forced_door_gc_cause` for the classified form).
                    h.allocation_failure_gc_cause(false)
                } else {
                    Some(h.hotspot_gc_cause(true).unwrap_or("Allocation Rate"))
                }
            }
        }
    }

    /// zr z3/t2: HotSpot 25 ZGC's cause for a collection taken through the
    /// allocation-failure door whose site the VM classified: `occupancy`
    /// (an occupancy-verdict door) carries the pending rule, any other door is
    /// `Allocation Stall` (`ZgcRealHeap::allocation_failure_gc_cause`). Call
    /// it BEFORE the collection. `None` off ZGC and under
    /// `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`.
    pub fn zgc_forced_door_gc_cause(&self, occupancy: bool) -> Option<&'static str> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = occupancy;
                None
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.allocation_failure_gc_cause(occupancy),
        }
    }

    /// zt t31/x10 (item 1): the last ZGC collection's Mark Start occupancy,
    /// when it closed a concurrent cycle (`ZgcRealHeap::last_mark_start_used`);
    /// `None` off ZGC and after a stop-the-world mark.
    pub fn zgc_last_mark_start_used(&self) -> Option<usize> {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.last_mark_start_used(),
            _ => None,
        }
    }

    /// zt t31/x10 (item 1): the bytes the ZGC slides have copied so far
    /// (`ZgcRealHeap::slide_moved_bytes_total`); 0 off ZGC.
    pub fn zgc_slide_moved_bytes_total(&self) -> usize {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.slide_moved_bytes_total(),
            _ => 0,
        }
    }

    /// zt t31/x10 (item 5): an allocation stall is about to ask for its
    /// collection; the cycle that serves it, whoever runs it, is a stall's
    /// (`ZgcRealHeap::note_stall_cycle_pending`). No-op off ZGC.
    pub fn zgc_note_stall_cycle_pending(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            h.note_stall_cycle_pending();
        }
    }

    /// zr z3/t2: whether a forced collection at a door the VM classified is an
    /// ALLOCATION STALL on ZGC. With `CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER`
    /// on (default): every non-occupancy door. At `=0`: every door but the
    /// compiled refill trigger's (`refill_trigger`), the z2 reading. `false`
    /// off ZGC.
    pub fn zgc_forced_collection_is_stall(&self, occupancy: bool, refill_trigger: bool) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = (occupancy, refill_trigger);
                false
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                if h.stall_classification_by_door() {
                    // zt t20/tg9: under `-XX:+ZCollectionIntervalOnly` an
                    // occupancy door's collection is a stall too
                    // (`ZgcRealHeap::occupancy_door_is_not_a_stall`).
                    !occupancy || !h.occupancy_door_is_not_a_stall()
                } else {
                    !refill_trigger
                }
            }
        }
    }

    /// zr z2/t2: record one mutator allocation stall of `nanos` on ZGC
    /// (`ZgcRealHeap::note_allocation_stall`); a no-op elsewhere.
    pub fn zgc_note_allocation_stall(&self, nanos: u64) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = nanos;
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_allocation_stall(nanos),
        }
    }

    /// zr z5/rv5: arm the next ZGC collection's soft-precise probe
    /// (`ZgcRealHeap::arm_soft_precise_probe`); a no-op on every other
    /// backend.
    pub fn zgc_arm_soft_precise_probe(&self, candidates: Vec<usize>) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = candidates;
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.arm_soft_precise_probe(candidates),
        }
    }

    /// zr z5/rv5: the last ZGC collection's soft-precise answer
    /// (`ZgcRealHeap::take_soft_precise_kept`); `None` elsewhere.
    pub fn zgc_take_soft_precise_kept(&self) -> Option<Vec<usize>> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.take_soft_precise_kept(),
        }
    }

    /// zr z4/jf: the last ZGC collection's JFR facts
    /// (`ZgcRealHeap::take_jfr_cycle`), taken once; `None` elsewhere.
    #[cfg(feature = "zgc")]
    pub fn zgc_take_jfr_cycle(&self) -> Option<crate::zgc::ZJfrCycle> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            VmHeap::Zgc(h) => h.take_jfr_cycle(),
        }
    }

    /// zs s7/jf: the reference pass's per-type counts (soft, weak, final,
    /// phantom) for the current ZGC collection's `jdk.GCReferenceStatistics`;
    /// a no-op elsewhere (Generational and G1 write no such event).
    pub fn zgc_note_jfr_reference_counts(&self, counts: [u64; 4]) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = counts;
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_jfr_reference_counts(counts),
        }
    }

    /// zs s7/jf: the tenuring threshold a ZGC young (`ZGC Minor`) collection
    /// reports on `jdk.ZYoungGarbageCollection`
    /// (`ZgcRealHeap::jfr_tenuring_threshold`): `Some` only on ZGC in its
    /// generational mode. Kept apart from [`Self::tenuring_threshold`], whose
    /// `None` on ZGC keeps the Serial/G1 young event off ZGC collections.
    pub fn zgc_jfr_tenuring_threshold(&self) -> Option<u32> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.jfr_tenuring_threshold(),
        }
    }

    /// zs s7/jf: the page HotSpot 25 ZGC would have stalled on for the request
    /// this heap's arena last refused (`ZgcRealHeap::jfr_stall_request_bytes`,
    /// mapped by `hotspot_stall_page` over `max_heap_bytes`): the `type` and
    /// `size` of `jdk.ZAllocationStall`. `None` off ZGC and when no recent
    /// refusal is on record.
    pub fn zgc_jfr_stall_page(&self, max_heap_bytes: usize) -> Option<(&'static str, u64)> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = max_heap_bytes;
                None
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h
                .jfr_stall_request_bytes()
                .map(|bytes| crate::zgc::hotspot_stall_page(bytes as u64, max_heap_bytes as u64)),
        }
    }

    /// zs s4/ob3: the last ZGC collection's phase boundaries for the pause
    /// bean's split (`ZgcRealHeap::take_pause_split`), taken once; `None`
    /// elsewhere (G1 and Generational book one record per pause, unchanged).
    pub fn zgc_take_pause_split(&self) -> Option<crate::gc_metrics::ZgcPauseSplit> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.take_pause_split(),
        }
    }

    /// zr z1/tr: `-XX:SoftMaxHeapSize` on ZGC (the warm-up rule and the
    /// `CRATONVM_ZGC_RATE_TRIGGER` rules read it); a no-op elsewhere (HotSpot
    /// reads the flag on ZGC only).
    pub fn set_soft_max_heap_bytes(&self, bytes: usize) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.set_soft_max_heap_bytes(bytes),
        }
    }

    /// zt t6/tc: the manageable `SoftMaxHeapSize` JMX row on ZGC: `(soft
    /// maximum, capacity, origin kind)` (see
    /// `ZgcRealHeap::soft_max_origin_kind`); `None` elsewhere (the row is
    /// listed on ZGC only, which is where it has an effect).
    pub fn zgc_soft_max_flag(&self) -> Option<(u64, u64, u8)> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some((
                h.reported_soft_max_capacity() as u64,
                h.heap_capacity() as u64,
                h.soft_max_origin_kind(),
            )),
        }
    }

    /// zt t6/tc: `setVMOption("SoftMaxHeapSize", ..)` on ZGC; `false`
    /// elsewhere (nothing applied).
    pub fn zgc_set_soft_max_by_management(&self, bytes: u64) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {
                let _ = bytes;
                false
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                h.set_soft_max_heap_bytes_by_management(
                    usize::try_from(bytes).unwrap_or(usize::MAX),
                );
                true
            }
        }
    }

    /// zr z1/tr: HotSpot's `soft_max_capacity()` on ZGC (`None` elsewhere),
    /// for the `gc,init` block's `Soft Max Capacity:` line.
    pub fn soft_max_capacity(&self) -> Option<usize> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some(h.reported_soft_max_capacity()),
        }
    }

    /// The age at which a surviving object is promoted, for the backend's
    /// YOUNG generation — `None` where there is no young generation to promote
    /// out of.
    ///
    /// Exists for the JFR `jdk.YoungGarbageCollection` event, whose one
    /// production caller passed the literal `15` for every backend until
    /// 2026-09-23 (G1's configured ceiling; Generational promotes at 3). Per
    /// arm:
    ///
    /// * Generational — `GenerationalHeap::hotspot_tenuring_threshold`, in
    ///   HotSpot's unit like G1's (gen r5w2/obs6): `gen_heap::PROMOTION_AGE -
    ///   1` (= 2) by default, the adaptive threshold itself when adaptive
    ///   tenuring is engaged. Until gen r5w2/obs6 this arm reported the
    ///   collector's "survivals before promotion" (`PROMOTION_AGE`, 3), one
    ///   above what HotSpot and the G1 arm report for the same policy
    ///   (`gengc-r4w6-young6-tenuring-threshold-units-differ-across-backends`).
    /// * G1 — the threshold the NEXT pause will use (adaptive by default; the
    ///   configured `promotion_age` under `CRATONVM_G1_ADAPTIVE_TENURING=0`).
    /// * ZGC — `None`: the default collector is non-generational, and its
    ///   opt-in generational mode ages objects by header label rather than by
    ///   a young space a JFR young-GC event describes.
    pub fn tenuring_threshold(&self) -> Option<u32> {
        match self {
            VmHeap::Generational(h) => Some(u32::from(h.hotspot_tenuring_threshold())),
            VmHeap::G1(h) => Some(u32::from(h.collector.tenuring_threshold())),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
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
    ///
    /// G1 reports Eden + Survivor regions — HotSpot's two young pools, "G1 Eden
    /// Space" and "G1 Survivor Space" (gc-common w4-f; Survivor was on the old
    /// side until then). Everything else a G1 heap holds is in
    /// [`Self::old_gen_stats`]. Both arms read one
    /// [`G1Collector::region_census`] each.
    pub fn young_gen_stats(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => {
                let from = h.young_from_used();
                let cap = h.young_semi_capacity();
                (from, cap)
            }
            VmHeap::G1(h) => {
                let c = h.region_census();
                (
                    c.eden.0 + c.survivor.0,
                    (c.eden.1 + c.survivor.1) * c.region_size,
                )
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => (0, 0),
        }
    }

    /// Return old generation (used, capacity) in bytes.
    ///
    /// # G1: everything that is not Eden (2026-09-23)
    ///
    /// This arm returned `G1Collector::old_gen_stats` — regions of type `Old`
    /// only, capacity = `count(Old) × region_size`. Survivor, `HumongousStart`
    /// / `HumongousContinuation` and Free regions were in NEITHER this nor
    /// [`Self::young_gen_stats`], so the serviceability heap summary built from
    /// the pair reported, after a young pause, a heap the size of its Old
    /// regions, and a 100 MiB humongous array as 0 bytes used
    /// (`docs/feature-designs/concurrent-gc-maturation.md`).
    ///
    /// It is now HotSpot's "G1 Old Gen": `used` = Old + humongous regions'
    /// bytes (a humongous start region's cursor spans the whole object),
    /// `capacity = heap_capacity − young capacity` (so Free regions count,
    /// HotSpot's "old absorbs the rest" convention). The young/old pair
    /// therefore still partitions the whole heap. Since gc-common w4-f
    /// Survivor regions are on the YOUNG side ([`Self::young_gen_stats`]),
    /// read from [`G1Collector::region_census`] — one region-table walk, so
    /// the families within one call are a snapshot; the young and old calls
    /// are two walks, so under a racing allocation the PAIR is a sample
    /// (capacity saturating, never below `used`).
    pub fn old_gen_stats(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => {
                let cap = h.old_gen_capacity();
                let used = h.old_gen_used();
                (used, cap)
            }
            VmHeap::G1(h) => {
                let c = h.region_census();
                let young_cap = (c.eden.1 + c.survivor.1) * c.region_size;
                let used = c.old.0 + c.humongous.0;
                (used, h.heap_capacity().saturating_sub(young_cap).max(used))
            }
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
    /// **Why not simply `heap_capacity() - live_bytes_estimate()` on
    /// Generational.** Its young generation can run non-moving (a live JIT
    /// frame diverts the cycle to the in-place sweep — see `docs/GC.md`'s
    /// `divert_non_moving` table), and under a non-moving, fragmenting young
    /// generation "unused bytes" and "bytes an allocation can actually obtain"
    /// diverge without bound: a heap can be 60% unused and still fail a modest
    /// allocation because no single free run is large enough. A policy keyed
    /// on unused bytes then refuses to clear soft references precisely while
    /// allocation is failing — the worst possible time — and `OutOfMemoryError`
    /// is thrown with a heap full of reclaimable soft-reachable objects.
    ///
    /// So the Generational arm reports the free space of the generation that
    /// must satisfy the next allocation (young), floored at the old
    /// generation's headroom. It is intentionally *pessimistic*:
    /// under-reporting free space makes the policy clear soft references
    /// sooner, which costs cache hit rate; over-reporting makes it clear them
    /// never, which costs the process. Prefer the recoverable failure.
    ///
    /// # G1 and ZGC: whole-heap headroom, which is HotSpot's own figure
    ///
    /// Both have ONE allocation pool, so "the generation that satisfies the
    /// next allocation" is the whole heap and the answer is the maximum
    /// capacity less `allocated_bytes()` — exactly HotSpot's
    /// `LRUMaxHeapPolicy` (`MaxHeapSize - used_at_last_gc`). For G1 that is
    /// the `-Xmx` region grid, not the sizing policy's soft capacity.
    ///
    /// **The G1 arm was wrong until 2026-09-23, in the direction that clears
    /// every soft reference at every collection.** It went through the
    /// generational formula above with G1's `eden_stats` / `old_gen_stats`,
    /// whose CAPACITIES are "regions currently of that type × region size" —
    /// so Free regions, which ARE G1's headroom, were counted nowhere. After a
    /// young pause Eden is empty (capacity 0), the formula fell into its
    /// "no young space" arm and answered the unused tails of the Old regions:
    /// under a megabyte on a fresh 256 MiB heap, i.e. `0` MB, i.e. maximum
    /// pressure — `SoftRefLRUPolicyMSPerMB × 0` makes every soft reference idle
    /// for longer than the threshold, so every `SoftReference` cache under
    /// `-XX:+UseG1GC` was emptied by every pause whatever the heap occupancy.
    /// Pinned by `soft_ref_policy_free_mb_on_g1_counts_free_regions`.
    ///
    /// The ZGC arm already produced the whole-heap figure (its
    /// `young_gen_stats` is `(0, 0)` and its `old_gen_stats` is the whole
    /// arena); it is now stated directly rather than reached by accident.
    ///
    /// Consumers: `process_references_after_gc` and
    /// `g1_remark_process_references` (`vm/src/runtime/interpreter/gc_and_alloc.rs`)
    /// and the interpreter's soft-reference touch path. (This doc carried a
    /// "HANDOFF — no caller yet" section until 2026-09-23; the handoff had
    /// landed.)
    pub fn soft_ref_policy_free_mb(&self) -> usize {
        const MB: usize = 1024 * 1024;
        match self {
            VmHeap::Generational(_) => {}
            VmHeap::G1(h) => {
                // g1sz: the `-Xmx` grid, not `heap_capacity()`. Under the
                // adaptive sizing policy (default ON since 2026-10-26) the
                // latter is the soft capacity, which starts at `-Xms`, and
                // HotSpot's `LRUMaxHeapPolicy` measures against
                // `MaxHeapSize` whatever is committed: on a 64 MiB heap
                // that soft capacity left a few-KiB live set reporting 3 MB
                // free, and the policy clearing SoftReferences HotSpot keeps.
                let max = h.collector.heap_target_bytes_within(false);
                return max.saturating_sub(self.allocated_bytes()) / MB;
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {
                return self.heap_capacity().saturating_sub(self.allocated_bytes()) / MB;
            }
        }
        let (young_used, young_cap) = self.young_gen_stats();
        // gcd d4/n (`gcd-d4n-soft-policy-free-mb-ignores-the-young-free-list`):
        // a non-moving young sweep frees onto the from-space free list and
        // never retracts the bump cursor, so `young_from_used()` after it
        // still reads "young is full" and the soft LRU saw ~0 MB free. The
        // free list is empty after a moving cycle, so that case is unchanged.
        let young_used = match self {
            VmHeap::Generational(h) => young_used.saturating_sub(h.young_from_free_bytes()),
            _ => young_used,
        };
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
            // Only Generational reaches this formula now (G1 and ZGC returned
            // above); a zero young capacity is not a state it produces, and
            // the old-gen pair is the right answer if it ever does.
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

    /// Could [`Self::satb_barrier`] retain a value right now? `false` only when
    /// the collector's SATB queue is known closed (G1, generational); ZGC
    /// answers `true`, its own gate being inside the barrier. For a caller that
    /// must do work BEFORE it can name the value to log (SBR4 satb: the
    /// `Reference.referent` keep-alive in `jit_getfield`), so the common
    /// no-cycle case pays one acquire load instead of that work.
    #[inline]
    pub fn satb_armed(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.satb_queue_ref().is_some_and(|q| q.is_active()),
            VmHeap::G1(h) => h.satb_queue().is_active(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => true,
        }
    }

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

    /// zt t23/zb7 (stage 2f blocker (c)): the `Reference.referent`
    /// keep-alive -- a referent the mutator just read out of a `Reference`
    /// (`Reference.get()` interpreted, compiled or native; JNI
    /// `GetObjectField` of the referent slot; `NativeContext::gc_reference_keep_alive`)
    /// stays alive for the running mark. G1 / generational: the SATB barrier,
    /// as before. ZGC: [`ZgcRealHeap::referent_keep_alive`], which under the
    /// color flip marks through the load barrier's mark step instead of the
    /// SATB pre-barrier and is otherwise exactly [`Self::satb_barrier`].
    ///
    /// zt t23/zb8: also the keep-alive of every other weakly held object the
    /// VM resolves and hands on -- weak JNI globals (`jobject_to_obj`,
    /// `resolve_global_handle_keeping_weak_alive`), the weak door's survivors
    /// (`WeakDoorArgs::repin`), weak layer-module handles, JDWP weak handles and
    /// interned-String pool hits -- so none of them publishes only through SATB.
    pub fn referent_keep_alive(&self, referent: Value) {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                if let Value::Object(Some(obj_ref)) = referent {
                    h.referent_keep_alive(obj_ref.as_ptr() as usize);
                }
            }
            _ => self.satb_barrier(referent),
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

    /// gcj j2/og2 — the span a Generational concurrent cycle's mark bitmap
    /// covers (`GenerationalHeap::old_gen_mark_span`: base and growth limit).
    /// `(0, 0)` on G1 and ZGC, exactly as [`Self::old_gen_info`].
    pub fn old_gen_mark_span(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => h.old_gen_mark_span(),
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
            VmHeap::Generational(h) => {
                // gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`, opt-in;
                // `gcj-j1cm-proposal-dirty-thread-satb-walk`, Generational
                // half): the remark / cleanup walks of this heap's queue visit
                // only the threads that logged during the cycle. Before the
                // queue's first activation; G1's queue is never switched.
                if gen_satb_dirty_thread_walk_enabled() {
                    satb_queue.enable_dirty_thread_walk();
                }
                h.enable_concurrent_gc(satb_queue, gc_state)
            }
            VmHeap::G1(_) => {} // G1 has built-in concurrent marking
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => {}
        }
    }

    /// gen r4w5/concmark5 (2026-09-24) — should the VM start the generational
    /// concurrent-cycle SERVICE thread (`ConcurrentGcState::serve`)? `true`
    /// only on the Generational backend with `CRATONVM_GEN_CONC_SERVICE_THREAD`
    /// set. G1 has its own controller (`G1State::concurrent_mark`); ZGC's
    /// marker threads are its own.
    pub fn wants_concurrent_service_thread(&self) -> bool {
        match self {
            VmHeap::Generational(_) => ConcurrentGcState::service_enabled(),
            VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// gen r4w5/concmark5 — the generational concurrent cycle's trigger, for
    /// the service thread's periodic check (`ConcurrentServiceHooks::cycle_due`).
    /// On the service thread this is the policy's verdict; on any other thread
    /// with a service attached a due cycle is handed off and this is `false`
    /// (`GenerationalHeap::concurrent_cycle_due`). `false` off Generational.
    pub fn concurrent_cycle_due(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.concurrent_cycle_due(),
            VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
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

    /// gcd d9/c: see `GenerationalHeap::young_starved_behind_blocked_drain`.
    /// G1 and ZGC have no promotion gate of this kind: `false`.
    /// (Lane d9/b applied d9/c's cross-lane request 2a; BUILD DEPENDENCY on
    /// d9/c's `02847aedd`, which defines the Generational method.)
    pub fn young_starved_behind_blocked_drain(&self, min_free: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.young_starved_behind_blocked_drain(min_free),
            VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
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

    /// zr z5/cr: ZGC's bytes allocated since the last collection
    /// (`ZgcRealHeap::bytes_allocated_since_cycle`: objects plus carved TLAB
    /// chunks in full, never an under-read). `None` on Generational and G1,
    /// so every caller is a no-op there.
    pub fn zgc_bytes_allocated_since_cycle(&self) -> Option<usize> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some(h.bytes_allocated_since_cycle()),
        }
    }

    /// zr z5/cr: ZGC only -- after exactly one collection since the arena last
    /// refused a request, does a single span now serve that request
    /// (`ZgcRealHeap::room_for_last_refusal`)? `None` on Generational and G1,
    /// and on ZGC when there is no such record.
    pub fn zgc_room_for_last_refusal(&self) -> Option<bool> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.room_for_last_refusal(),
        }
    }

    /// zs s6/no: arm the calling thread's exhausted verdict after a ZGC
    /// allocation refusal's collection (`ZgcRealHeap::arm_exhausted_verdict`,
    /// opt-in `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`). `false` (and nothing
    /// armed) on Generational and G1, and on ZGC with the switch off.
    pub fn zgc_arm_exhausted_verdict(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.arm_exhausted_verdict(),
        }
    }

    /// zs s8/rv5: is the ZGC exhausted verdict switched on
    /// (`ZgcRealHeap::exhausted_verdict_enabled`)? `false` off ZGC.
    pub fn zgc_exhausted_verdict_enabled(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.exhausted_verdict_enabled(),
        }
    }

    /// zs s9/no2: exhausted verdicts spent (one per fill that a verdict
    /// ended; `ZgcRealHeap::exhausted_verdict_spent_count`); `None` off ZGC.
    pub fn zgc_exhausted_spent_count(&self) -> Option<usize> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some(h.exhausted_verdict_spent_count()),
        }
    }

    /// zs s9/no2: `(capacity, free space the last collection left)` on ZGC
    /// (`ZgcRealHeap::free_after_last_cycle`); `None` on Generational and G1.
    pub fn zgc_free_after_last_cycle(&self) -> Option<(usize, usize)> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some((h.heap_capacity(), h.free_after_last_cycle())),
        }
    }

    /// zt t5/gn: is this a ZGC heap running its generational mode
    /// (`CRATONVM_ZGC_GENERATIONAL`)? `false` on Generational and G1, which
    /// are other collectors. One relaxed load.
    pub fn zgc_generational_enabled(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.generational_enabled(),
        }
    }

    /// zt t10/gn2: `(old generation bytes, bytes promoted)` after the last
    /// young cycle of a ZGC generational heap
    /// (`ZgcRealHeap::gen_last_young_xlog_figures`), for the `-Xlog` young
    /// table of a minor. `None` on Generational and G1 and off the mode.
    pub fn zgc_gen_last_young_xlog_figures(&self) -> Option<(usize, usize)> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.gen_last_young_xlog_figures(),
        }
    }

    /// zt t16/gn6 (item 12): did the last ZGC collection begin with an
    /// allocation failure latched -- HotSpot 25's `is_alloc_stalling_for_old`
    /// clause of `should_preclean_young` (`ZgcRealHeap::
    /// last_collection_after_alloc_failure`)? `false` on Generational and G1.
    pub fn zgc_last_collection_after_alloc_failure(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.last_collection_after_alloc_failure(),
        }
    }

    /// zt t10/gn2: did the last ZGC collection run whole-heap after the
    /// generational young arm of the pre-collection pass (consumed; see
    /// `ZgcRealHeap::take_armed_major_soft_clock_due`)? `false` on
    /// Generational and G1, so the caller's clock note is a no-op there.
    pub fn zgc_take_armed_major_soft_clock_due(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.take_armed_major_soft_clock_due(),
        }
    }

    /// gcj j4/rf: the Generational heap's
    /// `GenerationalHeap::next_pause_certainly_minor`; `false` on every other
    /// collector.
    pub fn gen_next_pause_certainly_minor(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.decide_next_pause_minor(),
            _ => false,
        }
    }

    /// zt t7/cc: `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` is on and the heap
    /// is ZGC in its generational mode. `false` on every other collector, so
    /// the reference pass's young-cycle arm is a no-op there.
    pub fn zgc_young_skip_references_on(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.young_skip_references_on(),
        }
    }

    /// zt t7/cc: the VM's pre-collection reference pass asks whether it may
    /// leave every referent in place for the collection about to run (a
    /// predicted young cycle; `ZgcRealHeap::arm_young_refs_strong`). `false`
    /// on every other collector.
    pub fn zgc_arm_young_refs_strong(&self, want: bool) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.arm_young_refs_strong(want),
        }
    }

    /// zt t11/gn3: is the ZGC collection about to run armed as a young cycle
    /// that keeps its referents (`ZgcRealHeap::young_refs_strong_armed`)?
    /// `false` on every other collector.
    pub fn zgc_young_refs_strong_armed(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.young_refs_strong_armed(),
        }
    }

    /// zs s6/no: drop the calling thread's exhausted verdict (an
    /// `OutOfMemoryError` is being built). No-op off ZGC.
    pub fn zgc_lift_exhausted_verdict(&self) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.lift_exhausted_verdict(),
        }
    }

    /// zt t2/rv: drop the calling thread's exhausted verdict WITHOUT spending
    /// it (`ZgcRealHeap::drop_exhausted_verdict`), for a VM-raised
    /// `OutOfMemoryError` that is not the heap refusal the verdict judged.
    /// No-op off ZGC.
    pub fn zgc_drop_exhausted_verdict(&self) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.drop_exhausted_verdict(),
        }
    }

    /// zs s10/sp: an `OutOfMemoryError` is being built for a refused
    /// allocation: arm the opt-in final-refusal rule
    /// (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`, `ZgcRealHeap::note_final_refusal`).
    /// No-op off ZGC and with the switch unset.
    pub fn zgc_note_final_refusal(&self) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_final_refusal(),
        }
    }

    /// zs s10/sp: `(threshold now, arms, disarms)` of that rule; `None` off ZGC.
    pub fn zgc_sub_request_span_stats(&self) -> Option<(usize, usize, usize)> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some(h.sub_request_span_stats()),
        }
    }

    /// zs s6/no: `(exhausted_armed, exhausted_refused)`; `None` off ZGC.
    pub fn zgc_exhausted_verdict_stats(&self) -> Option<(usize, usize)> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => Some(h.exhausted_verdict_stats()),
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
    /// evacuates by region and ZGC compacts by a whole-heap slide with its own
    /// per-cycle coverage proof — neither has a moving YOUNG generation — so on
    /// both of them the precise-moving-young question, and the unmemoised
    /// full-stack probe that answers it, is inert work. See the two
    /// `moving_young_precise_only` sites. (This said "ZGC never moves anything"
    /// until 2026-09-23; ZGC has compacted by default since 2026-08-13.)
    pub fn is_generational(&self) -> bool {
        matches!(self, VmHeap::Generational(_))
    }

    /// Is [`Self::is_object_address`] this backend's own marker root screen?
    ///
    /// ZGC only. Its marker skips any root or child that is not a registered
    /// allocation base (`ZgcRealHeap::collect_garbage`'s `registered.contains`,
    /// counted as `wild_skipped`), and `is_object_address` asks that same
    /// registry, so a candidate it refuses is never marked through: dropping it
    /// before the collection changes no mark. Generational and G1 answer with a
    /// header probe that can refuse a genuine young or mid-initialisation object
    /// (see `Frame::scan_locals_in`), so a refusal there proves nothing.
    pub fn object_start_is_the_mark_screen(&self) -> bool {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => true,
        }
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
    ///
    /// # LANE W3-C — this is the ONLY production consult of the IHOP policy
    ///
    /// Every caller of this function is a mark-cycle trigger
    /// (`interpreter::maybe_concurrent_gc`, `jit_drive_g1_concurrent_mark`),
    /// and it used to be the whole policy: two loads and a comparison,
    /// re-implemented here. `G1Collector::check_ihop` — which carries the
    /// mark-cycle back-off that wave 2 landed behind
    /// `CRATONVM_G1_IHOP_BACKOFF` — had **no caller outside tests**. A
    /// workspace grep for `check_ihop(` returned its definition, its doc
    /// citations, and `gc/src/g1.rs`'s own `mod tests` plus
    /// `gc/tests/g1_w2c_ihop_model.rs`. The flag was inert on every real run,
    /// so `backoff_declined_polls` read zero by construction and that zero was
    /// indistinguishable from "the back-off never needed to fire".
    ///
    /// That is `orchestrator-wave-1-measurements.md` §4 verbatim — a lever
    /// read on a path the workload does not take — arriving inside the wave
    /// that wrote §4 down.
    ///
    /// **The conjunction below is ordered so the DEFAULT build is unchanged.**
    /// With `CRATONVM_G1_IHOP_BACKOFF` off, `check_ihop` returns
    /// `old_bytes >= threshold`, which the second clause already implies, so
    /// the whole expression evaluates exactly as it did before. The only
    /// behaviour that changes is with the flag ON, which is what a flag is
    /// for. `check_ihop` is called last for the same reason: it is the only
    /// clause with a side effect (it counts the consult) and the cheap loads
    /// should short-circuit ahead of it.
    pub fn g1_should_start_marking(&self) -> bool {
        match self {
            VmHeap::G1(g1) => {
                // gci i22/sz: a humongous request that crossed IHOP asked for
                // this cycle (default-ON `CRATONVM_G1_HUMONGOUS_IHOP_START`; one
                // relaxed load otherwise). HotSpot's `G1 Humongous Allocation`
                // collection is the concurrent start by its cause, whatever
                // the old occupancy reads without the request.
                g1.humongous_conc_start_requested()
                    // Check if old gen bytes exceed the G1 marking threshold (IHOP)
                    || (g1.marking_threshold_bytes() > 0
                        && g1.old_gen_bytes() > g1.marking_threshold_bytes()
                        && g1.check_ihop())
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

    /// zr z3/cs: is a ZGC concurrent cycle REQUESTED by `needs_gc` and waiting
    /// for a poll site to open it (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`)? `false`
    /// on the other backends and with the switch off.
    #[inline]
    pub fn zgc_conc_start_requested(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.conc_start_requested(),
        }
    }

    /// zr z3/cs: the VM ran a ZGC mark-start pause (`opened`) or refused one.
    /// A no-op on the other backends and with the switch off.
    pub fn zgc_note_conc_start_outcome(&self, opened: bool) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_conc_start_outcome(opened),
        }
    }

    /// zr zs s1/tp: the VM dropped a ZGC mark start before requesting its
    /// pause because the start was no longer due. A no-op on the other
    /// backends.
    pub fn zgc_note_conc_start_skipped(&self) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_conc_start_skipped(),
        }
    }

    /// zt t24/hunt7 (`CRATONVM_ZGC_OOM_LADDER_FRESH_MARK`): did the last ZGC
    /// collection close a concurrent cycle with that cycle's mark set? `false`
    /// on the other backends ([`ZgcRealHeap::last_collection_marked_concurrently`]).
    pub fn zgc_last_collection_marked_concurrently(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.last_collection_marked_concurrently(),
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

    /// zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): has the open ZGC
    /// concurrent cycle's marker reached its fixed point? `false` with no
    /// cycle open and on the other backends.
    pub fn zgc_concurrent_mark_caught_up(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.concurrent_mark_caught_up(),
        }
    }

    /// zt t18/zc1: a ZGC `System.gc()` request asks for a concurrent cycle of
    /// its own (`ZgcRealHeap::request_system_gc_concurrent_cycle`). `false`:
    /// nothing was recorded (switch off, not the every-trigger mode, a cycle
    /// already open, another backend) and the request collects as before.
    pub fn zgc_request_system_gc_concurrent_cycle(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.request_system_gc_concurrent_cycle(),
        }
    }

    /// zt t18/zc1: the mark start of a ZGC `System.gc()` request ran; `opened`
    /// = a cycle is open now. A no-op on the other backends.
    pub fn zgc_note_system_gc_concurrent_cycle(&self, opened: bool) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_system_gc_concurrent_cycle(opened),
        }
    }

    /// zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): the collecting
    /// door will run a deferred sweep after it releases the world, so the
    /// collection it is about to run may park its sweep. Call inside the
    /// pause, right before the collection. A no-op off ZGC and with the switch
    /// off.
    pub fn zgc_arm_concurrent_sweep_runner(&self) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.arm_concurrent_sweep_runner(),
        }
    }

    /// zt t21/zc2: run the sweep the collection just parked, on this (the
    /// collecting) thread, with the world released. `true` when the
    /// collection deferred its sweep (zt t22/x18: whoever ran it -- this call
    /// or a runner that took it first, `ZgcRealHeap::run_pending_sweep_after_release`):
    /// the heap's after-collection figures changed, and the caller re-samples
    /// them before it reports the collection.
    pub fn zgc_run_pending_sweep_after_release(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.run_pending_sweep_after_release(),
        }
    }

    /// zt t33: the collection's pause ends: heals and colored stores write the
    /// good color again (`ZgcRealHeap::end_closing_heal_color`). Called by the
    /// VM right before it releases the mutators. A no-op off ZGC.
    pub fn zgc_end_closing_heal_color(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            h.end_closing_heal_color();
        }
    }

    /// zt t32 (R6, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): the collecting door's
    /// drain after its release -- copy every planned object no barrier copied
    /// yet (`ZgcRealHeap::run_concurrent_relocate_drain`). `false` (one lock)
    /// when nothing is planned, and off ZGC.
    pub fn zgc_run_concurrent_relocate_drain(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.run_concurrent_relocate_drain(),
        }
    }

    /// zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): may this
    /// collection's VM reference round run after the release, before the
    /// parked sweep's install? Asked inside the pause; `false` off ZGC.
    pub fn zgc_reference_round_may_follow_release(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.reference_round_may_follow_release(),
        }
    }

    /// zt t31/zc11: the VM kept a round the heap allowed in the pause.
    pub fn zgc_note_reference_round_kept_by_vm(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            h.note_reference_round_refusal(crate::zgc::ZRefRoundRefusal::Vm);
        }
    }

    /// zt t31/zc11: hold the parked sweep's install for the round (inside the
    /// pause).
    pub fn zgc_hold_sweep_install(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            h.hold_sweep_install();
        }
    }

    /// zt t31/zc11: the round ended (`round_us`, `0` on an unwind).
    pub fn zgc_release_sweep_install_hold(&self, round_us: u64) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            h.release_sweep_install_hold(round_us);
        }
    }

    /// zt t21/zc2: a `System.gc()` caller returns only after the whole cycle
    /// (HotSpot's synchronous request): finish a pending sweep, or wait for
    /// the thread running it. One load when nothing is pending.
    pub fn zgc_finish_pending_sweep_for_waiter(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.finish_pending_sweep(crate::zgc::ZSweepRunner::Waiter),
        }
    }

    /// zt t23/x19 (page item 28d; `CRATONVM_ZGC_EXIT_FINISHES_SWEEP`, default
    /// ON, `=0` kill): the VM's exit finishes a pending deferred sweep (or
    /// waits for its runner) before it reads the heap it reports
    /// (`ZgcRealHeap::finish_pending_sweep_at_exit`). Called by the exit
    /// readers themselves -- the `-Xlog:gc+exit` heap lines, the `[GC]`
    /// summary, the end-of-run metrics row -- so every launcher arm gets it.
    /// One load when nothing is pending; `false` off ZGC.
    pub fn zgc_finish_pending_sweep_at_exit(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.finish_pending_sweep_at_exit(),
        }
    }

    /// zt t21/zc2: the live bytes the last installed deferred sweep left,
    /// without what the mutators allocated after its pause (`None` off ZGC,
    /// with the switch off, or before the first install).
    pub fn zgc_concurrent_sweep_last_live(&self) -> Option<usize> {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.concurrent_sweep_last_live(),
        }
    }

    /// zr z1/rf (`CRATONVM_ZGC_CONC_REF_SKIP`): hand the VM's active Weak /
    /// Phantom `Reference` objects to the ZGC concurrent mark cycle about to
    /// open (`ZgcRealHeap::set_concurrent_vm_reference_skip`). A no-op on G1
    /// and Generational, which have their own skip sets.
    pub fn zgc_set_concurrent_reference_skip(&self, addrs: Vec<usize>) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.set_concurrent_vm_reference_skip(addrs),
        }
    }

    /// zr z2/cc (`CRATONVM_ZGC_CONC_REF_SKIP_SOFT`): hand the VM's active Soft
    /// `Reference` objects to the ZGC concurrent mark cycle about to open
    /// (`ZgcRealHeap::set_concurrent_vm_soft_reference_skip`: hidden for the
    /// concurrent phase, kept referents re-rooted at the mark end). A no-op on
    /// G1 and Generational.
    pub fn zgc_set_concurrent_soft_reference_skip(&self, addrs: Vec<usize>) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.set_concurrent_vm_soft_reference_skip(addrs),
        }
    }

    /// zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in): may the
    /// pre-collection pass HIDE the referent slots of this collection instead
    /// of nulling them (`ZgcRealHeap::concurrent_refs_skip_mode`)? `false`
    /// off ZGC, with the switch off and in generational mode. Ask once per
    /// collection, inside the pause.
    pub fn zgc_concurrent_refs_skip_mode(&self) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.concurrent_refs_skip_mode(),
        }
    }

    /// zt t22/zc3: hand the rows the pre-collection pass hid to the collection
    /// about to run (`ZgcRealHeap::set_stw_vm_reference_skip`). A no-op off
    /// ZGC.
    pub fn zgc_set_stw_reference_skip(&self, addrs: Vec<usize>) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.set_stw_vm_reference_skip(addrs),
        }
    }

    /// zt t22/zc3: forget the collection's hidden rows (the door, after the
    /// collection). A no-op off ZGC.
    pub fn zgc_clear_stw_reference_skip(&self) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.clear_stw_vm_reference_skip(),
        }
    }

    /// zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): run the referent clear
    /// the collection staged (`ZgcRealHeap::run_staged_referent_clear`). A
    /// no-op off ZGC and when nothing is staged.
    pub fn zgc_run_staged_referent_clear(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            h.run_staged_referent_clear();
        }
    }

    /// zt t32: the referent read gate's state (`ZgcRealHeap::referent_gate_state`);
    /// `ZGC_REFERENT_GATE_NEVER` (0) off ZGC.
    #[inline]
    pub fn zgc_referent_gate_state(&self) -> u8 {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.referent_gate_state(),
            _ => 0,
        }
    }

    /// j27 wave 5 (lane REFS): can the referent read gate ever arm on this
    /// heap (`ZgcRealHeap::referent_gate_may_arm`)? `false` off ZGC.
    pub fn zgc_referent_gate_may_arm(&self) -> bool {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.referent_gate_may_arm(),
            _ => false,
        }
    }

    /// zt t32: is a referent read blocked by the armed gate
    /// (`ZgcRealHeap::referent_read_blocked`)? `false` off ZGC.
    pub fn zgc_referent_read_blocked(&self, receiver: usize, referent: usize) -> bool {
        match self {
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.referent_read_blocked(receiver, referent),
            _ => false,
        }
    }

    /// zt t22/zc3 (the census): the door's laps of this collection's VM
    /// reference round, in microseconds (`ZgcRealHeap::note_vm_reference_round`).
    /// A no-op off ZGC.
    pub fn zgc_note_vm_reference_round(&self, pre_us: u64, post_us: u64, jni_us: u64) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {
                let _ = (pre_us, post_us, jni_us);
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.note_vm_reference_round(pre_us, post_us, jni_us),
        }
    }

    /// zt t31/zc11 (1c step 2c): the census note of a round whose body ran
    /// after the release (`ZgcRealHeap::note_vm_reference_round_after_release`).
    /// A no-op off ZGC.
    pub fn zgc_note_vm_reference_round_after_release(
        &self,
        pre_us: u64,
        post_us: u64,
        jni_us: u64,
        after_us: u64,
    ) {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {
                let _ = (pre_us, post_us, jni_us, after_us);
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                h.note_vm_reference_round_after_release(pre_us, post_us, jni_us, after_us)
            }
        }
    }

    /// zr z1/rf: can the interned-String table be a WEAK root on this heap?
    /// `true` only on a ZGC heap whose every reclamation is a stop-the-world
    /// whole-heap collection followed, in the same pause, by the VM's
    /// `update_all_roots` epilogue: concurrent marking is not configured
    /// (`CRATONVM_ZGC_CONC_START` unset / `0`: the trigger is latched at
    /// construction) and generational mode is off. A concurrent mark would
    /// need a keep-alive barrier on pool hits (a mutator could fetch an
    /// unmarked pooled String and hide it in a scanned object), and a minor
    /// cycle's young sweep was not reviewed for the table's verdicts. `false`
    /// on G1 and Generational (their gate is the VM's own).
    #[inline]
    pub fn zgc_weak_string_table_sound(&self) -> bool {
        self.zgc_weak_string_table_sound_for(false)
    }

    /// zt t27/gn5 (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`, the gn page's item
    /// 13): [`Self::zgc_weak_string_table_sound`], with the generational mode
    /// admitted when `generational_ok` (the VM's switch). Sound there because
    /// every collection of the mode is stop-the-world and ends in the same
    /// `update_all_roots` sweep, whose ZGC verdict is the exact start registry
    /// (`addr_keyed::survived_in_place` -> `ZgcRealHeap::is_object_address`):
    /// a minor sweeps only NON-old bases (`sweep_young_aged_words`: `registered
    /// & !old`), so an old pooled String the minor did not trace stays
    /// registered and its entry stays (only a major, which traces the old
    /// generation, can prove it dead), while a dead young one is unregistered
    /// by the minor that freed it and its entry is dropped.
    #[inline]
    pub fn zgc_weak_string_table_sound_for(&self, generational_ok: bool) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {
                let _ = generational_ok;
                false
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                h.adaptive_conc_start_state().2 == 0
                    && !h.concurrent_mark_active()
                    && (generational_ok || !h.generational_enabled())
            }
        }
    }

    /// zt t23/st1 (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`): the
    /// interned-String table's soundness test once the VM supplies the three
    /// pieces a concurrent mark needs (`vm/src/memory/string_table.rs`,
    /// module doc "Concurrent ZGC"): the mark start's and the closing
    /// collection's root scans leave the pool out, every pool hit while a
    /// cycle marks goes through the SATB keep-alive, and the closing
    /// collection's `update_all_roots` sweeps the pool under the mark's
    /// verdict. Then only generational mode is refused (a minor cycle's young
    /// sweep was not reviewed for the table's verdicts). `false` on G1 and
    /// Generational, as [`Self::zgc_weak_string_table_sound`].
    #[inline]
    pub fn zgc_weak_string_table_sound_under_concurrent_mark(&self) -> bool {
        self.zgc_weak_string_table_sound_under_concurrent_mark_for(false)
    }

    /// zt t27/gn5: [`Self::zgc_weak_string_table_sound_under_concurrent_mark`]
    /// with the generational mode admitted when `generational_ok`
    /// (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`; see
    /// [`Self::zgc_weak_string_table_sound_for`]). A young cycle is never
    /// predicted or run while a concurrent cycle is open
    /// (`ZgcRealHeap::young_cycle_predicted` refuses on `conc_cycle_active`),
    /// so a concurrent major of the mode is the non-generational case: its two
    /// scans leave the pool out, its hits are kept alive, its closing pause
    /// sweeps.
    #[inline]
    pub fn zgc_weak_string_table_sound_under_concurrent_mark_for(
        &self,
        generational_ok: bool,
    ) -> bool {
        match self {
            VmHeap::G1(_) | VmHeap::Generational(_) => {
                let _ = generational_ok;
                false
            }
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => generational_ok || !h.generational_enabled(),
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

    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`):
    /// has G1 taken enough consecutive young pauses on a starved Free pool,
    /// with a mark cycle open, that the mark driver should finish the cycle
    /// now? Consumes the streak. See `G1Collector::take_futile_young_escalation`.
    /// The other backends have no G1 mark cycle to finish.
    pub fn g1_take_futile_young_escalation(&self) -> bool {
        match self {
            VmHeap::G1(g1) => g1.take_futile_young_escalation(),
            VmHeap::Generational(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// r23 w6 (`CRATONVM_G1_STARVED_FULL`): should the mark driver abort the
    /// open cycle and run G1's starved full now? Consumes the streak. See
    /// `G1Collector::take_starved_full`. G1 only.
    pub fn g1_take_starved_full(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.take_starved_full())
    }

    /// r23 w6: arm the starved full's pause (`G1Collector::arm_starved_full`).
    /// G1 only; `false` when nothing was armed.
    pub fn g1_arm_starved_full(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.arm_starved_full())
    }

    /// The G1 near-full proxy row: an in-place-only full compaction for the
    /// next pause (`G1Collector::arm_in_place_compaction`), for the
    /// allocation-failure ladder when [`Self::g1_arm_full_compaction`]
    /// refused. `false` off G1.
    pub fn g1_arm_in_place_compaction(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.arm_in_place_compaction())
    }

    /// The R22 aastore-humongous page: see
    /// `G1Collector::arm_fragmented_humongous_slide`. `None` off G1.
    pub fn g1_arm_fragmented_humongous_slide(&self) -> Option<(usize, usize, usize)> {
        match self {
            VmHeap::G1(g1) => g1.arm_fragmented_humongous_slide(),
            _ => None,
        }
    }

    /// gcki-ngr: see `G1Collector::failed_humongous_request_fits`. `false`
    /// off G1.
    pub fn g1_failed_humongous_request_fits(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.failed_humongous_request_fits())
    }

    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the address ranges
    /// this heap's NEXT collection treats as young, for the VM's split root
    /// gather (`ThreadRegistry::arm_parked_split_gather`), or `None` when the
    /// heap cannot take an old-only root section for that collection. Asked
    /// inside the pause, right before the gather.
    ///
    /// The contract a `Some` answer makes: the next collection either follows
    /// no root that lies outside these ranges (a young-only phase: old -> young
    /// edges come from the card table / remembered sets), or adds the section
    /// handed to [`Self::stage_parked_old_only_roots`] back to its root set
    /// first.
    ///
    /// * Generational: both semispaces, unless a concurrent old mark is open
    ///   (SATB armed: its initial mark and remark are separate doors, but the
    ///   split is not argued against an open snapshot). A same-pause major
    ///   (moving Phase 5, the non-moving old sweep) consumes the staged
    ///   section (`GenerationalHeap::parked_old_only_young_ranges`).
    /// * G1: every region that is not `Old` (eden, survivor, humongous, free:
    ///   the eager humongous reclaim reads the root set), and only when the
    ///   pause is provably young-only and no mark is open; a G1 pause never
    ///   consumes a staged section (`G1Collector::parked_old_only_young_ranges`).
    /// * ZGC: never (out of scope).
    pub fn parked_old_only_young_ranges(&self) -> Option<Vec<(usize, usize)>> {
        match self {
            VmHeap::Generational(h) => h.parked_old_only_young_ranges(),
            VmHeap::G1(g1) => g1.parked_old_only_young_ranges(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => None,
        }
    }

    /// gci i13/vg: the old-only root section of this pause's split gather
    /// (see [`Self::parked_old_only_young_ranges`]). Generational keeps it for
    /// the collection that follows and clears it when that collection ends;
    /// G1 answered the ranges only for a young-only pause, so it has no phase
    /// that needs the section and drops it; ZGC never arms a split.
    pub fn stage_parked_old_only_roots(&self, old_only: Vec<ObjectRef>) {
        match self {
            VmHeap::Generational(h) => h.stage_parked_old_only_roots(&old_only),
            VmHeap::G1(_) => drop(old_only),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => drop(old_only),
        }
    }

    /// Preserve a proved fragmented humongous request for the very next
    /// allocation-failure compaction pause.  The caller invokes this only
    /// after a synchronous final remark + cleanup; normal G1 pauses never
    /// acquire a full-compaction goal.
    pub fn g1_arm_fragmented_humongous_compaction(&self) {
        if let VmHeap::G1(g1) = self {
            g1.arm_fragmented_humongous_compaction();
        }
    }

    /// The G1 near-full proxy-define page: the allocation ladder's final
    /// attempt may take one reserve region (`G1Collector::arm_reserve_last_chance`).
    /// A no-op off G1.
    pub fn g1_arm_reserve_last_chance(&self) {
        if let VmHeap::G1(g1) = self {
            g1.arm_reserve_last_chance();
        }
    }

    /// The G1 near-full proxy-define page: the address ranges the last G1
    /// pause slid in place (`G1Collector::last_pause_slid_ranges`); empty off
    /// G1 and after any pause that did not slide.
    pub fn g1_last_pause_slid_ranges(&self) -> Vec<(usize, usize)> {
        match self {
            VmHeap::G1(g1) => g1.last_pause_slid_ranges(),
            _ => Vec::new(),
        }
    }

    /// gcd d10/o: arm one G1 full-compaction pause for the next collection
    /// (see `G1Collector::arm_full_compaction`). The caller has just run a
    /// synchronous final remark + cleanup. `false` off G1, or when that
    /// cleanup did not trust its closure.
    pub fn g1_arm_full_compaction(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.arm_full_compaction())
    }

    /// JIT round 22 wave 11 (lane memory): [`Self::g1_arm_full_compaction`] as
    /// the OOME path's maximal pass (`G1Collector::arm_full_compaction_maximal`).
    pub fn g1_arm_full_compaction_maximal(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.arm_full_compaction_maximal())
    }

    /// Whether G1 holds an allocation-failure compaction request no pause has
    /// consumed yet: a staging pause re-arms it for one more recovery pause.
    pub fn g1_humongous_compaction_pending(&self) -> bool {
        match self {
            VmHeap::G1(g1) => g1.humongous_compaction_pending(),
            _ => false,
        }
    }

    /// True only after G1 has observed a humongous allocation refusal caused
    /// by fragmentation rather than aggregate exhaustion.
    #[inline]
    pub fn g1_has_fragmented_humongous_request(&self) -> bool {
        matches!(self, VmHeap::G1(g1) if g1.has_fragmented_humongous_request())
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
        self.g1_start_concurrent_mark_with_reference_skip(stw, &[]);
    }

    /// [`Self::g1_start_concurrent_mark`] with the cycle's referent-slot skip
    /// set published inside `G1Collector::start_concurrent_mark_with_reference_skip`,
    /// i.e. before the overlay snapshot grays anything and before the marker
    /// is spawned (gci i14/gq G-4). `reference_objs` empty = the plain door
    /// (the caller then publishes with [`Self::g1_set_reference_skip_set`]).
    pub fn g1_start_concurrent_mark_with_reference_skip(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        reference_objs: &[usize],
    ) {
        self.g1_start_concurrent_mark_inner(stw, reference_objs, false);
    }

    /// g1ep7: [`Self::g1_start_concurrent_mark_with_reference_skip`] for the
    /// concurrent cycle: its young regions are root regions
    /// (`G1Collector::start_concurrent_mark_with_root_regions`).
    pub fn g1_start_concurrent_mark_with_root_regions(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        reference_objs: &[usize],
    ) {
        self.g1_start_concurrent_mark_inner(stw, reference_objs, true);
    }

    fn g1_start_concurrent_mark_inner(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        reference_objs: &[usize],
        root_regions: bool,
    ) {
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
            if root_regions {
                state
                    .collector
                    .start_concurrent_mark_with_root_regions(stw, reference_objs);
            } else {
                state
                    .collector
                    .start_concurrent_mark_with_reference_skip(stw, reference_objs);
            }
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

    /// gci i12/wb: inside the concurrent-start pause, after
    /// [`Self::g1_mark_roots`], scan every object the takeover's frozen peers
    /// hold (`G1Collector::scan_frozen_held_objects`, which has the argument),
    /// and wake the worker for the grays it pushed. `0` off G1.
    pub fn g1_scan_frozen_held_objects(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        frozen: &[cratonvm_types::ObjectRef],
    ) -> usize {
        if let VmHeap::G1(g1) = self {
            let scanned = g1.scan_frozen_held_objects(stw, frozen);
            if scanned > 0 {
                if let Some(ctrl) = g1.concurrent_mark.lock().as_ref() {
                    ctrl.notify_work_available();
                }
            }
            return scanned;
        }
        0
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
    ///    step 5 cannot free them. (Since gch h4/g1 the VM's callback marks
    ///    that list itself, inside the callback -- `g1_remark_mark_kept_closure`,
    ///    the same `resurrect_after_remark` -- so that its class, loader and
    ///    weak verdicts read the final bitmap; the heap's call here is then a
    ///    no-op kept for any caller that does not.) The predicate is
    ///    [`G1Collector::remark_liveness`]'s view: `is_live_after_mark` with the
    ///    snapshot copied once (gci i22/g1rk). This window — bitmap complete, nothing
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
            // r11w19 census: count the orphans only a rejected band word
            // keeps alive, against the drained closure (see
            // `G1Collector::finish_deferred_pin_seeds`).
            let census = crate::g1::g1_jit_pin_need_census();
            if census {
                state.collector.defer_next_remark_pin_seeds();
            }
            state.collector.remark(stw, roots);
            // g1ep7: on the evacuation pool (`drain_marking_to_fixed_point`).
            state.collector.drain_marking_to_fixed_point();
            if census && state.collector.finish_deferred_pin_seeds() {
                state.collector.drain_marking_to_fixed_point();
            }
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
                // gci i22/g1rk (item 7 of `gch-h4g1-g1-review-findings-FIXED-20261017`):
                // `is_live_after_mark`'s verdict with the mark-start snapshot
                // copied once for the whole callback, instead of its mutex per
                // question (`G1Collector::remark_liveness`; the snapshot cannot
                // change inside this pause). No lock is held between calls.
                let view = collector.remark_liveness();
                let is_live = |addr: usize| view.is_live(addr);
                let resurrect = cb(&is_live);
                drop(view);
                collector.resurrect_after_remark(&resurrect);
            }
            // The collection-overlay rows of every owner this completed mark
            // proves dead go BEFORE cleanup frees their elements (after the
            // resurrection above, whose objects may own rows). G1 marks overlay
            // contents only on behalf of a live owner (`g1_overlay.rs`).
            state.collector.prune_overlays_after_mark();
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
                    // As in `g1_final_remark_and_cleanup`: overlay rows of
                    // owners the mark proves dead go before cleanup frees.
                    state.collector.prune_overlays_after_mark();
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

    /// gci i15/ca (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
    /// item 7's cost half): ABORT the open G1 marking cycle, as HotSpot's full
    /// collection does (`G1ConcurrentMark::concurrent_cycle_abort`): stop and
    /// join the background marker, then
    /// [`G1Collector::abort_concurrent_mark`](crate::g1::G1Collector::abort_concurrent_mark)
    /// -- gray set, overlay snapshot, referent-slot skip set, TAMS rows and
    /// mark-start snapshot dropped, the cycle's trust withdrawn
    /// (`last_mark_trusted`), the phase back to `Idle`, then the SATB queue
    /// closed with every per-thread bucket and shard discarded. No remark, no
    /// reference processing, no cleanup: nothing is freed, and the next cycle
    /// re-marks from its own roots (`start_concurrent_mark_with_reference_skip`
    /// clears every bitmap and bumps the SATB epoch).
    ///
    /// MUST be called inside a stop-the-world pause (the token): every mutator
    /// is parked, so no store can be mid-SATB-log while the queue closes, and no
    /// young pause can run between the join and the abort. The caller is the
    /// VM's full-collection door (`vm/src/runtime/interpreter/gc_and_alloc.rs`
    /// `g1_abort_open_cycle_pause`), which then starts a fresh cycle.
    ///
    /// `true` = a cycle was marking and is now aborted; `false` (nothing done)
    /// off G1 or with no cycle marking (another thread's remark closed it first).
    pub fn g1_abort_concurrent_mark(&self, _stw: &crate::collector::StopTheWorldToken) -> bool {
        if let VmHeap::G1(state) = self {
            if !state.collector.gc_state.is_marking_active() {
                return false;
            }
            // Joined BEFORE the abort: a marker step racing `clear_gray_set` /
            // the TAMS reset could re-gray or account into the state the abort
            // is discarding. Released before the blocking join, as
            // `g1_start_concurrent_mark_with_reference_skip` does.
            let ctrl = state.concurrent_mark.lock().take();
            if let Some(ctrl) = ctrl {
                let steps = ctrl.steps_performed();
                let outcome = ctrl.request_stop_and_join();
                tracing::debug!(
                    "g1_abort_concurrent_mark: marker joined (steps={}, joined_ok={})",
                    steps,
                    outcome.is_ok(),
                );
            }
            state.collector.abort_concurrent_mark();
            return true;
        }
        false
    }

    /// Apply the HotSpot tenuring flags (`-XX:MaxTenuringThreshold`,
    /// `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio`,
    /// `-XX:+PrintTenuringDistribution`) — gen r4w6/young6.
    ///
    /// Honoured in full by the generational backend (adaptive tenuring, see
    /// `gen_heap_tenuring.rs`); G1 takes `MaxTenuringThreshold` and
    /// `TargetSurvivorRatio`, as HotSpot G1 does (gci i23/tn,
    /// `G1Collector::set_tenuring_flags`); ZGC has no tenuring, so for it
    /// this is a no-op and the launcher, which knows the flags were typed,
    /// prints the one-time "ignored on this collector" note. A default
    /// config is a no-op on every backend.
    pub fn set_tenuring_config(&self, cfg: &crate::gen_heap::TenuringConfig) {
        if let VmHeap::Generational(h) = self {
            h.set_tenuring_config(cfg);
        } else if let VmHeap::G1(h) = self {
            // gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`
            // item 5): HotSpot G1 honours `-XX:MaxTenuringThreshold` and
            // `-XX:TargetSurvivorRatio`; `InitialTenuringThreshold` is
            // Parallel's only, and `PrintTenuringDistribution` is not G1's
            // (the launcher still notes that one). Both `None` = no change.
            h.set_tenuring_flags(cfg.max_tenuring_threshold, cfg.target_survivor_ratio);
        }
    }

    /// gcg g3/x: `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio`; Generational,
    /// and G1's sizing policy (gci i13/ef finding 6; read only under
    /// `CRATONVM_G1_HEAP_RESIZE`). ZGC does not read them.
    pub fn set_heap_free_ratios(&self, min: Option<u8>, max: Option<u8>) {
        if let VmHeap::Generational(h) = self {
            h.set_heap_free_ratios(min, max);
        } else if let VmHeap::G1(h) = self {
            h.set_heap_free_ratios(min, max);
        }
    }

    /// Enable GC logging (verbose:gc).
    pub fn enable_gc_logging(&self) {
        match self {
            VmHeap::G1(g1) => g1.enable_gc_logging(),
            // 2026-09-20 (round 2): a plain delegation, exactly like its two
            // neighbours. Until this date this arm turned NOTHING on —
            // `GenerationalHeap` had no logging flag and no gated per-collection
            // statement to flip it for — while logging an affirmative at `info`,
            // which survives a release build (`release_max_level_info`). So
            // `--verbose:gc -XX:+UseGenerationalGC` printed a green light and
            // then produced no per-collection output for the rest of the run.
            // (The *shutdown* census below was always emitted; the missing half
            // was the PER-COLLECTION line, which is what the flag means on
            // HotSpot and what G1 and ZGC emit. The wave-1 write-up overstated
            // this; the measured correction is in
            // `docs/feature-designs/generative-gc-architecture.md`.)
            //
            // `GenerationalHeap::set_verbose_gc` now owns the flag and the
            // per-collection emitter reads it, mirroring `zgc.rs:8007` and
            // `g1.rs:18073`. Gap:
            // `docs/feature-designs/generative-gc-architecture.md`.
            VmHeap::Generational(h) => h.set_verbose_gc(true),
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

    /// Switch GC logging (verbose:gc) on or off at run time -- the VM side of
    /// `MemoryMXBean.setVerbose` (gc-common w18-g). Each backend's own flag,
    /// the same ones `enable_gc_logging` sets.
    pub fn set_gc_logging(&self, on: bool) {
        match self {
            VmHeap::G1(g1) => {
                if on {
                    g1.enable_gc_logging()
                } else {
                    g1.disable_gc_logging()
                }
            }
            VmHeap::Generational(h) => h.set_verbose_gc(on),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                if on {
                    h.enable_gc_logging()
                } else {
                    h.disable_gc_logging()
                }
            }
        }
    }

    /// Emit ZGC's end-of-run metrics summary and TSV row, once per process.
    ///
    /// **Deliberately NOT inside [`VmHeap::print_gc_summary`]**, which the
    /// callers gate on `gc_stats_requested` (`--verbose:gc` /
    /// `CRATONVM_GC_STATS`). The whole point of `CRATONVM_ZGC_METRICS_TSV` is
    /// a machine-readable row a harness can `join`, and requiring a log flag
    /// to get it would reintroduce the log-parsing this replaces.
    ///
    /// It also cannot live where it was: `impl Drop for ZgcRealHeap` does not
    /// run in the shipped VM (the heap is held in an `Arc` until the process
    /// exits), which is why a run with `CRATONVM_ZGC_METRICS=1` printed no
    /// metrics summary at all before 2026-09-21. `ZgcRealHeap::
    /// emit_end_of_run_metrics` claims once per process, so calling this from
    /// both of `vm-cli`'s exit arms is safe.
    ///
    /// A no-op on every other backend and in a build without the `zgc`
    /// feature.
    pub fn emit_zgc_end_of_run_metrics(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeap::Zgc(h) = self {
            // zt t23/x19 (page item 28d): the cycle's end first.
            let _ = h.finish_pending_sweep_at_exit();
            h.emit_end_of_run_metrics();
        }
    }

    /// Print the end-of-run `[GC] …` census to stderr: G1's pause summary
    /// (p50/p99/max young + mixed) and its per-subsystem lines, ZGC's
    /// `[GC] zgc-*` block, the Generational counts and sweep-health lines, and
    /// the cross-backend card/remembered-set report. Called at VM shutdown when
    /// GC stats are requested (`--verbose:gc` or `CRATONVM_GC_STATS`).
    ///
    /// (This doc used to sit on [`Self::emit_zgc_end_of_run_metrics`], merged
    /// into that function's own doc, and claimed the Generational arm was a
    /// no-op — it prints more lines than any other arm.)
    pub fn print_gc_summary(&self) {
        // zt t23/x19 (page item 28d): a sweep still pending at exit is
        // finished first, so the summary (`allocated`, `csw_pending=`) reports
        // the cycle's end. One load when nothing is pending.
        let _ = self.zgc_finish_pending_sweep_at_exit();
        // gcf f4/r (`gengc-r5w4-defaults8-proposal-print-the-effective-heap-policy`):
        // which allocation / heap policies actually ran, each with where its
        // value came from, first, so two runs' logs can be compared after
        // the fact. Rendered from the flag snapshot (`GcFlags::policy_line`).
        let backend = match self {
            VmHeap::Generational(_) => "generational",
            VmHeap::G1(_) => "g1",
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => "zgc",
        };
        let env = |key: &str| {
            cratonvm_types::flags::runtime_var_os(key).map(|v| v.to_string_lossy().into_owned())
        };
        eprintln!(
            "{}",
            crate::gc_flags().policy_line(backend, self.is_generational(), &env)
        );
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
            // zs s3/ac: the collection's CPU by owner, and the process CPU it
            // is a share of, so one run's stderr answers "collection CPU /
            // process CPU" (`ZCollectionCpuTotals` says which shares nest).
            // Outside every `cycles_started > 0` block: a run with no
            // concurrent cycle still has a STW collection CPU to report.
            eprintln!(
                "{}",
                h.collection_cpu_totals().summary_line(
                    crate::zgc::mark::process_cpu_nanos(),
                    h.collection_cpu_accounting_on(),
                )
            );
            // zs s3/ac: the SATB direct hand-off's engagement
            // (`CRATONVM_ZGC_SATB_DIRECT_HANDOFF`); 0 on a run with no
            // concurrent cycle.
            let (satb_direct_handed, satb_direct_on) = h.satb_direct_handoff_report();
            eprintln!(
                "[GC] zgc-satb-direct: handed={satb_direct_handed} switch={}",
                satb_direct_on as u8
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
                            "[GC] zgc-entry:   tlab refills attempted={att} refill_succeeded={ok}; bytes_allocated_total={total} (the wedge break's re-arm, one break per 64 MB)"
                        );
                        let (rt, rok) = cratonvm_types::gc_entry_census::refill_retry_totals();
                        eprintln!(
                            "[GC] zgc-entry:   post-break refill retries={rt} retry_succeeded={rok} (a success seeds the TLAB, whose allocations re-arm the breaker)"
                        );
                        // WHO ZEROES TLAB CHUNKS (round 9 wave 11, `zgc11`).
                        // `pristine_bytes_skipped` had no reader outside a unit
                        // test, so whether the pristine skip did anything in a
                        // run could not be told from its log. Read it against
                        // `vm_tlab_refill_bytes`: a GC-heavy run skips only
                        // up to the arena's first high-water mark, and pays the
                        // `memset` for every chunk carved below it.
                        let (_, refill_bytes, _, _) = h.vm_tlab_engagement();
                        eprintln!(
                            "[GC] zgc-tlab-zero: pristine_bytes_skipped={} slide_zero_bytes_skipped={} tlab_zero_refill_bytes={refill_bytes}",
                            h.tlab_pristine_bytes_skipped(),
                            h.tlab_slide_zero_bytes_skipped(),
                        );
                        // zt t2/al: the opt-in descending chunk zeroing's
                        // engagement, printed only while it is on (the
                        // default line above is unchanged).
                        let (piece, chunks) = h.chunk_zero_descending_stats();
                        if piece != 0 {
                            eprintln!(
                                "[GC] zgc-tlab-zero-descending: piece_bytes={piece} chunks={chunks}"
                            );
                        }
                        // THE TLAB RESERVATION, AGAINST THE BUDGET IT IS
                        // SUPPOSED TO OBEY (applies
                        // `handoff-s-tlab-reserved-peak.md`).
                        //
                        // `reserved` at shutdown is ~0 -- every thread has
                        // retired. The peak is monotone and is still there.
                        //
                        // The two rules do not disagree about the current
                        // reservation; they disagree about the SUM OUTSTANDING
                        // AT THE WORST INSTANT. `ZGC_TLAB_RESERVATION_SHARE` is
                        // a budget in bytes, while the count rule enforces a
                        // per-thread chunk SIZE, so with N threads the sum can
                        // exceed the budget by roughly a factor of N -- and the
                        // window re-opens at every collection, because the count
                        // is rebuilt there.
                        //
                        // Read `peak/budget`: far above 1 is the unbounded
                        // shape the share exists to prevent. (zt t12/rt: the
                        // byte arm, `CRATONVM_ZGC_TLAB_RESERVED_BYTES`, and its
                        // `byte_sizing=` field were retired.)
                        eprintln!(
                            "[GC] zgc-tlab-reserve: reserved={} peak={} budget={}",
                            h.tlab_reserved_bytes(),
                            h.tlab_reserved_bytes_peak(),
                            h.tlab_reservation_budget_bytes(),
                        );
                        // THE PAGE SURVEY -- roadmap C0 stage 0, applying
                        // `handoff-s-page-survey-stage-0.md`. Without this line
                        // `CRATONVM_ZGC_PAGE_SURVEY` is UNFALSIFIABLE, which is
                        // the condition `page.rs` has been in since it was
                        // written: 2,000 lines of allocator whose engagement
                        // could not be told from its absence.
                        //
                        // The whole flag changes no behaviour -- a survey
                        // cannot allocate, free or write a byte -- so the only
                        // thing it can be judged on is whether it saw the same
                        // heap the collector did. Two readings say that:
                        // `grid_refreshes` must equal the collection count, and
                        // `views_rejected` must be ZERO. A non-zero rejection
                        // count is a WIRING DEFECT, not a measurement: it means
                        // the grid was built against a different base than the
                        // survey was, so the survey describes the wrong heap.
                        //
                        // `base_aligned` is recorded rather than enforced.
                        // Stage 0 does not need it; stage 1 does, because a
                        // real page must start on a granule.
                        if let Some(survey) = h.page_survey_stats() {
                            eprintln!(
                                "[GC] zgc-page-survey: grid_refreshes={} \
                                 views_installed={} views_rejected={} \
                                 granules_indexed={} base_aligned={:?} \
                                 survey_collections={}",
                                survey.grid_refreshes,
                                survey.views_installed,
                                survey.views_rejected,
                                survey.granules_indexed,
                                h.page_survey_base_aligned(),
                                h.gc_count(),
                            );
                        }
                    }
                    for (site, n) in cratonvm_types::gc_entry_census::forced_sites() {
                        eprintln!("[GC] zgc-entry:   forced by {site}: {n}");
                    }
                }
                let (stress, threshold, headroom, budget, hard) = h.trigger_tallies();
                eprintln!(
                    "[GC] zgc-trigger: stress={stress} live_bytes_threshold={threshold} headroom_low={headroom} alloc_budget={budget} hard_alloc_refusals={hard} warmup={} director={} director_deferred={}",
                    h.trigger_warmup_count(),
                    // zr zs s1/tp: the director's tally had no reader -- the
                    // rate-trigger rows' "director tally > 0" positive
                    // control could not be read -- and its anti-storm
                    // deferrals (`CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`).
                    h.trigger_director_count(),
                    h.trigger_director_deferred_count(),
                );
                // zr z6/tp: trigger pacing near a full heap -- which rule asked
                // for each collection taken with the live set above the
                // threshold, and the allocation window before it.
                eprintln!("{}", h.pacing_line());
                // WHY THE COLLECTOR DID **NOT** RUN, which the five tallies
                // above cannot say: `headroom_low` is about allocatable space,
                // `gc_rearm` is about live bytes, and on a non-compacting heap
                // those can diverge — so the arena can be unable to serve an
                // allocation while the trigger that exists for exactly that
                // case is held below its floor.
                //
                // What that COSTS is open, and smaller than this round's
                // summary claimed. Measured 2026-09-21 at the wave-4 base
                // commit: every exhaustion shape the orchestrator could
                // construct — including the non-compacting arm, where the
                // divergence is supposed to be unbounded — threw a catchable
                // `OutOfMemoryError` at ~63 MiB of a 64 MiB heap. Near-full,
                // no abort. `starved_intervals` is what settles whether the
                // state occurs at all; see `gap-p-headroom-flip-criteria.md`.
                //
                // `starved` is the counterfactual and is readable on the
                // DEFAULT configuration: it does not need
                // `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM=1` to be set, which is
                // the point — the flip criterion it replaces asked for a
                // reading that only exists in the arm being justified.
                //
                // Printed here with the zeroes included, because in a
                // stats-requested run "this workload never reached the shape"
                // is itself the answer and a suppressed line reads identically
                // to a line nobody looked at. `ZgcRealHeap`'s `Drop` prints the
                // same figures under NO flag when they are non-zero, which is
                // the reading for a run with logging off — the run this counter
                // is actually about.
                let (starved, starved_gap, bypass, suppressed) = h.headroom_flip_evidence();
                eprintln!(
                    "[GC] zgc-headroom: starved_intervals={starved} \
                     starved_max_gap_bytes={starved_gap} bypass_fired={bypass} \
                     bypass_suppressed={suppressed} bypass_enabled={}",
                    h.headroom_trigger_state().1,
                );
                // Every count here is a dropped old->young edge, i.e. a live
                // object freed. It must read 0; see
                // `ZgcCounters::remset_pages_without_base`.
                eprintln!(
                    "[GC] zgc-remset: pages_without_a_base={}",
                    h.remset_pages_without_base(),
                );
                // WHAT THE PAUSE TARGET IS ACTUALLY MISSING BY, which decides
                // `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE`.
                //
                // The p99 here is over the WHOLE cycle -- every phase guard,
                // including the relocation slide -- and it is measured the same
                // way in both arms of that flag, because it comes from the
                // phase guards and not from the `gc_started` clock the flag
                // moves. So it is the honest pause in a run that believes the
                // pre-slide figure, which is exactly what an A/B needs and what
                // no line printed before: `zgc-relocate` gives one cycle's
                // `pause_with_relocate_us` and a percentile over it needed the
                // whole log post-processed.
                //
                // Read it against `target_ms`: a p99 above the target in the
                // `=0` arm is the default-on control loop failing to deliver
                // what it promises while reporting success, because the number
                // it was fed excluded about two thirds of the pause. See
                // `ZgcMetrics::cycle_pause_distribution` and
                // `gap-p-pause-target-includes-relocate-default.md`.
                let (target_ms, _span, budget, unreachable) = h.pause_target_state();
                let (sampled, p50_ns, p99_ns, max_ns) = h.metrics().cycle_pause_distribution();
                eprintln!(
                    "[GC] zgc-pause-dist: target_ms={target_ms} cycles_sampled={sampled} \
                     p50_us={} p99_us={} max_us={} alloc_trigger_bytes={budget} \
                     target_unreachable_cycles={unreachable}",
                    p50_ns / 1_000,
                    p99_ns / 1_000,
                    max_ns / 1_000,
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
            // zt t12/rt: `targets_recorded=` / `targets_consumed=` left this
            // line with the retired `CRATONVM_ZGC_TARGETED_COMPACTION`; nothing
            // names a target page any more, so `targeted_pages=` reads 0.
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
            // IS THE EVACUATION BUDGET BINDING ON THIS RUN?
            //
            // `ZRelocationSet::select` is a prefix rule over
            // `max_evacuation_bytes` (64 MiB of live bytes by default,
            // `CRATONVM_ZGC_RELOCATE_BUDGET_MB` to change it), so a cycle that
            // compacted a prefix of the heap and stopped reports one
            // `compaction_cycles`, a large `objects_relocated` and no skip
            // reason -- identical to a cycle that finished. On this collector
            // compaction is the only defragmentation there is, so the two
            // readings point at opposite repairs for a fragmentation
            // `OutOfMemoryError`: "the slide never ran" wants the coverage
            // proof, "the slide ran and stopped early" wants the constant.
            //
            // `reloc_budget_truncated_cycles=0` is the finding that the
            // constant is NOT binding here, and no A/B of its value can
            // measure anything on this workload.
            let (reloc_pages_deferred, reloc_budget_truncated_cycles) =
                h.relocation_budget_engagement();
            eprintln!(
                "[GC] zgc-features: parallel_mark_cycles={par_cycles} \
                 driver_passes={driver_passes} mark_fallbacks={mark_fallbacks} \
                 compaction_cycles={compactions} objects_relocated={relocated} \
                 relocation_skipped_jit={skipped_jit} \
                 relocation_on_proven_jit={proven_jit} \
                 tlab_retire_skipped={tlab_skipped} tlab_retire_skipped_at_safepoint={tlab_retire_skipped_at_safepoint} targeted_pages={targeted_pages} tlab_recycled_refills={recycled_refills} tlab_starved_refills={starved_refills} tlab_starved_bytes={starved_bytes} \
                 vm_tlab_refills={vm_tlab_refills} vm_tlab_refill_bytes={vm_tlab_refill_bytes} \
                 vm_tlab_tails_returned={vm_tlab_tails} vm_tlab_tail_bytes_returned={vm_tlab_tail_bytes} \
                 vm_tlab_tail_recharge_bytes={tail_recharge_bytes} \
                 reloc_pages_deferred={reloc_pages_deferred} \
                 reloc_budget_truncated_cycles={reloc_budget_truncated_cycles}",
                targeted_pages = h.targeted_pages_selected(),
                // zr z1/al residual: published tails a collection kept charged
                // (`ZgcRealHeap::recharge_published_tails`); 0 on a run whose
                // collections froze nobody holding a VM TLAB.
                tail_recharge_bytes = h.published_tail_recharge_bytes(),
                tlab_retire_skipped_at_safepoint = h.tlab_retire_skipped_at_safepoint(),
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
            // WHICH OBLIGATION FAILED, independently of whether it stopped the
            // slide. The loop above counts refusals that took effect; this one
            // counts the verdicts, including the four terms the
            // `compiled_frames_live` gate computes and discards. A term that
            // appears here with no matching `zgc-relocation-skip-reason:` line
            // fired on every cycle it was asked about and stopped none of them.
            //
            // One code is structurally absent here and that is not a gap:
            // `tlab-retire-incomplete-at-safepoint` is an earlier unconditional
            // refusal that returns before this census.
            let refusal_terms = h.relocation_refusal_term_counts();
            for (reason, n) in refusal_terms.iter().enumerate() {
                if *n > 0 {
                    eprintln!(
                        "[GC] zgc-relocation-refusal-term: {}={} (stopped_a_slide={})",
                        crate::zgc::relocation_skip_reason::label(reason),
                        n,
                        skip_reasons.get(reason).copied().unwrap_or(0),
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
                    "[GC] zgc-unregistered-jit-frame: hits_above_residue_mark={residue_live} hits_explained_by_residue={residue_explained} (the second kind marks and pins the band but no longer refuses relocation; CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0 restores the refusal)"
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
                "[GC] zgc-high-compaction: high_cycles={hi_cycles} high_declined={hi_declined} high_objects_relocated={hi_moved} high_bytes_copied={hi_bytes} high_vacated_spans={vac_spans} high_vacated_bytes={vac_bytes}"
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
                "[GC] zgc-slide-verify: slides_verified={sv_runs} survivors_walked={sv_survivors} missed_rewrites={sv_missed} unregistered_targets={sv_unreg} slide_verify_enabled={}",
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
            // ...and the WINDOW FIT, which is the one number
            // `proposal-a-concurrent-mark-by-default.md` is gated on.
            //
            // `scanned_at_safepoint / scanned_concurrently` below **0.1** is
            // that proposal's step-3 acceptance bar: the fraction of the mark
            // that the concurrent window was too short to absorb, and which
            // therefore landed inside the stop-the-world pause concurrent
            // marking exists to shorten. Per-cycle figures are on
            // `[GC] zgc-markend:`; these are the sums, so the question is a
            // single run rather than a log-parsing session.
            //
            // Read `scanned_concurrently=0` as "no concurrent cycle completed
            // -- this run says nothing about the bar", not as a pass.
            // `window_target_ms=0` means the adaptive sizing had no opinion (a
            // fixed `CRATONVM_ZGC_CONC_START`, or no allocation rate sampled).
            //
            // Printing it is NOT a claim that the bar is met, and the default
            // is unchanged: `CRATONVM_ZGC_CONC_START=0`.
            let (scanned_conc, scanned_stw, window_target_ms) = h.concurrent_window_fit();
            // zr z7/cm: `satb_filtered` = SATB records dropped because the old
            // value was already marked (`CRATONVM_ZGC_SATB_FILTER_MARKED`);
            // `satb_filter` / `root_filter_conc` = the two concurrent-cost
            // switches as this heap runs them.
            let (satb_filtered, satb_filter_on, root_filter_conc) = h.concurrent_cost_report();
            // zt t7/cc: the concurrent overlay repair's engagement
            // (`CRATONVM_ZGC_CONC_OVERLAY_REPAIR`), summed over cycles.
            let (ov_rows, ov_refs) = h.concurrent_overlay_repair_totals();
            eprintln!(
                "[GC] zgc-concurrent: cycles_started={started} cycles_completed={completed} \
                 black_allocations={black} satb_replayed={replayed} \
                 concurrent_phase_ms={} scanned_concurrently={scanned_conc} \
                 scanned_at_safepoint={scanned_stw} window_target_ms={window_target_ms} \
                 satb_filtered={satb_filtered} satb_filter={} root_filter_conc={} \
                 overlay_repair_rows_total={ov_rows} overlay_repair_refs_total={ov_refs}",
                phase_nanos / 1_000_000,
                satb_filter_on as u8,
                root_filter_conc as u8,
            );
            // zr z3/cs: `CRATONVM_ZGC_CONC_EVERY_TRIGGER`. `ct_opened` cycles
            // were opened on a trigger's request; each ends `ct_finished` (the
            // collection waited for the mark's fixed point) or
            // `ct_abandoned_stw` (closed before it: `ct_urgent` of those at
            // the urgent reserve, the rest by another door -- allocation
            // failure, `System.gc()`). `ct_lapsed` requests never opened
            // (`ct_start_refusals` of them refused by the take-over cover);
            // `ct_no_headroom` early triggers collected stop-the-world because
            // the heap could not finish a mark. All zero with the switch off.
            let ct = h.conc_trigger_report();
            eprintln!(
                "[GC] zgc-conc-trigger: every_trigger={} ct_requests={} ct_opened={} \
                 ct_finished={} ct_abandoned_stw={} ct_urgent={} ct_lapsed={} \
                 ct_start_refusals={} ct_no_headroom={} ct_lost_races={} \
                 ct_start_rechecks={} ct_sysgc={} ct_sysgc_requests={} \
                 ct_sysgc_opened={} ct_sysgc_served={} ct_sysgc_lost={} \
                 ct_sysgc_not_opened={} ct_sysgc_held={}",
                ct.enabled as u8,
                ct.requests,
                ct.opened,
                ct.finished,
                ct.abandoned,
                ct.urgent,
                ct.lapsed,
                ct.start_refusals,
                ct.no_headroom,
                // zr zs s1/tp: open-race losers that paid a mark-start pause
                // anyway, and the starts dropped before requesting one.
                ct.lost_races,
                ct.start_rechecks,
                // zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): requests
                // that asked for a cycle of their own, cycles that opened, the
                // ones the request's own collection closed with their mark set
                // (`served`) or another door closed first (`lost`), mark
                // starts that did not open, and mark-end polls held for the
                // waiting caller.
                ct.sysgc_enabled as u8,
                ct.sysgc_requests,
                ct.sysgc_opened,
                ct.sysgc_served,
                ct.sysgc_lost,
                ct.sysgc_not_opened,
                ct.sysgc_held,
            );
            // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): cycles
            // whose sweep left the pause (`csw_deferred`), cycles the screen
            // kept in it (`csw_refused`, the last reason's code), who ran each
            // parked sweep, the allocations that waited for one, and the
            // cumulative microseconds of the deferred pauses, the walks, the
            // installs and the release-to-walk waits. All zero with the switch
            // off.
            let cs = h.concurrent_sweep_report();
            eprintln!(
                "[GC] zgc-concurrent-sweep: csw_enabled={} csw_deferred={} csw_refused={} \
                 csw_last_refusal={} csw_by_initiator={} csw_by_allocation={} \
                 csw_by_cycle_start={} csw_by_mark_start={} csw_by_waiter={} \
                 csw_alloc_retries={} csw_pause_us={} csw_walk_us={} csw_install_us={} \
                 csw_wait_us={} csw_pending={} csw_by_exit={}",
                cs.enabled as u8,
                cs.deferred,
                cs.refused,
                cs.last_refusal,
                cs.by_initiator,
                cs.by_allocation,
                cs.by_cycle_start,
                cs.by_mark_start,
                cs.by_waiter,
                cs.alloc_retries,
                cs.pause_us,
                cs.walk_us,
                cs.install_us,
                cs.wait_us,
                cs.pending as u8,
                // zt t23/x19 (page item 28d): sweeps the VM's exit ran.
                cs.by_exit,
            );
            // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in): collections
            // whose referent slots were hidden instead of nulled
            // (`crf_skip_cycles`, rows `crf_skip_rows`, slots the pause cleared
            // `crf_stw_cleared`), collections generational mode refused, and the
            // census of the VM's reference round (recorded with the switch off
            // too): rounds, cumulative microseconds of the pre-collection pass,
            // `process_references_after_gc` and the JNI weak sweep, and the
            // longest round.
            let cr = h.concurrent_refs_report();
            eprintln!(
                "[GC] zgc-concurrent-refs: crf_enabled={} crf_skip_cycles={} crf_skip_rows={} \
                 crf_stw_cleared={} crf_refused={} crf_rounds={} crf_pre_us={} crf_post_us={} \
                 crf_jni_us={} crf_max_round_us={} crf_deferred_clears={} \
                 crf_after_release_cleared={} crf_gate_blocked={}",
                cr.enabled as u8,
                cr.skip_cycles,
                cr.skip_rows,
                cr.stw_cleared,
                cr.refused,
                cr.rounds,
                cr.pre_us,
                cr.post_us,
                cr.jni_us,
                cr.max_round_us,
                // zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): collections
                // that cleared after the release, the slots nulled there, and
                // mutator referent reads the gate answered `null`.
                cr.deferred_clears,
                cr.after_release_cleared,
                cr.gate_blocked,
            );
            // zt t23/zc6 (stage 1f): the concurrent cycles' closing pauses --
            // certified closes, closes whose pool had already reached its
            // fixed point, closes that fell back to a stop-the-world mark, the
            // addresses those pauses pushed and newly claimed, what the phase
            // marked / scanned against what the pauses did, the pauses' drive
            // time -- and the mark-end service (`CRATONVM_ZGC_MARK_END_SERVICE`,
            // opt-in): its looks at an open cycle and its closing attempts.
            let me = h.mark_end_report();
            eprintln!(
                "[GC] zgc-markend-census: mec_service={} mec_closed={} mec_caught_up_at_entry={} \
                 mec_fallbacks={} mec_pushed={} mec_claimed={} mec_conc_marked={} \
                 mec_end_marked={} mec_conc_scanned={} mec_end_scanned={} mec_end_us={} \
                 mec_end_max_us={} mec_svc_ticks={} mec_svc_closes={} mec_svc_lost={} \
                 mec_svc_stale={} mec_svc_extra={}",
                me.service as u8,
                me.closed,
                me.caught_up_at_entry,
                me.fallbacks,
                me.pushed,
                me.claimed,
                me.conc_marked,
                me.end_marked,
                me.conc_scanned,
                me.end_scanned,
                me.end_us,
                me.end_max_us,
                me.svc_ticks,
                me.svc_closes,
                me.svc_lost,
                me.svc_stale,
                me.svc_extra,
            );
            // zt t24/zc8 (stage 1f, `CRATONVM_ZGC_MARK_CONTINUE`, opt-in):
            // Concurrent Mark Continue -- the Pause Mark End attempts, their
            // verdicts, the closes that drained in the pause instead (the
            // cycle's attempts used up, the urgent reserve), what the attempts
            // handed the pool and newly claimed, the failed pauses' and the
            // continue phases' time, and the service attempts that released.
            let mc = h.mark_continue_report();
            eprintln!(
                "[GC] zgc-markcontinue: mcc_enabled={} mcc_attempts={} mcc_continues={} \
                 mcc_completes={} mcc_exhausted={} mcc_urgent={} mcc_pushed={} mcc_claimed={} \
                 mcc_pause_us={} mcc_continue_us={} mcc_max_cycle_continues={} \
                 mcc_svc_continued={} cpb_forget={} cpb_forgets={}",
                mc.enabled as u8,
                mc.attempts,
                mc.continues,
                mc.completes,
                mc.exhausted,
                mc.urgent,
                mc.pushed,
                mc.claimed,
                mc.pause_us,
                mc.continue_us,
                mc.max_cycle_continues,
                mc.svc_continued,
                // Stage 0 CPU: concurrent closes that dropped the pause-target
                // allocation budget (`CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET`).
                mc.budget_forget as u8,
                mc.budget_forgets,
            );
            // zt t26/zc9 (stage 1a, `CRATONVM_ZGC_CONCURRENT_RATE_START`,
            // opt-in): every-trigger requests made at HotSpot's
            // allocation-rate point, the cycles that opened on one, and the
            // rule's lapsed requests dropped instead of collected.
            let rs = h.rate_start_report();
            eprintln!(
                "[GC] zgc-rate-start: crs_enabled={} crs_requests={} crs_opened={} crs_dropped={}",
                rs.enabled as u8, rs.requests, rs.opened, rs.dropped,
            );
            // zt t27/zc10 (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`, default ON):
            // the referent-skip snapshots the member-class table was filled
            // for, the ones an unregistered member switched it off, and the
            // last fill's members and distinct class bits.
            let sk = h.skip_class_filter_report();
            eprintln!(
                "[GC] zgc-skip-class-filter: skc_enabled={} skc_fills={} skc_off={} \
                 skc_members={} skc_classes={}",
                sk.enabled as u8, sk.fills, sk.off, sk.members, sk.classes,
            );
            // zt t31/zc11 (`CRATONVM_ZGC_MARK_SKIP_BITS`, default ON): the
            // snapshots the member bitmap answered for, the members it set
            // (each once per cycle), the last cycle's members and the bits the
            // cycles' ends cleared.
            // zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): the reference
            // rounds run after the release, the deferring cycles that kept
            // theirs in the pause (and the last reason), the rounds' time and
            // the sweep runners that waited for one.
            let rr = h.concurrent_ref_round_report();
            eprintln!(
                "[GC] zgc-concurrent-ref-round: crr_enabled={} crr_rounds={} crr_refused={} \
                 crr_last_refusal={} crr_round_us={} crr_max_round_us={} crr_hold_waits={}",
                rr.enabled as u8,
                rr.rounds,
                rr.refused,
                rr.last_refusal,
                rr.round_us,
                rr.max_round_us,
                rr.hold_waits,
            );
            // zt t31/zc11 (`CRATONVM_ZGC_REMAP_IN_MARK`, R4): the slides that
            // left healed slots stale, the refusals (and the last reason), the
            // slots skipped, the markers' heals (and those that forwarded), the
            // load barrier's and the SATB pre-barrier's forwards.
            let rm = h.remap_in_mark_report();
            eprintln!(
                "[GC] zgc-remap-in-mark: rim_enabled={} rim_engaged_slides={} rim_refused={} \
                 rim_last_refusal={} rim_skipped={} rim_healed={} rim_forwarded={} \
                 rim_barrier_forwarded={} rim_satb_forwarded={} rim_gen_engaged={} \
                 rim_kept_past_young={}",
                rm.enabled as u8,
                rm.engaged_slides,
                rm.refused,
                rm.last_refusal,
                rm.skipped,
                rm.healed,
                rm.forwarded,
                rm.barrier_forwarded,
                rm.satb_forwarded,
                rm.gen_engaged,
                rm.kept_past_young,
            );
            // zt t32 (`CRATONVM_ZGC_CONCURRENT_RELOCATE`, R6): the closing pauses
            // that planned instead of sliding, the refusals (and the last
            // reason), the regions and objects planned, and who copied them:
            // the pause (eager), a pointer-map lookup after the release, the
            // barrier / marker / SATB forward, the drain; the copiers that
            // waited for another's claim; the bytes the releases freed. zt t34:
            // the plans made from the mark's liveness and those that sized the
            // live set instead, the selected regions the exact re-read took
            // out, the born-black objects scanned for legacy holders (and
            // found), and the liveness check (plans, under-estimated regions,
            // missed holders; both 0).
            let cr = h.concurrent_relocate_report();
            eprintln!(
                "[GC] zgc-concurrent-relocate: crl_enabled={} crl_cycles={} crl_refused={} \
                 crl_last_refusal={} crl_regions={} crl_trimmed={} crl_planned={} crl_planned_bytes={} \
                 crl_eager={} crl_hit={} crl_barrier={} crl_drain={} crl_waits={} \
                 crl_released_bytes={} crl_release_kept={} crl_deferred={} \
                 crl_legacy_holders={} crl_walk_checked={} crl_walk_missed={}/{}/{} \
                 crl_mark_plans={} crl_mark_fallbacks={} crl_exact_dropped={} \
                 crl_black_scanned={} crl_black_legacy={} crl_liveness_check={}/{}/{} \
                 crl_dead={}/{}/{} crl_from_writes={}",
                cr.enabled as u8,
                cr.cycles,
                cr.refused,
                cr.last_refusal,
                cr.regions,
                cr.trimmed,
                cr.planned,
                cr.planned_bytes,
                cr.eager,
                cr.hit,
                cr.barrier,
                cr.drain,
                cr.waits,
                cr.released_bytes,
                cr.release_kept,
                cr.deferred,
                cr.legacy_holders,
                cr.walk_checked,
                cr.walk_missed[0],
                cr.walk_missed[1],
                cr.walk_missed[2],
                cr.mark_plans,
                cr.mark_fallbacks,
                cr.exact_dropped,
                cr.black_scanned,
                cr.black_legacy,
                cr.check[0],
                cr.check[1],
                cr.check[2],
                cr.dead_check[0],
                cr.dead_check[1],
                cr.dead_check[2],
                cr.from_writes,
            );
            // zt t32 (`CRATONVM_ZGC_LOAD_BARRIER_MARKING`, 2f): armed, and the
            // store side's deletions it did not log (colored) and did (plain).
            let (lbm, lbm_skipped, lbm_logged) = h.load_barrier_marking_report();
            eprintln!(
                "[GC] zgc-lbm: lbm_armed={} lbm_colored_skipped={} lbm_plain_logged={}",
                lbm as u8, lbm_skipped, lbm_logged,
            );
            let sb = h.skip_bits_report();
            eprintln!(
                "[GC] zgc-skip-bits: skb_enabled={} skb_fills={} skb_members={} \
                 skb_last_members={} skb_cleared={} skb_withdrawn={}",
                sb.enabled as u8,
                sb.fills,
                sb.members,
                sb.last_members,
                sb.cleared,
                sb.withdrawn,
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
                "[GC] zgc-nursery-trigger: fired={nursery_fired} budget_bytes={nursery_budget} promotions_by_slide={} \
                 overshoot_max={} nursery_warmup_held={}",
                h.promotions_by_slide(),
                // BESIDE THE BUDGET, because it is meaningless without it: this
                // is how far past `budget_bytes` the nursery got before a
                // safepoint arrived, and it prices "a hard ceiling rather than a
                // trigger" -- an open item that has had no number attached. See
                // `ZgcRealHeap::gen_nursery_overshoot_max`.
                h.nursery_overshoot_max(),
                // zt t20/gn7 (D7): `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP`'s
                // engagement.
                h.nursery_warmup_held(),
            );
            // ...AND THE ONE NUMBER THAT DECIDES THE YOUNG-SPACE PROGRAMME.
            //
            // `survivor_page_pct` is the verdict on
            // `docs/feature-designs/zgc-architecture.md`,
            // and the rule was written down before the run: **90 or above
            // refutes it** (nine nursery pages in ten hold a survivor, so a
            // page-granular young generation has almost nothing to release),
            // 60 or below supports it, in between is inconclusive.
            //
            // READ THE TWO ENGAGEMENT FIELDS BEFORE THE VERDICT. This is the
            // difference between refuting the young-space programme and
            // refuting nothing, and `survivor_cycles` alone cannot tell them
            // apart -- it reads `0` for three different situations.
            //
            //   survivor_cycles_observed=0      the census never ran. A fact
            //                                   about the WIRING; this run says
            //                                   nothing about the heap.
            //   survivor_geometry_rejected>0    a young cycle handed the census
            //                                   a frame that does not describe
            //                                   a nursery (an unset young
            //                                   floor). Not evidence; there is
            //                                   a `cratonvm::gc::guard` error
            //                                   naming the frame.
            //   survivor_cycles<3               not enough evidence yet -- the
            //                                   first young cycle after a
            //                                   whole-heap collection sees a
            //                                   nursery that has only just
            //                                   started filling.
            //
            // Past those, `survivor_page_pct >= 90` refutes and `<= 60`
            // supports. `survivor_pages=0/0` means the nursery was empty on
            // every cycle, not that every page was releasable.
            //
            // It is a bit per page rather than a survival RATE on purpose: a
            // nursery can be 1% live and still have a survivor on every page,
            // which is exactly the case that defeats page-granular reclamation
            // and exactly the case a byte-ratio calls healthy.
            //   survivor_pct_last - survivor_pct_first large
            //                                   the census RATCHETED. The
            //                                   nursery floor moves only at a
            //                                   major, and promotion on this
            //                                   backend is a header label
            //                                   rather than a copy, so a
            //                                   promoted object stays inside
            //                                   the nursery's address range
            //                                   and a page that held a
            //                                   survivor holds it for every
            //                                   later young cycle of the same
            //                                   major epoch. Later cycles also
            //                                   examine more pages, so they
            //                                   dominate the sum. A
            //                                   `survivor_page_pct >= 90`
            //                                   built on a climbing series
            //                                   refutes NOTHING. Read
            //                                   `survivor_pct_first` -- the
            //                                   one cycle whose nursery holds
            //                                   only what was allocated since
            //                                   the floor last moved.
            //
            // That fourth rule is not hypothetical and it is not a caveat on a
            // number nobody used: `proposal-d-real-young-space.md` was marked
            // REFUTED on this aggregate and the refutation was propagated to
            // the roadmap and the round summary.
            // `gc/tests/zgc_s_nursery_census_ratchet.rs` exhibits a workload
            // with a constant 1-in-5 MARGINAL survivor rate -- which would
            // strongly SUPPORT the proposal -- that this census scores
            // `Refuted`. See `handoff-s-nursery-census-per-cycle.md`.
            let surv = h.nursery_survivor_report();
            eprintln!(
                "[GC] zgc-nursery: sweep_skipped={skipped} floor={floor} \
                 old_live_bytes={old_live} \
                 survivor_pages={}/{} \
                 survivor_page_pct={} survivor_cycles={} \
                 survivor_cycles_observed={} \
                 survivor_geometry_rejected={} \
                 survivor_pct_first={} survivor_pct_last={} \
                 survivor_pct_range={}-{} survivor_ratchet_margin={}",
                surv.pages_with_survivor,
                surv.pages,
                surv.survivor_page_percent(),
                surv.cycles,
                surv.cycles_observed,
                surv.geometry_rejected,
                surv.first_cycle_percent,
                surv.last_cycle_percent,
                surv.min_cycle_percent,
                surv.max_cycle_percent,
                surv.ratchet_margin(),
            );
            // zr z1/gn: the old-start bitmap (`CRATONVM_ZGC_GEN_OLD_BITMAP`) and
            // the young-aged sweep it enables. `premarks=0` with
            // `young_cycles>0` and `enabled=1` means every young cycle fell
            // back to the header walk (`fallbacks`); `verify_unsound` must be 0
            // under `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1`.
            let [ob_on, ob_valid, ob_pre, ob_fb, ya_sweeps, bf_visited, bf_freed, v_unsound, v_imprecise] =
                h.gen_old_bits_stats();
            // zr z3/rv3: `young_refusals` = would-be young cycles run whole-heap
            // because the bitmap was on but not usable
            // (`CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR`); with it on,
            // `fallbacks` stays 0.
            let ob_refused = h.gen_old_bits_young_refusals();
            eprintln!(
                "[GC] zgc-gen-old-bits: old_bits_enabled={ob_on} valid={ob_valid} premarks={ob_pre} \
                 fallbacks={ob_fb} young_refusals={ob_refused} young_aged_sweeps={ya_sweeps} \
                 below_floor_visited={bf_visited} below_floor_freed_bytes={bf_freed} \
                 verify_unsound={v_unsound} verify_imprecise={v_imprecise}"
            );
            // zr z2/g2: WHERE A GENERATIONAL RUN'S COLLECTOR TIME WENT, by cycle
            // kind (`minor_*` / `major_*`), summed over the run. Filled only on
            // a generational heap when the pause clock ran (`--verbose:gc`), so
            // a quiet or non-generational run prints zeroes. `remset_us` and
            // `census_us` are parts of `mark_us` and `sweep_us`. Read
            // `minor_cycles` and `major_cycles` against the default collector's
            // cycle count first: more cycles and dearer cycles are different
            // fixes. `inline_announce_blocked` is the positive control for
            // `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE` (`-` = table unpublished).
            {
                let (
                    y,
                    m,
                    [
                        yc_sweeps,
                        yc_refusals,
                        yc_last,
                        majors_growth,
                        granules,
                        converted,
                        majors_request,
                    ],
                ) = h.gen_cost_report();
                let row = |p: &str, r: &[u64; crate::zgc::GEN_COST_COLUMNS]| {
                    use crate::zgc::*;
                    format!(
                        "{p}_cycles={} {p}_pause_us={} {p}_pre_us={} {p}_mark_us={} \
                         {p}_remset_us={} {p}_refs_us={} {p}_sweep_us={} {p}_census_us={} \
                         {p}_relocate_us={} {p}_swept={} {p}_dead={} {p}_moved={} \
                         {p}_stream_us={} {p}_sweep_walk_us={}",
                        r[GEN_COST_CYCLES],
                        r[GEN_COST_PAUSE_US],
                        r[GEN_COST_PRE_US],
                        r[GEN_COST_MARK_US],
                        r[GEN_COST_REMSET_US],
                        r[GEN_COST_REFS_US],
                        r[GEN_COST_SWEEP_US],
                        r[GEN_COST_CENSUS_US],
                        r[GEN_COST_RELOCATE_US],
                        r[GEN_COST_SWEPT],
                        r[GEN_COST_DEAD],
                        r[GEN_COST_MOVED],
                        r[GEN_COST_STREAM_US],
                        r[GEN_COST_SWEEP_WALK_US],
                    )
                };
                let blocked = match h.jit_inline_announce_blocked_report() {
                    Some(b) => (b as u8).to_string(),
                    None => "-".to_string(),
                };
                // zt t6/tc (GN-17): `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS`'s
                // engagement counters.
                let (ys_sharded, ys_shards) = h.gen_young_sweep_shard_report();
                // zt t6/tc (GN-5): `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES`'s.
                let tenures = h.gen_tenure_all_sweeps();
                // zt t7/tg (GN-12): the weak-minor census and the opt-in
                // `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR`'s engagement.
                let (weak_minors, weak_run, weak_escalations) = h.gen_weak_minor_report();
                // zt t7/cc: `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`'s
                // engagement (`young_refs_strong_cycles`), its wrong
                // predictions and the finalizables it deferred to a major.
                let (yr_cycles, yr_mispredicted, yr_fin) = h.gen_young_refs_strong_report();
                // zt t11/gn3: `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES`'s engagement.
                let majors_rule_cause = h.gen_majors_by_rule_cause();
                // zt t14/gn5: `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION`'s.
                let empty_skipped = h.gen_empty_minor_escalations_skipped();
                eprintln!(
                    "[GC] zgc-gen-cost: {} {} complement_sweeps={yc_sweeps} \
                     complement_refusals={yc_refusals} complement_last_refusal={yc_last} \
                     majors_by_old_growth={majors_growth} element_granules_scanned={granules} \
                     element_granules_carded={converted} inline_announce_blocked={blocked} \
                     majors_by_request={majors_request} \
                     young_sweep_sharded={ys_sharded} young_sweep_shards={ys_shards} \
                     system_gc_tenures={tenures} weak_minors={weak_minors} \
                     weak_minor_max_run={weak_run} weak_minor_escalations={weak_escalations} \
                     young_refs_strong_cycles={yr_cycles} \
                     young_refs_mispredicted={yr_mispredicted} young_fin_deferred={yr_fin} \
                     majors_by_rule_cause={majors_rule_cause} \
                     empty_minor_escalations_skipped={empty_skipped}",
                    row("minor", &y),
                    row("major", &m),
                );
            }
            // zr z1/gn: the AGE-FILTERED nursery census
            // (`CRATONVM_ZGC_NURSERY_CENSUS_AGE=1`): the same instrument as the
            // line above, counting only survivors that were young when their
            // cycle began. Read `observed` first, then `first`.
            //
            // zr z2/g2: every key carries a `young_` prefix (and `enabled` is
            // `age_census_enabled`) -- the z1 line reused `[GC] zgc-nursery:`'s
            // keys verbatim, which `every_gc_summary_key_is_unique_across_the_
            // whole_summary` refuses (a `grep survivor_pct_first | tail -1`
            // picked whichever line printed last).
            let ys = h.nursery_survivor_young_report();
            eprintln!(
                "[GC] zgc-nursery-young: age_census_enabled={} young_survivor_pages={}/{} \
                 young_survivor_page_pct={} young_survivor_cycles={} \
                 young_survivor_cycles_observed={} young_survivor_geometry_rejected={} \
                 young_survivor_pct_first={} young_survivor_pct_last={} \
                 young_survivor_pct_range={}-{} young_survivor_ratchet_margin={}",
                h.nursery_census_age_enabled() as u8,
                ys.pages_with_survivor,
                ys.pages,
                ys.survivor_page_percent(),
                ys.cycles,
                ys.cycles_observed,
                ys.geometry_rejected,
                ys.first_cycle_percent,
                ys.last_cycle_percent,
                ys.min_cycle_percent,
                ys.max_cycle_percent,
                ys.ratchet_margin(),
            );
            // WHAT THE YOUNG SWEEP STOPPED DOING PER DEAD OBJECT.
            //
            // Read the two together and against `young_cycles` above.
            // `zero_bytes_skipped=0` with `young_cycles>0` means every dead
            // object was still memset in full, so `CRATONVM_ZGC_SWEEP_HEADER_ZERO`
            // is on and inert; `dead_runs == dead_objects` means no two dead
            // objects were ever adjacent, so the run merge is. Neither number
            // says it alone -- a small `dead_runs` is equally consistent with a
            // cycle that found almost no garbage.
            let (zero_skipped, dead_runs, dead_objects) = h.gen_sweep_cost_stats();
            eprintln!(
                "[GC] zgc-sweep-cost: zero_bytes_skipped={zero_skipped} \
                 dead_runs={dead_runs} dead_objects={dead_objects}",
            );
            // zr z1/al: the engagement counters of the allocation / sweep
            // switches, so a host row has a positive control and a kill-switch
            // control to read. `forget_*`: `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS`
            // (passes + declined = collections; all declined = the switch is
            // off or every cycle took a peer over). `inflight_*`:
            // `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT` (tails lowered, bytes added
            // to the withheld set -- its whole cost). `oversize_*`:
            // `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS` (dead > 256 MiB arrays
            // reclaimed from the large-object end; `refused` = still retained).
            let (fp, fd, fb) = h.forget_block_stats();
            let (il, ib) = h.inflight_cover_stats();
            let (os, osb, orf) = h.oversize_array_sweep_stats();
            // `dead_headers_skipped`: `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`
            // (default ON since zr z8/zb); 0 on a collecting run means `=0`
            // or the corpse forensics turned the store back on.
            let dhs = h.sweep_dead_headers_skipped();
            // `huge_extent_hits` (zr z2/rv): interior probes the huge-extent
            // table answered -- derived pointers more than 256 MiB into a live
            // array, which the bounded scan alone resolved to nothing.
            let hx = h.huge_extent_hits();
            // `huge_extent_reseeds` (zr z4/ab, `CRATONVM_ZGC_HUGE_EXTENT_RESEED`;
            // zt t2/rv: it was on no line, so the reseed row had no visible
            // engagement counter): prunes that rebuilt an overflowed table.
            let hxr = h.huge_extent_reseeds();
            eprintln!(
                "[GC] zgc-alloc-sweep: forget_passes={fp} forget_declined={fd} \
                 blocks_forgotten={fb} inflight_tails_lowered={il} \
                 inflight_cover_bytes={ib} oversize_arrays_swept={os} \
                 oversize_bytes_swept={osb} oversize_refused={orf} \
                 dead_headers_skipped={dhs} huge_extent_hits={hx} \
                 huge_extent_reseeds={hxr}",
            );
            // zr z8/st: `CRATONVM_ZGC_LAZY_TLAB_STARTS` (default off; every
            // figure 0 and `lz_mode=off` then). `lz_unregistered_found` is the
            // walk's whole disagreement with the per-allocation bit (0 expected
            // apart from dead objects whose announce failed); `lz_q_own` /
            // `lz_q_peer` are oracle hits inside a live chunk (the proposal's
            // refutation census); `lz_q_mismatch` must read 0.
            let lz = h.lazy_tlab_starts_report();
            eprintln!(
                "[GC] zgc-lazy-starts: lz_mode={} lz_chunks_live={} lz_retire_walks={} \
                 lz_stw_walks={} lz_objects_walked={} lz_already_registered={} \
                 lz_unregistered_found={} lz_walk_stops={} \
                 lz_chunks_dropped={} lz_q_own={} lz_q_peer={} lz_q_checked={} \
                 lz_q_mismatch={} lz_q_steps={} lz_q_capped={} lz_chunks_unpublished_dropped={}",
                lz.mode,
                lz.chunks_live,
                lz.retire_walks,
                lz.stw_walks,
                lz.objects_walked,
                lz.already_registered,
                lz.unregistered_found,
                lz.walk_stops,
                lz.chunks_dropped,
                lz.q_own,
                lz.q_peer,
                lz.q_checked,
                lz.q_mismatch,
                lz.q_steps,
                lz.q_capped,
                lz.chunks_unpublished_dropped,
            );
            // zr z5/sw: `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`'s engagement.
            // `prune_key_cycles` > 0 on any run with an inflated monitor (the
            // key probe replaced the dead list); `region_words_skipped` > 0
            // only with `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` on as well
            // (the default since zr z8/zb);
            // all five read 0 at `=0`.
            let (kc, kp, kd, rw, mf) = h.sweep_live_proportional_stats();
            eprintln!(
                "[GC] zgc-sweep-live: prune_key_cycles={kc} keys_probed={kp} \
                 keys_dead={kd} region_words_skipped={rw} mark_blocks_forgotten={mf}",
            );
            // zr z2/cp: `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`'s engagement.
            // `after_failure_slides` is the positive control of any row that
            // raised (or used to raise) an OOME on fragmentation; 0 there with
            // the switch unset means the widened selector never ran.
            let (pf, pt, pb) = h.relocation_pressure_select_stats();
            eprintln!(
                "[GC] zgc-capacity: after_failure_slides={pf} tight_slides={pt} \
                 widened_live_bytes={pb} refusals_seen={} \
                 infallible_unwind_offers={}",
                h.trigger_tallies().4,
                // `CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND`: infallible arms that
                // offered the VM's OOME unwind instead of aborting outright.
                h.exhaustion_unwind_offers(),
            );
            // zr z9/sl: `CRATONVM_ZGC_SELECTIVE_RELOCATE`'s engagement.
            // `sel_slides` > 0 only with the switch on (the positive control);
            // `sel_pages_kept` = sparse pages left in place because their holes
            // were already usable; `sel_emptied` = slides it declined outright;
            // `sel_failure_bypass` = after-failure slides that ran the full
            // selector (the fallback). `sel_bytes_moved` is the slides' copy
            // volume on EVERY arm (both ends), the number `bytes_copied=` on
            // `[GC] zgc-real:` is not.
            let (ss, sk, se, sb, sm) = h.selective_relocate_stats();
            // zs s1/sl: the cycle gate (`=2`) and the fused census.
            // `sel_cycle_*` > 0 only on `=2`; `sel_fused` = selective slides
            // whose census rode `logical_pages` (0 under
            // `CRATONVM_ZGC_SELECTIVE_FUSED_FRAG=0`, the positive control).
            let (ca, cd, ct, cf) = h.selective_cycle_stats();
            eprintln!(
                "[GC] zgc-selective: sel_slides={ss} sel_pages_kept={sk} \
                 sel_emptied={se} sel_failure_bypass={sb} sel_bytes_moved={sm} \
                 sel_cycle_admit={ca} sel_cycle_decline={cd} sel_cycle_tight={ct} \
                 sel_fused={cf}",
            );
            // zr z3/fp: what the collector holds, per structure, so a host
            // maxrss can be attributed (`docs/known-issues/gc/zgc-zr2cp-maxrss-
            // is-four-times-hotspot-20261012.md`). Bytes throughout.
            // `*_reserved` is address space (free until written), `*_noted`
            // its estimated resident part; `fp_arena_written` bounds the arena's
            // resident set from above; `fp_side_resident` sums the side
            // structures. `fp_trims` / `fp_idle_stamps` / `fp_sparse_skipped`
            // are the engagement counters of `CRATONVM_ZGC_FOOTPRINT_TRIM`,
            // `CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP` and
            // `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`.
            let fp = h.footprint_stats();
            eprintln!(
                "[GC] zgc-footprint: fp_arena_reserved={} fp_arena_committed={} \
                 fp_arena_committed_peak={} fp_arena_written={} fp_arena_uncommitted={} \
                 fp_arena_reset={} fp_arena_bookkeeping={} fp_start_bits_reserved={} \
                 fp_start_bits_noted={} fp_mark_bits_reserved={} fp_mark_bits_noted={} \
                 fp_old_bits_reserved={} fp_old_bits_noted={} fp_grids={} \
                 fp_relocations_entries={} fp_relocations_bytes={} fp_relocations_peak={} \
                 fp_dead_scratch_bytes={} fp_dead_scratch_peak={} fp_snapshot_peak={} \
                 fp_remset_bytes={} fp_side_resident={} fp_trims={} fp_trimmed_bytes={} \
                 fp_idle_stamps={} fp_sparse_skipped={} fp_trim_regrows={}",
                fp.arena_reserved,
                fp.arena_committed,
                fp.arena_committed_peak,
                fp.arena_ever_written,
                fp.arena_uncommitted_total,
                fp.arena_reset_total,
                fp.arena_bookkeeping,
                fp.start_bits_reserved,
                fp.start_bits_noted,
                fp.mark_bits_reserved,
                fp.mark_bits_noted,
                fp.old_bits_reserved,
                fp.old_bits_noted,
                fp.grids,
                fp.relocations_entries,
                fp.relocations_bytes,
                fp.relocations_peak,
                fp.dead_scratch_bytes,
                fp.dead_scratch_peak,
                fp.snapshot_peak,
                fp.remset_bytes,
                fp.side_resident_estimate(),
                fp.trims,
                fp.trimmed_bytes,
                fp.idle_stamps,
                fp.sparse_words_skipped,
                // zt t8/rl2: trims the next slide undid (see
                // `CRATONVM_ZGC_RELOCATIONS_TRIM_HYSTERESIS`).
                fp.trim_regrows,
            );
            // zr z7/rs: where the arena and the bitmaps LIVE, so
            // `tools/rss-by-mapping.sh` can attribute the smaps mappings that
            // hold them (`gc:java-heap`, `gc:bitmaps`) instead of counting
            // them as the allocator's. `arena_no_thp` is
            // `CRATONVM_ZGC_ARENA_NO_THP`'s outcome (0 unset, 1 applied,
            // 2 refused, 3 yielded to `CRATONVM_HEAP_THP`).
            eprintln!(
                "[GC] zgc-footprint-spans: {} at=exit",
                h.footprint_spans().render()
            );
            // zr z4/un: the uncommitter (`CRATONVM_ZGC_UNCOMMITTER`), the net
            // commitment (`CRATONVM_ZGC_RESET_ACCOUNTING`, what
            // `Runtime.totalMemory()` reads) and the slide's transient
            // bookkeeping peaks (`CRATONVM_ZGC_SLIDE_BUFFER_TRIM`). Bytes,
            // except the pass counts.
            eprintln!(
                "[GC] zgc-uncommitter: un_running={} un_passes={} un_passes_released={} \
                 un_reset={} un_decommitted={} un_busy={} un_net_committed={} \
                 un_slide_pairs_peak={} un_slide_index_peak={} un_slide_map_peak={} \
                 un_slide_transient_peak={} un_bitmap_released={} un_prefaulted={}",
                h.uncommitter_running() as u8,
                fp.uncommit_passes,
                fp.uncommit_passes_released,
                fp.uncommit_reset_bytes,
                fp.uncommit_decommitted_bytes,
                fp.uncommit_busy,
                fp.arena_net_committed,
                fp.slide_pairs_peak,
                fp.slide_index_peak,
                fp.slide_map_peak,
                fp.slide_transient_peak,
                // zt t3/rl: `CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`.
                fp.bitmap_pages_released,
                // jr21 gc: bytes the pre-fault worker populated ahead of the
                // low cursor (`CRATONVM_ZGC_PREFAULT_AHEAD`).
                h.prefaulted_bytes(),
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
                    "[GC] zgc-overlay-gate: provider={name} hits={hits} misses={misses} disabled={disabled}",
                );
            }
            eprintln!(
                "[GC] zgc-mark-wait: park_timeouts={}",
                h.mark_park_timeouts(),
            );
            eprintln!(
                "[GC] zgc-integrity: unsizable_registered_objects={}",
                h.unsizable_registered_objects(),
            );
            let g = h.frag_gauge();
            match g.worst_permille {
                Some(worst) => eprintln!(
                    "[GC] zgc-frag: frag_samples={} worst_largest_free_permille={} free_permille_at_worst={} at_cycle={}",
                    g.samples, worst, g.free_permille, g.worst_cycle,
                ),
                None => eprintln!(
                    "[GC] zgc-frag: frag_samples=0 worst_largest_free_permille=n/a (no collection left >=25% of the heap free; this run is not evidence either way)"
                ),
            }
        }
        // gen r4w4/alloc4: the TLAB census on G1 and ZGC too — residue (b) of
        // the write-only-instruments page. The census is process-wide and every
        // backend's TLABs retire through `tlab.rs`, and `FILLER_OVER_OBJECT` can
        // fire on any of them, but the two lines were printed only in the
        // Generational arm below, so a G1 or ZGC summary said nothing about its
        // own TLAB waste, sizer or tripwires. Same two lines, same renderer;
        // `vm-cli`'s fired-tripwire print now defers to every backend's summary.
        if !matches!(self, VmHeap::Generational(_)) {
            // gci i48/zl3: `sizer_on=` resolves G1's default on a G1 heap.
            let [tlab_waste, tlab_guard] =
                crate::tlab::tlab_census_lines_for_backend(false, self.is_g1());
            eprintln!("{tlab_waste}");
            eprintln!(
                "{}",
                render_tlab_guard_line(&tlab_guard, crate::tlab::tlab_guard_line_is_urgent()),
            );
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
            // gen r4w2/obs (2026-09-23): three instruments wave 1 added and
            // nothing printed.
            //
            // The young trigger (lane E): the live threshold, what set it and
            // what the pause-goal loop did to it. Formatted in `gen_heap.rs`
            // (`YoungTriggerCensus::line`); its keys are prefixed
            // (`young_trigger_*`, `adapt_*`) and are in this file's
            // key-uniqueness corpus.
            eprintln!("{}", h.young_trigger_census_line());
            // gcd d9/b: the young trigger's floor on a wedged old generation
            // (`gen_heap::young_trigger_floor_judged`, keys `ytw_*`) and the
            // allocation-failure ladder's work per heap `OutOfMemoryError`
            // (`GenerationalHeap::oome_ladder_census_line`, keys `ladder_*`).
            // Both formatted in `gen_heap.rs` and in the uniqueness corpus.
            eprintln!("{}", h.young_trigger_wedged_census_line());
            eprintln!("{}", h.oome_ladder_census_line());
            // gen r4w4/young4 (2026-09-24): the young cycles the conservative
            // JIT-root term (`nonmoving-unrewritable-conservative-jit-roots`)
            // diverted, and what a pin-aware copying cycle would have had to
            // leave in place on them. See
            // `GenerationalHeap::conservative_divert_census`.
            let [cj_div, cj_zero, cj_ysum, cj_ymax, cj_psum, cj_noveto] =
                h.conservative_divert_census();
            // gen r4w5/pinwords5: the young pin-word ledger on the same cycles
            // (option B's price; process-wide, see
            // `gc_quiescence::young_pin_ledger_census`). `cjdiv_ledger_empty` is
            // how many term-4 cycles option B alone would have let copy;
            // `cjdiv_ledger_cleared` how many it did
            // (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`).
            // gcj j1pn ledger item 2: `cjdiv_ledger_screened_out_sum` is the
            // to-space / above-top words the from-space screen dropped.
            let [
                lg_evals,
                lg_incomplete,
                lg_empty,
                lg_wsum,
                lg_wmax,
                lg_native,
                lg_cleared,
                lg_screened,
            ] = crate::gc_quiescence::young_pin_ledger_census();
            // gcj j1pn ledger item 3: initiator deposits that skipped the
            // pause door's own frames, and the bytes skipped.
            let (lg_cuts, lg_cut_bytes) = crate::gc_quiescence::young_pin_collector_cut_census();
            eprintln!(
                "[GC] young_conservative_divert: cjdiv_diverts={cj_div} \
                 cjdiv_no_young_pin={cj_zero} cjdiv_young_pins_sum={cj_ysum} \
                 cjdiv_young_pins_max={cj_ymax} cjdiv_pins_sum={cj_psum} \
                 cjdiv_no_initiator_veto={cj_noveto} cjdiv_ledger_evals={lg_evals} \
                 cjdiv_ledger_incomplete={lg_incomplete} cjdiv_ledger_empty={lg_empty} \
                 cjdiv_ledger_words_sum={lg_wsum} cjdiv_ledger_words_max={lg_wmax} \
                 cjdiv_ledger_native_sum={lg_native} cjdiv_ledger_cleared={lg_cleared} \
                 cjdiv_ledger_screened_out_sum={lg_screened} \
                 cjdiv_ledger_collector_cuts={lg_cuts} \
                 cjdiv_ledger_collector_cut_bytes={lg_cut_bytes}"
            );
            // gen r4w5/pinned5 (2026-09-24): the pinned in-place young copy
            // (`CRATONVM_GEN_PINNED_YOUNG_COPY`) — the term-4 cycles it took,
            // the ones it declined, and what it left in place. See
            // `GenerationalHeap::pinned_young_census`.
            let [py_cycles, py_nowords, py_bound, py_ledger, py_psum, py_pmax, py_osum, py_bsum, py_copies, py_overflow, py_kept] =
                h.pinned_young_census();
            // gch h2/yg: of `pycopy_ledger_incomplete`, the declines with raw
            // JNI locals open (`GenerationalHeap::pinned_ledger_incomplete_raw_jni`).
            let py_raw_jni = h.pinned_ledger_incomplete_raw_jni();
            eprintln!(
                "[GC] young_pinned_copy: pycopy_cycles={py_cycles} \
                 pycopy_no_young_words={py_nowords} pycopy_over_bound={py_bound} \
                 pycopy_ledger_incomplete={py_ledger} pycopy_pages_sum={py_psum} \
                 pycopy_pages_max={py_pmax} pycopy_objects_sum={py_osum} \
                 pycopy_bytes_sum={py_bsum} pycopy_inplace_copies={py_copies} \
                 pycopy_overflow_promotions={py_overflow} pycopy_kept={py_kept} \
                 pycopy_incomplete_raw_jni={py_raw_jni}"
            );
            // gch h2/yg (gcd d1e proposal): relocating cycles whose blocked-peer
            // capture held a young non-start word, and how many of them pinned it
            // (`GenerationalHeap::blocked_peer_interior_census`).
            let [bpw_cycles, bpw_with, bpw_words, bpw_pinned] = h.blocked_peer_interior_census();
            eprintln!(
                "[GC] young_blocked_peer_words: bpw_relocating_cycles={bpw_cycles} bpw_cycles_with_words={bpw_with} bpw_words_sum={bpw_words} bpw_cycles_pinned={bpw_pinned}"
            );
            // gcf f2/y: the pinned copy's defragmentation gate
            // (`CRATONVM_GEN_PINNED_COPY_DEFRAG`), formatted in `gen_heap.rs`.
            eprintln!("{}", h.pinned_copy_defrag_census_line());
            // gcj j1/yg: which term kept each term-4 cycle from the pinned
            // copy (`CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK`).
            eprintln!("{}", h.term4_offer_census_line());
            // gcj yg3-residuals R3.
            eprintln!("{}", h.young_geometry_census_line());
            // gcg g1/y: `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`'s engagement
            // (`[GC] pinned_exact_roots:`), formatted in `gen_heap.rs`.
            eprintln!("{}", h.pinned_exact_roots_census_line());
            // gcf f3/q: the young-premium cuts of the non-moving sweep
            // (`CRATONVM_GEN_WALK_SIZE_MEMO` and friends), formatted in
            // `gen_heap.rs`.
            eprintln!("{}", h.young_premium_census_line());
            // gcd d3/m: the opt-in pinned-copy take-over arm
            // (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`).
            let [pt_cand, pt_taken, pt_incomplete, pt_over] = h.pinned_takeover_census();
            eprintln!(
                "[GC] young_pinned_takeover: ptko_candidates={pt_cand} ptko_taken={pt_taken} ptko_ledger_incomplete={pt_incomplete} ptko_over_bound={pt_over}"
            );
            // gcj j1/yg item 1: the opt-in unproven-frame arm
            // (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`).
            let [pu_cand, pu_taken, pu_incomplete, pu_over] = h.pinned_unproven_census();
            eprintln!(
                "[GC] young_pinned_unproven: pyun_candidates={pu_cand} pyun_taken={pu_taken} pyun_ledger_incomplete={pu_incomplete} pyun_over_bound={pu_over}"
            );
            let [ur_reads, ur_dep, ur_depth, ur_orphan, ur_bounds, ur_jni, ur_tko, ur_cov, ur_peer, ur_ovf, ur_marked] =
                crate::gc_quiescence::young_pin_unproven_read_census();
            eprintln!(
                "[GC] young_pinned_unproven_reads: pyur_reads={ur_reads} pyur_deposit_incomplete={ur_dep} pyur_depth_short={ur_depth} pyur_orphaned={ur_orphan} pyur_bounds={ur_bounds} pyur_raw_jni={ur_jni} pyur_takeover={ur_tko} pyur_coverage={ur_cov} pyur_peer_stacks={ur_peer} pyur_deposits_overflowed={ur_ovf} pyur_deposits_marked={ur_marked}"
            );
            // gcd d5/s: why the take-over arm declined
            // (`GenerationalHeap::pinned_takeover_declines`).
            let [tk_reached, tk_none, tk_frozen, tk_unread, tk_refused, tk_derived, tk_cov, tk_div] =
                h.pinned_takeover_declines();
            eprintln!(
                "[GC] young_pinned_takeover_declines: ptkod_reached={tk_reached} ptkod_no_takeover_peer={tk_none} ptkod_frozen={tk_frozen} ptkod_unread={tk_unread} ptkod_window_refused={tk_refused} ptkod_derived_unresolved={tk_derived} ptkod_other_coverage={tk_cov} ptkod_other_divert={tk_div}"
            );
            // gen r5w6/pin10: header screens that refused a forward chain or a
            // BUSY mark (`gen_heap::header_screen_counts`).
            let (hs_fwd, hs_busy, hs_abandoned) = crate::gen_heap::header_screen_counts();
            eprintln!(
                "[GC] header_screens: hscr_forward_declined={hs_fwd} hscr_busy_declined={hs_busy} hscr_busy_wait_abandoned={hs_abandoned}"
            );
            // gcd d2/h: precise young roots the object-start walk did not
            // record (`GenerationalHeap::unrecorded_young_root_census`).
            let [ur_refused, ur_open, ur_roots] = h.unrecorded_young_root_census();
            let ur_ledger = h.unrecorded_young_root_ledger_guarded_cycles();
            // gch h1/yp: offenders the guard's exclusion set absorbed
            // (`GenerationalHeap::unrecorded_young_root_exclusion_census`).
            let [ur_ex_cycles, ur_ex_roots] = h.unrecorded_young_root_exclusion_census();
            // gch h2/yg: the longest run of consecutive refused cycles
            // (`GenerationalHeap::unrecorded_young_root_max_streak`).
            let ur_streak = h.unrecorded_young_root_max_streak();
            // gci i10/gr: cycles the dirty-card / overlay value guard ran on
            // (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`; 0 with it off).
            let ur_cards = h.unrecorded_young_root_cards_guarded_cycles();
            eprintln!(
                "[GC] young_unrecorded_roots: urr_cycles_refused={ur_refused} urr_failed_open={ur_open} urr_roots_sum={ur_roots} urr_ledger_guarded={ur_ledger} urr_excluded_cycles={ur_ex_cycles} urr_excluded_roots={ur_ex_roots} urr_max_streak={ur_streak} urr_cards_guarded={ur_cards}"
            );
            // gen r4w4/alloc4: which allocation door this heap's objects took
            // (TLAB misses, old-gen direct), the refill outcomes, and the
            // zero-once / humongous-unlocked memset census. Formatted in
            // `gen_heap.rs` (`GenerationalHeap::alloc_path_census_line`, wave 3
            // left it unprinted); its keys are in the uniqueness corpus below.
            eprintln!("{}", h.alloc_path_census_line());
            // The collections as the two JMX collector beans report them
            // (`Copy` / `MarkSweepCompact`, `gc_metrics::serial_collectors`), so
            // a probe reading `GarbageCollectorMXBean` counts has the VM-side
            // figure to compare against. `minor=` above is NOT the `Copy` count:
            // it is also bumped by cycles that ran old gen.
            let [young_bean, full_bean] = h.jmx_collectors();
            eprintln!(
                "[GC] jmx_collectors: copy_collections={} copy_time_ms={} \
                 marksweepcompact_collections={} marksweepcompact_time_ms={}",
                young_bean.count, young_bean.time_ms, full_bean.count, full_bean.time_ms,
            );
            // The card table's self-maintained scan bound (lane D). A zero
            // `cards_skipped_by_scan_bound` on a run that collected means the
            // bound is inert; `raw_card_address_escaped=true` says why (the JIT
            // was handed the raw map, which disarms the bound for good).
            let (bound, num_cards, skipped, escaped) = h.card_scan_bound_census();
            eprintln!(
                "[GC] cards/scan_bound: scan_bound_cards={bound} num_cards={num_cards} \
                 cards_skipped_by_scan_bound={skipped} raw_card_address_escaped={escaped}"
            );
            // gen r4w3/obs2 (2026-09-23): the wave-2 instruments nobody printed
            // (cross-lane requests of the youngpolicy, oldgen2 and concmark
            // reviews). Every key carries its line's prefix, so it stays unique
            // across the summary.
            //
            // Promote-on-pressure arms (lane youngpolicy): armed, consumed by a
            // moving cycle, consumed after being carried across a non-moving
            // cycle, dropped at a divert (`CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES`).
            let (pp_armed, pp_consumed, pp_stale, pp_dropped) = h.promote_pressure_census();
            eprintln!(
                "[GC] promote_pressure: promote_pressure_armed={pp_armed} \
                 promote_pressure_consumed={pp_consumed} \
                 promote_pressure_consumed_stale={pp_stale} \
                 promote_pressure_dropped_on_divert={pp_dropped}"
            );
            // The old-gen trigger's verdicts and the old generation's backing
            // store (lane oldgen2). One old-gen lock at shutdown.
            let (trig, oldgen_committed, oldgen_given_back) = h.old_gen_trigger_stats();
            eprintln!(
                "[GC] oldgen_trigger: trig_collections={} trig_low_yield={} trig_requested={} \
                 trig_occupancy={} trig_alloc_failure={} trig_suppressed={} \
                 trig_would_suppress={} oldgen_alloc_failures={} oldgen_commit_refusals={} \
                 oldgen_allocs={} oldgen_nonfit_probes={} oldgen_committed_bytes={oldgen_committed} \
                 oldgen_given_back_bytes={oldgen_given_back}",
                trig.collections,
                trig.low_yield,
                trig.requested,
                trig.occupancy,
                trig.allocation_failure,
                trig.suppressed,
                trig.would_suppress,
                trig.alloc_failures,
                trig.commit_refusals,
                trig.allocs,
                trig.nonfit_probes,
            );
            // gen r5w3/obs7 (cross-lane request from oldgen7,
            // `gengc-r5w3-oldgen7-old-gen-sizing-stats-are-never-printed`): the
            // old generation's sizing counters, which no production code
            // printed, so no flag-on/flag-off A/B could read them. Keys are
            // `oldsz_`-prefixed (unique across the summary). One more old-gen
            // lock at shutdown.
            let s = h.old_gen_sizing_stats();
            eprintln!(
                "[GC] oldgen_sizing: oldsz_reserved={} oldsz_committed={} oldsz_committed_peak={} \
                 oldsz_commit_floor={} oldsz_shrinks={} oldsz_shrunk_bytes={} oldsz_shrinks_damped={} \
                 oldsz_conc_resizes={} oldsz_conc_shrinks={} oldsz_interior_decommits={} \
                 oldsz_interior_decommitted_bytes={} oldsz_interior_damped={} \
                 oldsz_hole_recommits={} oldsz_frag_refusals={} \
                 oldsz_frag_compactions_requested={} oldsz_compactions={} \
                 oldsz_pinned_compactions={} oldsz_pinned_moved_bytes={} oldsz_pinned_refusals={} oldsz_humongous_requests={} \
                 oldsz_walk_gap_recoveries={} oldsz_borrow_growths={} oldsz_borrowed_bytes={} \
                 oldsz_growth_max={} oldsz_humongous_top_allocs={} \
                 oldsz_humongous_top_fallbacks={} oldsz_walk_breaks_planted={} \
                 oldsz_moving_major_vetoes={} oldsz_true_root_majors={} \
                 oldsz_true_root_fallbacks={} oldsz_true_root_young_excluded={} \
                 oldsz_true_root_fb_walk_gap={} oldsz_true_root_fb_promotion_off_grid={} \
                 oldsz_true_root_fb_promoted={} oldsz_true_root_fb_pinned_plan={} \
                 oldsz_true_root_fb_moving_cycle={} oldsz_true_root_fb_compacting={} \
                 oldsz_true_root_promotions={} \
                 oldsz_true_root_promotions_dead={} oldsz_requested_major_compactions={} \
                 oldsz_requested_major_compaction_declines={} oldsz_mark_admission_bitmaps={} \
                 oldsz_pinned_compaction_movable_words_unpinned={} \
                 oldsz_moving_legacy_seed_majors={}",
                s.reserved_bytes,
                s.committed_bytes,
                s.committed_peak,
                s.commit_floor,
                s.shrinks,
                s.shrunk_bytes,
                s.shrinks_damped,
                s.concurrent_sweep_resizes,
                s.concurrent_sweep_shrinks,
                s.interior_decommits,
                s.interior_decommitted_bytes,
                s.interior_decommits_damped,
                s.hole_recommits,
                s.fragmentation_refusals,
                s.fragmentation_compactions_requested,
                s.compactions,
                s.pinned_compactions,
                s.pinned_compaction_moved_bytes,
                s.pinned_compaction_refusals,
                s.humongous_compaction_requests,
                s.walk_gap_recoveries,
                s.borrow_growths,
                s.borrowed_bytes,
                s.growth_max_bytes,
                s.humongous_top_allocs,
                s.humongous_top_fallbacks,
                s.walk_breaks_planted,
                s.moving_major_compaction_vetoes,
                s.true_root_majors,
                s.true_root_fallbacks,
                s.true_root_young_excluded,
                s.true_root_fallbacks_for(crate::old_gen::TrueRootFallback::WalkGap),
                s.true_root_fallbacks_for(crate::old_gen::TrueRootFallback::PromotionOffGrid),
                s.true_root_fallbacks_for(crate::old_gen::TrueRootFallback::Promoted),
                s.true_root_fallbacks_for(crate::old_gen::TrueRootFallback::PinnedPlan),
                s.true_root_fallbacks_for(crate::old_gen::TrueRootFallback::MovingCycle),
                s.true_root_fallbacks_for(crate::old_gen::TrueRootFallback::Compacting),
                s.true_root_promotions,
                s.true_root_promotions_dead,
                s.requested_major_compactions,
                s.requested_major_compaction_declines,
                s.mark_admission_bitmaps,
                s.pinned_compaction_movable_words_unpinned,
                s.moving_legacy_seed_majors,
            );
            // gcj j1/og (cross-lane X1): engagement of the old-gen compaction
            // forward index and of the free-ratio growth.
            eprintln!(
                "[GC] oldgen_j1og: oldsz_forward_index_compactions={} oldsz_free_ratio_growths={} \
                 oldsz_free_ratio_grown_bytes={} oldsz_par_adjust_compactions={} \
                 oldsz_free_ratio_growths_after_concurrent={}",
                s.forward_index_compactions, s.free_ratio_growths, s.free_ratio_grown_bytes,
                s.par_adjust_compactions, s.free_ratio_growths_after_concurrent,
            );
            // gcj j3/og3 (cross-lane X1): the Phase 0 drop, the Serial-shaped
            // trigger and capacity-shrink counters.
            eprintln!(
                "[GC] oldgen_j3og3: oldsz_phase0_skips={} oldsz_trust_mark_disagreements={} oldsz_trust_mark_verifications={} oldsz_compact_escape_hits={} \
                 oldsz_serial_trigger_due={} oldsz_serial_trigger_over_floor_skips={} \
                 oldsz_capacity_shrinks={} oldsz_capacity_shrunk_bytes={} \
                 oldsz_frag_window_compactions={} oldsz_frag_window_not_needed={}",
                s.phase0_skips, s.trust_mark_disagreements, s.trust_mark_verifications,
                crate::old_gen::COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed),
                s.serial_trigger_due,
                s.serial_trigger_over_floor_skips, s.capacity_shrinks, s.capacity_shrunk_bytes,
                s.frag_window_compactions, s.frag_window_not_needed,
            );
            eprintln!(
                "[GC] serial_pool_accounting: snapshot_fallbacks={}",
                h.serial_pool_snapshot_fallbacks(),
            );
            // gch h4/wr: the engagement counter of
            // `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD` (watched young
            // survivors the true-root majors judged dead), on a line of its own
            // so the sizing line's key list stays the old generation's.
            eprintln!(
                "[GC] true_root_watched: oldsz_true_root_watched_dead={}",
                h.true_root_watched_left_out_total(),
            );
            // The sliced concurrent old-gen cycle (lane concmark). Process-wide
            // counters (`concurrent_mark.rs` statics), printed on this arm
            // because the generational driver is the one that bumps them.
            {
                use crate::concurrent_mark as cm;
                use std::sync::atomic::Ordering as O;
                eprintln!(
                    "[GC] conc_mark: conc_mark_slices={} conc_remark_retries={} \
                     conc_remark_abandons={} conc_cycle_stale_abandons={} \
                     conc_cycle_open_refused={} conc_mark_walks_built={} \
                     conc_mark_walks_reused={} conc_mark_walk_desync_aborts={} conc_mark_walk_gap_recoveries={}",
                    cm::CONC_MARK_SLICES.load(O::Relaxed),
                    cm::CONC_REMARK_RETRIES.load(O::Relaxed),
                    cm::CONC_REMARK_ABANDONS.load(O::Relaxed),
                    cm::CONC_CYCLE_STALE_ABANDONS.load(O::Relaxed),
                    cm::CONC_CYCLE_OPEN_REFUSED.load(O::Relaxed),
                    cm::CONC_MARK_WALKS_BUILT.load(O::Relaxed),
                    cm::CONC_MARK_WALKS_REUSED.load(O::Relaxed),
                    cm::CONC_MARK_WALK_DESYNC_ABORTS.load(O::Relaxed),
                    // gen r5w6/sizer10 (old9's cross-lane request 2): remarks
                    // that recovered an unwalked gap conservatively instead of
                    // refusing the sweep (`concurrent_mark.rs`).
                    cm::CONC_MARK_WALK_GAP_RECOVERIES.load(O::Relaxed),
                );
            }
            // gen r5w6/sizer10 (pin9's request): candidates
            // `GenerationalHeap::is_object_address` declined because their
            // header reads FORWARDED and the forward leaves this heap (the
            // netty `0x100000004` SIGSEGV family). Process-wide, like the counters
            // above; printed only when non-zero, so a clean run's summary is
            // unchanged.
            {
                let declined = crate::gen_heap::FORWARD_SCREEN_DECLINED
                    .load(std::sync::atomic::Ordering::Relaxed);
                if declined != 0 {
                    eprintln!("[GC] forward_screen: fwd_screen_declined={declined}");
                }
            }
            // gen r4w4/concmark4: the old-gen policy (concurrent-first vs
            // legacy), its start threshold, and the per-VM cycle census —
            // including the initial-mark takeover numbers wave 3 asked for.
            if let Some(line) = h.concurrent_policy_census_line() {
                eprintln!("{line}");
            }
            // gen r4w5/concmark5: the concurrent-cycle driver (service thread,
            // remark reference-processing hook) and the STW old-gen trigger's
            // count-based cadence (majors per 100 young decisions, back-to-back
            // majors, majors under 5 % yield). Keys `concdrv_` / `majcad_`.
            if let Some(line) = h.concurrent_driver_census_line() {
                eprintln!("{line}");
            }
            if let Some(line) = h.major_cadence_census_line() {
                eprintln!("{line}");
            }
            // The shared SATB queue's two gate-ordering detectors
            // (`gengc-r4-mark-counters-and-comments-that-lie`): a log that
            // arrived after the queue was deactivated, and a deactivation
            // that timed out waiting for in-flight barriers. Non-zero on
            // either is a finding. Absent until the concurrent collector is
            // wired (`enable_concurrent_gc`).
            if let Some(q) = h.satb_queue_ref() {
                eprintln!(
                    "[GC] satb_queue: satb_late_log_drops={} satb_quiescence_timeouts={}",
                    q.late_log_drops(),
                    q.quiescence_timeouts(),
                );
            }
            // GC notifications (this lane): printed once any bean has had a
            // listener, so a run that never used them keeps its summary.
            let notif = h.gc_notifications().census();
            if notif.queued != 0 || notif.deferred != 0 {
                eprintln!("{}", notif.summary_line());
            }
            // Re-publish the normalization denominators so the card-cost report
            // below divides by the CURRENT heap rather than by whatever the
            // last collection saw. Cheap: two arena locks at shutdown.
            h.publish_gc_metrics_occupancy();
        }
        // gc-common w7-f: the same GC-notification census for a backend whose
        // beans the event plumbing books (G1 here; ZGC's state lives in the VM
        // and is not reachable from this summary), under the same rule.
        if let Some(b) = self.backend_gc_beans() {
            // The collections as G1's three JMX collector beans report them,
            // unconditionally (the VM-side figure a `GarbageCollectorMXBean`
            // probe compares against, like Generational's `jmx_collectors:`).
            // Not `collections=` of the G1 collector's own counters: those
            // also count evacuation-failure drain passes inside one pause.
            if let [young, old, conc] = b.collectors().as_slice() {
                eprintln!(
                    "[GC] g1_jmx_collectors: g1_young_collections={} g1_young_time_ms={} \
                     g1_old_collections={} g1_concurrent_collections={} \
                     g1_concurrent_time_ms={}",
                    young.count, young.time_ms, old.count, conc.count, conc.time_ms,
                );
            }
            let notif = b.notifications().census();
            if notif.queued != 0 || notif.deferred != 0 {
                eprintln!("{}", notif.summary_line());
            }
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
            // The selective-promotion census, read ONCE for both lines that
            // need it — the `young_sweep:` denominator immediately below and
            // the `young_walk_entries:` line further down. One read, so the two
            // lines of a single summary can never disagree.
            let (sw, sel, defrag, cand, pin, unaged, evac, ofull) =
                crate::gen_heap::selective_promotion_census();
            // `non_moving_sweeps` is `par_attempts`' DENOMINATOR, printed on
            // the same line as the numerator (gengc-round2-core2).
            //
            // `par_attempts` lives inside `sweep_young_non_moving`, so its
            // population is "non-moving young cycles" and NOT "collections".
            // Read without that denominator, `par_attempts=0` says "the
            // parallel-sweep predicate is inert" when the truth may be "this
            // workload took the MOVING path on every cycle and never reached
            // the predicate at all". That misreading is exactly what round 1's
            // probe results recorded and round 2 had to correct
            // (`docs/feature-designs/generative-gc-architecture.md`);
            // the 2026-09-20 `BinT 14` soak had ~1500 collections and
            // `non_moving_cycles_total=2`.
            //
            // The key is deliberately NOT `sp_sweeps=`, which is the same
            // counter under its selective-promotion name on the
            // `young_walk_entries:` line below (and, per cycle, on
            // `[GC_OVERHEAD]` in `vm/`). Minting a second `sp_sweeps=` token
            // would give one run two lines carrying that key, which is the
            // duplicate-key defect this round kept finding; a distinct name
            // keeps a scraper's `sp_sweeps=` grep single-valued while still
            // putting the number where the reader of `par_attempts` needs it.
            eprintln!(
                "[GC] young_sweep: non_moving_sweeps={sw} par_attempts={} par_accepts={} zero_spans={} \
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
            // The moving path's twin: which check refused a chunk of the
            // PARALLEL object-start walk (gen r4w2/youngmark). Legend on
            // `OBJSTART_CHUNK_BAILS`; `objstart_` prefix per the key test below.
            let osb = &crate::gen_heap::OBJSTART_CHUNK_BAILS;
            eprintln!(
                "[GC] objstart_chunk_bails: objstart_anchor_in_skip={} objstart_skip_overflow={} \
                 objstart_cursor_in_skip={} objstart_gap_filler={} objstart_bad_size={} \
                 objstart_skip_crossing={} objstart_misaligned={} objstart_anchor_miss={}",
                osb[0].load(O::Relaxed),
                osb[1].load(O::Relaxed),
                osb[2].load(O::Relaxed),
                osb[3].load(O::Relaxed),
                osb[4].load(O::Relaxed),
                osb[5].load(O::Relaxed),
                osb[6].load(O::Relaxed),
                osb[7].load(O::Relaxed),
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
            // `sw`/`sel`/… come from the single `selective_promotion_census()`
            // read at the top of this block, which also feeds
            // `non_moving_sweeps=` on the `young_sweep:` line.
            let w = &crate::gen_heap::YOUNG_WALK_ENTRIES;
            // `sp_sweeps_without_selective` (gen r4w2/obs): the non-moving
            // sweeps that did not run the selective-promotion walk. Since gen
            // r4w2/youngmark those sweeps still AGE their survivors (the walk
            // runs age-only), so this counts non-promoting sweeps, not
            // non-aging ones (only `CRATONVM_NO_SELECTIVE_PROMOTE` skips the
            // walk) — see
            // `docs/feature-designs/generative-gc-architecture.md`.
            eprintln!(
                "[GC] young_walk_entries: evac_prepass={} fixup_3a={} mark_y2o={} \
                 fixup_yo={} walk_young={} | sp_sweeps={sw} sp_selective={sel} \
                 sp_sweeps_without_selective={} \
                 sp_defrag={defrag} sp_candidates={cand} sp_pinned={pin} \
                 sp_unaged={unaged} sp_evacuated={evac} sp_old_full={ofull}",
                w[0].load(O::Relaxed),
                w[1].load(O::Relaxed),
                w[2].load(O::Relaxed),
                w[3].load(O::Relaxed),
                w[4].load(O::Relaxed),
                sw.saturating_sub(sel),
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
            // THE TLAB CENSUS AND THE TWO TLAB TRIPWIRES.
            //
            // `gengc-alloc-tlab-instruments-are-write-only-FIXED-20260924.md`:
            // `FILLER_OVER_OBJECT` and `REFILL_OVER_OBJECT` were incremented
            // and loaded by NOTHING in the workspace. `FILLER_OVER_OBJECT`'s
            // own doc calls a non-zero count "a use-after-free in waiting, not
            // a diagnostic curiosity" — the write site names the live lambda
            // capture at `0x200868400d8`, 261 336 bytes inside a 261 792-byte
            // filler, that reached `invokevirtual` as an all-zero header — and
            // the only trace a run left was a rate-limited `tracing::error!`
            // on `cratonvm::gc::guard` (the first eight occurrences and then
            // powers of two — `tlab.rs:1562`), a target a release run with the
            // default subscriber never prints and nothing in CI greps. A run
            // could trip it on every collection and finish looking clean.
            // Round 1 added the readers, round 2 the formatter; this is the
            // printer, and it is the half that makes the instrument an
            // instrument.
            //
            // A PULL, like `tlab_lazy_zero_stats()` on the ZGC block above:
            // `tlab.rs` holds no heap handle (that is the stated reason
            // `WATCH_COLLECTION` is a process static there) and has no output
            // policy, so the formatting lives there and the decision to print
            // lives here. Cheap: nine relaxed loads at shutdown.
            //
            // G1 and ZGC print the same two lines near the top of this
            // function (gen r4w4/alloc4; they used to be Generational-only, so
            // the other two backends' share of these process-wide numbers was
            // reported nowhere). The tuning element carries the
            // `[GC] tlab-sizer:` line after a newline (`tlab_sizer_line`).
            // gen r5w4/defaults8: `_for(true)` — this is the Generational arm,
            // where the share sizer's default is ON, so `sizer_on=` must be
            // this heap's answer, not the heap-less one.
            let [tlab_waste, tlab_guard] = crate::tlab::tlab_census_lines_for(true);
            eprintln!("{tlab_waste}");
            // The guard line is CORRECTNESS, not tuning, so it is printed
            // whether or not it fired — a zero-valued guard line is what
            // distinguishes "the tripwire says no" from "nobody asked", which
            // is the whole complaint the gap page was filed over. When it DID
            // fire it must not read like another gauge in a wall of gauges, so
            // the urgent case gets the `compact_oop_map_missing` treatment: the
            // same key=value prefix a grep wants, plus a suffix that says what
            // the number means. `tlab_guard_line_is_urgent()` is the predicate.
            eprintln!(
                "{}",
                render_tlab_guard_line(&tlab_guard, crate::tlab::tlab_guard_line_is_urgent()),
            );
            // THE EVACUATION POOL'S WIDTH, step (4) of
            // `gengc-alloc-evac-pool-width-frozen-FIXED-20260923.md`: "`EvacPool::
            // helpers()` should be published on the `[GC]` shutdown line …  so
            // 'the pool was narrower than the ask' stops needing a temporary
            // `eprintln!` to see."
            //
            // The ASK is not reachable from here — both callers clamp to
            // `pool.helpers()` before dispatching, so `scope` is never told it
            // was asked for more, and a counter of that inside the pool would
            // be unfireable. What the line carries instead is the pool's own
            // history, which shows the same defect from the other side:
            // `pool_built_with == pool_max_dispatched` with `pool_grew_by=0`
            // over a large `pool_dispatches` IS the frozen pool. `pool_grew_by`
            // is also the engagement counter for `EvacPool::ensure_helpers`,
            // whose only production caller is the one line of part (b) that is
            // still open in `gen_heap.rs` — so `pool_grew_by=0` says "part (b)
            // has not landed" rather than "it landed and did nothing".
            //
            // Generational arm (G1 prints its own pool's as `[GC] g1
            // evac-pool:`). g1gen/c6: this heap's own pool, not process-wide
            // counters every heap's pool fed.
            if let VmHeap::Generational(h) = self {
                eprintln!(
                    "{}",
                    crate::evac_pool::evac_pool_census_line(h.evac_pool_census())
                );
            }
        }
        // Old-gen free-list coalescing (the counterpart of the young sweep's
        // post-sweep coalescer). A large `merged` with compaction never having
        // run is the fragmentation regime this exists for.
        //
        // gen r4w2/obs (2026-09-23): `coalesce_skipped_maximal` is the passes
        // that returned at once because nothing had been freed since the last
        // full merge (`OldGen::free_list_maximal`, round 4 lane C). This line
        // used to print only when `calls > 0`, and the comment here read
        // `calls>0 merged=0` as "already maximally coalesced" — the skip
        // counter is now that reading, so the line prints when EITHER moved.
        {
            use std::sync::atomic::Ordering as O;
            let calls = crate::old_gen::COALESCE_CALLS.load(O::Relaxed);
            let merged = crate::old_gen::BLOCKS_MERGED.load(O::Relaxed);
            let skipped_maximal = crate::old_gen::COALESCE_SKIPPED_MAXIMAL.load(O::Relaxed);
            if calls > 0 || skipped_maximal > 0 {
                eprintln!(
                    "[GC] oldgen_coalesce: calls={calls} blocks_merged={merged} \
                     coalesce_skipped_maximal={skipped_maximal}"
                );
            }
            // The old-gen live-set closure and the compaction's stale-grid
            // re-walk (lane C, design item 5): readable only from a debugger
            // until this line. `compact_stale_grid_rewalks` is expected to stay
            // 0. `close_live_set_multi_pass_calls` / `_deep_calls` are the calls
            // on which the pre-round-4 fixpoint ran >= 2 / >= 3 full passes, so
            // the two against `close_live_set_calls` price the change.
            let cl_calls = crate::old_gen::CLOSE_LIVE_SET_CALLS.load(O::Relaxed);
            let cl_rescued = crate::old_gen::CLOSE_LIVE_SET_RESCUED.load(O::Relaxed);
            let cl_multi = crate::old_gen::CLOSE_LIVE_SET_MULTI_PASS_CALLS.load(O::Relaxed);
            let cl_deep = crate::old_gen::CLOSE_LIVE_SET_DEEP_CALLS.load(O::Relaxed);
            let cl_scans = crate::old_gen::CLOSE_LIVE_SET_WORKLIST_SCANS.load(O::Relaxed);
            let stale_rewalks = crate::old_gen::COMPACT_STALE_GRID_REWALKS.load(O::Relaxed);
            if cl_calls | stale_rewalks != 0 {
                eprintln!(
                    "[GC] oldgen_closure: close_live_set_calls={cl_calls} \
                     close_live_set_rescued={cl_rescued} \
                     close_live_set_multi_pass_calls={cl_multi} \
                     close_live_set_deep_calls={cl_deep} \
                     close_live_set_worklist_scans={cl_scans} \
                     compact_stale_grid_rewalks={stale_rewalks}"
                );
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
                 deferred_cards={} abandoned_old_bytes={} par_evac_overflow_promotions={} \
                 cheney_overflow_promotions={}",
                c.cycles,
                c.helper_scans,
                c.cas_losses,
                c.declined_for_slack,
                c.filler_bytes,
                c.promotions,
                c.deferred_cards,
                // gen r4w2/obs: old gen filled over a CAS loser's abandoned
                // promotion (kept since round 1, never printed), and survivors
                // tenured early because the reserved to-space region ran out
                // (lane A, round 4).
                c.abandoned_old_bytes,
                c.overflow_promotions,
                // gcj j3/yg3: the serial Cheney copy's twin of the last one
                // (`CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES`).
                crate::gen_heap::CHENEY_OVERFLOW_PROMOTIONS
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        let fallbacks = crate::gc_quiescence::moving_young_coverage_fallback_count();
        // GENERATIONAL ONLY (2026-09-23). The line describes the moving YOUNG
        // generation, which only this backend has, and its totals come from the
        // decision histogram — which G1 has always fed
        // (`moving-backend-always-evacuates`, so every G1 pause was counted in
        // `moving_cycles_total`) and which ZGC feeds since 2026-09-23. The gate
        // below is `moving_young_enabled()`, a FLAG that defaults on for every
        // backend, so on a G1 or ZGC run this line printed that backend's
        // pauses under the generational young collector's name. The histogram
        // itself, with per-backend reason labels, is in
        // `collector_decision_report()`.
        let generational = matches!(self, VmHeap::Generational(_));
        if generational && (crate::gc_quiescence::moving_young_enabled() || fallbacks > 0) {
            // Both numbers, always. A correct answer while the moving count is
            // zero means the young generation never actually copied anything,
            // which is the exact way the 2026-07-01 validation declared
            // moving-young working while it was inert (see
            // `moving-young-corruption-rootcause.md` section 6). The histogram
            // then names what stopped it.
            //
            // THE KEY WAS `cycles=`, AND IT WAS NOT THE NUMBER OF MOVING CYCLES
            // (renamed 2026-09-20). `gc_quiescence::record_moving_young_cycle`
            // has one caller, `gen_heap::collect_garbage_inner`, and it sits
            // behind `if moving_young && has_conservative_roots` -- so it counts
            // only the moving cycles taken WHILE A JIT FRAME WAS LIVE. A run
            // with no compiled frames can relocate on every single collection
            // and still print `cycles=0 coverage_fallbacks=0`, which reads as
            // "the young generation never moved" and is the precise inverse of
            // the truth. `gc_metrics::collector_decision_report` has always
            // called the same number `moving_cycles_under_live_jit`; this line
            // did not, and `docs/GC.md` told operators to read this line.
            //
            // The honest totals come from the decision histogram, which is
            // bumped by `record_collector_decision` on BOTH arms of the young
            // path -- once per generational collection, moving or not.
            let under_jit = crate::gc_quiescence::moving_young_cycle_count();
            // `refused_cycles_total` is a THIRD term, added 2026-09-21. A
            // young cycle that refuses to run at all (an unparseable
            // from-space object-start layout, or a live mutator's reserved
            // TLAB tail inside the arena the cycle was about to reset) ran
            // NEITHER collector, and folding it into
            // `non_moving_cycles_total` claims the non-moving sweep
            // reclaimed something. A heap wedged in a refusal loop and an
            // idle heap used to print identical lines here; now they do
            // not, and `collector_decision_report()`'s histogram names
            // WHICH of the two refusal causes fired.
            let (moving_total, non_moving_total, refused_total, _) =
                crate::gc_metrics::decision_histogram();
            eprintln!(
                "[GC] moving_young: moving_cycles_total={moving_total} \
                 non_moving_cycles_total={non_moving_total} \
                 refused_cycles_total={refused_total} \
                 moving_cycles_under_live_jit={under_jit} \
                 coverage_fallbacks={fallbacks}"
            );
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
        self.refill_tlab_at_least(requested_size, 0)
    }

    /// [`Self::refill_tlab`], carving at least `min_size` bytes or nothing
    /// (gcd d1/d, `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object-FIXED-20261012.md`
    /// clamp 2): the VM's refill site passes the footprint of the object that
    /// missed, so a chunk that could not hold it is never carved. `0` is
    /// [`Self::refill_tlab`].
    ///
    /// Every backend: Generational (gcd d1/d), G1 (gch h4/g1) and ZGC (wave 9,
    /// lane gcI, `ZgcRealHeap::refill_tlab_at_least`).
    pub fn refill_tlab_at_least(
        &self,
        requested_size: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        let chunk = match self {
            VmHeap::Generational(h) => h.refill_tlab_at_least(requested_size, min_size),
            // gch h4/g1: G1 takes the floor too (`G1Collector::refill_tlab_at_least`).
            VmHeap::G1(h) => h.refill_tlab_at_least(requested_size, min_size),
            // Wave 9, lane gcI: and ZGC (`ZgcRealHeap::refill_tlab_at_least`).
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.refill_tlab_at_least(requested_size, min_size),
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

    /// HIB-CV-24 (ZGC manifestation): arm a class-unload reconciliation watch
    /// (see `zgc::zgc_reconcile_watch_map`) for the given pre-collection
    /// addresses, so their EXACT pre-slide mark-bitmap verdict can be read
    /// back after the cycle via [`Self::zgc_take_reconcile_results`] instead
    /// of the post-slide, compaction-aliasable `is_addr_live`. No-op on
    /// Generational/G1: the historical `gen_heap.rs` fix already covers
    /// Generational, and G1's own remark-time `is_marked` (see
    /// `g1_remark_process_references`) is already exact for this consumer.
    pub fn zgc_set_reconcile_watch(&self, addrs: Vec<usize>) {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.zgc_set_reconcile_watch(addrs),
        }
    }

    /// Take back this cycle's captured exact verdicts armed via
    /// [`Self::zgc_set_reconcile_watch`]. `None` on Generational/G1 (nothing
    /// to take) or if nothing was armed for ZGC.
    pub fn zgc_take_reconcile_results(&self) -> Option<std::collections::HashMap<usize, bool>> {
        match self {
            VmHeap::Generational(_) | VmHeap::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.zgc_take_reconcile_results(),
        }
    }

    /// gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9): is
    /// `addr` (a PRE-collection address) a watched young survivor that the
    /// last Generational true-root major left out of its trace, i.e. dead
    /// although still in place? See
    /// `GenerationalHeap::true_root_watched_left_out`. Always `false` on G1 and
    /// ZGC (no true-root young seed), and with the switch off. Since gci i3/og
    /// also installed by a MOVING true-root major (P-C) under
    /// `CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD` (opt-in).
    pub fn true_root_watched_left_out(&self, addr: usize) -> bool {
        match self {
            VmHeap::Generational(h) => h.true_root_watched_left_out(addr),
            VmHeap::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => false,
        }
    }

    /// gch h4/wr: [`Self::true_root_watched_left_out`]'s whole verdict,
    /// ascending (empty on G1 / ZGC and on almost every pause), for a caller
    /// that judges many addresses in one window.
    pub fn true_root_watched_left_out_snapshot(&self) -> Vec<usize> {
        match self {
            VmHeap::Generational(h) => h.true_root_watched_left_out_snapshot(),
            VmHeap::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => Vec::new(),
        }
    }

    /// gci i27/gr (`jr20-g1-young-pause-keeps-every-referent-in-a-jit-pinned-region-FIXED-20261016`):
    /// is the last G1 collection an evacuation pause that kept a young region
    /// in place, with no collection since? One load; `false` off G1. The VM
    /// asks before it gathers candidates for
    /// [`Self::g1_pinned_young_referents_unheld`].
    pub fn g1_pinned_referent_verdict_armed(&self) -> bool {
        match self {
            VmHeap::G1(h) => h.pinned_referent_verdict_armed(),
            _ => false,
        }
    }

    /// gci i27/gr: G1's per-object verdict for addresses in the young regions
    /// the last pause kept in place: which `candidates` (PRE-collection
    /// addresses the pause neither moved nor identity-mapped) nothing holds.
    /// `roots` are the pause's root words as the collection left them. The
    /// region-granular arm of [`Self::watched_pre_gc_addr_survived`] answers
    /// "live" for every one of them; see
    /// `G1Collector::pinned_young_referents_unheld` for what "holds" covers and
    /// why every doubt declines. `Err(NotArmed)` off G1.
    pub fn g1_pinned_young_referents_unheld(
        &self,
        candidates: &[usize],
        roots: &[usize],
    ) -> Result<crate::g1::G1PinnedReferentVerdict, crate::g1::G1PinnedReferentDecline> {
        match self {
            VmHeap::G1(h) => h.pinned_young_referents_unheld(candidates, roots),
            _ => {
                let _ = (candidates, roots);
                Err(crate::g1::G1PinnedReferentDecline::NotArmed)
            }
        }
    }

    /// gci i32/ec: G1's private closure of the dead finalizables the last
    /// kept-region verdict found (`dead`, unchanged addresses): the young
    /// objects (POST-collection addresses) only they reach. See
    /// `G1Collector::kept_finalizables_private_closure`. `Err(NotArmed)` off
    /// G1.
    pub fn g1_kept_finalizables_private_closure(
        &self,
        dead: &[usize],
        roots: &[usize],
    ) -> Result<crate::g1::G1KeptFinalizableClosure, crate::g1::G1PinnedReferentDecline> {
        match self {
            VmHeap::G1(h) => h.kept_finalizables_private_closure(dead, roots),
            _ => {
                let _ = (dead, roots);
                Err(crate::g1::G1PinnedReferentDecline::NotArmed)
            }
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
        // gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9):
        // a watched young survivor the last true-root major left out of its
        // trace is dead, though it is still in place. Asked FIRST, before the
        // map (its identity entry was removed, but this stays the one
        // authority) and before the permissive escape hatch below: once its
        // old referents may be freed, "live" would hand back a use-after-free.
        if self.true_root_watched_left_out(addr) {
            return false;
        }
        // gcj j1yg-pause: an OLD address on a cycle that reclaimed no old
        // storage is a survivor whatever the map holds for it (the arm below
        // answers the same) -- asked before the map, so the reference passes'
        // thousands of old rows each skip a hash probe into a map of every
        // young survivor.
        if let VmHeap::Generational(h) = self {
            if h.is_old_gen_addr(addr) && !crate::gc_quiescence::old_gen_reclaimed_last_cycle() {
                return true;
            }
        }
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
            // G1/ZGC: `is_addr_live` (live-region membership / registry
            // lookup); neither has a generation whose addresses are trusted
            // wholesale.
            //
            // gci i27/gr: G1's arm is exact only for a region the pause
            // EVACUATED (its survivors are map keys, so a non-key there is
            // dead). A young region the pause kept in place (a JIT or JNI pin,
            // an evacuation failure) keeps every object below its cursor, dead
            // or alive, and this answers "survived" for all of them. Reference
            // processing asks G1's per-object verdict for those first
            // (`g1_pinned_young_referents_unheld`, `process_references_after_gc`).
            // gci i28/wt: the JNI weak-global sweep refuses the same unheld
            // referents through the pause's `InPlaceVerdict::survived`, which
            // it ANDs with this; an address-keyed side-table row is refused
            // only when its owner is also one of the offered candidates -- see
            // `gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`.
            VmHeap::G1(_) => self.is_addr_live(addr),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => self.is_addr_live(addr),
        }
    }

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

    /// H2-CID0 — see [`GenerationalHeap::live_holders_of`]. Empty for every
    /// backend that does not publish one (G1).
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

    /// Is `addr` inside memory the collector has RECLAIMED?
    ///
    /// H2-CID0 — see [`GenerationalHeap::reclaimed_hole_at`] for why this
    /// exists: it is the flag-free discriminator between an ordinary
    /// `new Object()` and a reference into a span the collector freed and
    /// zeroed, which are otherwise indistinguishable at the point a
    /// `checkcast` fails with `java.lang.Object` as the actual class.
    ///
    /// G1 returns `None`: its liveness is region-based and `is_addr_live`
    /// already answers exactly, so there is no free-list view to consult. The
    /// ZGC arm's `None` rested on the same sentence and that half of it was
    /// wrong — see the arm's own comment below; it answers now.
    ///
    /// Cost: a young from-space address on Generational, and any arena address
    /// on ZGC, copies and sorts that arena's whole free list PER CALL. A caller
    /// that judges many addresses in one allocation-free window takes
    /// [`Self::reclaimed_hole_probe`] once and asks [`Self::is_reclaimed_hole`]
    /// (gc-common w16-x / w18-a; the stop-the-world epilogue's
    /// `addr_keyed::InPlaceVerdict`).
    //
    // Doc placement, 2026-09-20: this block, and `live_holders_of`'s, had both
    // slid onto `zgc_corpse_lookup` above, which is why the "G1/ZGC return
    // `None`" sentence outlived the 2026-08-17 ZGC arm that contradicts it —
    // nothing was reading it next to the code it described.
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

    /// Capture a [`ReclaimedHoleProbe`]. Stale once anything allocates, so
    /// only a stop-the-world epilogue may hold one.
    ///
    /// Per backend (gc-common w18-a: spelled out rather than `_ =>`, the rule
    /// this type's doc states):
    ///
    /// * Generational: the young free list and semispace geometry, once.
    /// * G1: nothing to capture; `reclaimed_hole_at` is `None` for free.
    /// * ZGC: nothing worth capturing for the `survived_in_place` verdict. Its
    ///   `reclaimed_hole_at` does rebuild the arena free list per call, but the
    ///   verdict never reaches it: ZGC's `is_object_address` and `is_addr_live`
    ///   are the same exact-registry test, so an address either fails the
    ///   first (dead, answered without the free list) or passes the second
    ///   (live). Should that ever change, capture the arena list here.
    pub fn reclaimed_hole_probe(&self) -> ReclaimedHoleProbe {
        match self {
            VmHeap::Generational(h) => ReclaimedHoleProbe::Generational(h.reclaimed_hole_probe()),
            VmHeap::G1(_) => ReclaimedHoleProbe::PerAddress,
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(_) => ReclaimedHoleProbe::PerAddress,
        }
    }

    /// `self.reclaimed_hole_at(addr).is_some()`, answered through `probe`.
    pub fn is_reclaimed_hole(&self, probe: &ReclaimedHoleProbe, addr: usize) -> bool {
        match (self, probe) {
            (VmHeap::Generational(h), ReclaimedHoleProbe::Generational(p)) => {
                h.is_reclaimed_hole_probed(p, addr)
            }
            _ => self.reclaimed_hole_at(addr).is_some(),
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

    /// Generational: the young addresses the last collection vacated -- the
    /// inactive semispace, plus a pinned in-place cycle's took-back spans (gen
    /// r5w5/pin9). G1/ZGC have neither, hence `None`.
    pub fn young_vacated_probe(&self) -> Option<crate::gen_heap::YoungVacatedProbe> {
        match self {
            VmHeap::Generational(h) => Some(h.young_vacated_probe()),
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
    /// Generational: every old-gen address, and — gc-common w36-d — a YOUNG
    /// address on exactly the cycles [`Self::mirror_pin_deferrable`] defers a
    /// young mirror on (`young_marker_follows_side_tables`: the cycle is
    /// certain to take the non-moving young precise marker), unless
    /// `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` opts out of both.
    ///
    /// Why a young value was refused until w36-d: the MOVING young copy
    /// closure seeds strictly from the direct root set and never consults
    /// `metadata_pin`, so a young address deferred on a moving cycle has no
    /// path to be marked; if it has no other reachability (a static field's
    /// value right after `<clinit>` assigns it, a class-lock / condy object)
    /// it is reclaimed and its memory reused — a live object that reads back
    /// as a DIFFERENT, unrelated type
    /// (`spb1-springframework-util-investigation-FIXED.md`, repro 3). The
    /// non-moving young precise marker is different: `scan_young_object`
    /// follows `metadata_pin` from every young owner it marks, and since w36-d
    /// `seed_metadata_pins_of_old_owners` seeds the values of every OLD owner
    /// (which no young cycle scans) — the precondition the w3-b handoff named.
    /// So on a cycle certain to take that marker a young value is reached
    /// through its loader, and deferring it is what lets the loader unload
    /// (`common-w2b-generational-young-classvalue-values-root-their-class`,
    /// `common-w16b-clv-values-are-permanent-roots-that-pin-their-loader`).
    ///
    /// G1 / ZGC: always `true` — both backends' `metadata_pin` consumers
    /// (`g1.rs`, `zgc.rs`) walk every live region uniformly during the same
    /// full-mark pass that activates `conditional_metadata`, so deferring a
    /// young-resident object is sound; this matches their existing,
    /// unconditional behavior and is unchanged here.
    pub fn metadata_pin_deferrable(&self, addr: usize) -> bool {
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
        // gch h4/wr: the strict sibling's first question, answered the same
        // way here (the two must not disagree about one address).
        if self.true_root_watched_left_out(addr) {
            return true;
        }
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

    /// gci i14/hd: [`Self::walk_objects`] as a visitor, the same objects in
    /// the same order under the same safepoint contract. G1 walks its regions
    /// without materialising the list (`G1Collector::for_each_object`: a heap
    /// dump or histogram of a large heap, at an `OutOfMemoryError` too, no
    /// longer allocates 16 bytes per object first); Generational lists only
    /// its young half and visits the old one in slices
    /// (`GenerationalHeap::for_each_object`, gci i14/hd second pass); ZGC
    /// still walks into a list and visits it.
    pub fn for_each_object(&self, f: &mut dyn FnMut(*mut u8, usize)) {
        match self {
            VmHeap::G1(h) => h.for_each_object(f),
            VmHeap::Generational(h) => h.for_each_object(f),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => {
                for (ptr, size) in h.walk_objects() {
                    f(ptr, size);
                }
            }
        }
    }

    /// gci i14/hd second pass (item 7): [`Self::for_each_object`] minus the
    /// objects the last trusted completed mark proves dead, on G1
    /// (`G1Collector::for_each_object_skipping_proven_dead`); the number
    /// skipped, or `None` (nothing visited) where no such proof exists -- off
    /// G1 always. The live-only heap walks' opt-in.
    pub fn for_each_object_skipping_proven_dead(
        &self,
        f: &mut dyn FnMut(*mut u8, usize),
    ) -> Option<u64> {
        match self {
            VmHeap::G1(h) => h.for_each_object_skipping_proven_dead(f),
            _ => None,
        }
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

    /// JIT round 23 wave 1 L7 (`CRATONVM_DBG=root-kind-census`, the VM's
    /// `memory::root_kind_census`): the object that starts at `addr` -- its
    /// total size and class id -- and every heap reference it holds (its
    /// reference slots, then its external-overlay edges, the two edge families
    /// a collector's mark follows), each passed to `visit`. `None`, and nothing
    /// visited, when `addr` is not an object start ([`Self::is_object_address`])
    /// or its header implies an implausible size. Read-only; must run with the
    /// world stopped (the same contract as [`Self::walk_objects`]).
    pub fn dbg_object_size_and_refs(
        &self,
        addr: usize,
        visit: &mut dyn FnMut(usize),
    ) -> Option<(usize, u32)> {
        let obj = self.is_object_address(addr)?;
        let base = obj.as_ptr();
        // SAFETY: `is_object_address` accepted `addr` as the start of a live
        // object of this heap, so its header bytes are in bounds; the world is
        // stopped (the caller's contract), so nothing rewrites them.
        let header = unsafe { &*(base as *const ObjectHeader) };
        let size = crate::gc::object_total_size(header);
        if size < HEADER_SIZE {
            return None;
        }
        // SAFETY: as above; the slot ranges `header` implies are inside the
        // object's extent (`for_each_ref_slot` also caps an array's length).
        unsafe {
            crate::gen_heap::for_each_ref_slot(base, header, |r, _| visit(r as usize));
        }
        let class_id = header.class_id.as_u32();
        crate::external_roots::with_external_roots_for_owner(addr, Some(class_id), |refs| {
            for r in refs {
                visit(r.as_ptr() as usize);
            }
        });
        Some((size, class_id))
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
        let _gpu_drain = wait_for_gpu_critical_drain();
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
        drop(_gpu_drain);
        assert!(!gpu_relocation_forbidden());
    }

    /// zr z4/ab (`common-watched-referent-set-is-per-thread-not-per-heap-20261012.md` §4): VM B's
    /// cycle ending must not clear the veto VM A's cycle raised while A's
    /// cycle is still running. Before z4 the veto was one process flag that
    /// any `after_collection` stored `false`.
    #[test]
    fn another_cycles_end_does_not_clear_this_cycles_veto() {
        let (vetoed_tx, vetoed_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        // Cycle A, on its own collecting thread: raises the veto mid-cycle.
        let a = std::thread::spawn(move || {
            let cycle = gpu_coordination::before_collection();
            gpu_coordination::forbid_relocation_this_cycle();
            vetoed_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            let still = gpu_relocation_forbidden();
            cycle.after_collection(&[]);
            still
        });
        vetoed_rx.recv().unwrap();
        // Cycle B, here: starts and ends while A is mid-cycle.
        let b = gpu_coordination::before_collection();
        assert!(gpu_relocation_forbidden(), "A's veto covers B's cycle too");
        b.after_collection(&[]);
        assert!(
            gpu_relocation_forbidden(),
            "B's cycle end cleared A's veto while A was still running"
        );
        done_tx.send(()).unwrap();
        assert!(a.join().unwrap(), "A saw its own veto until its end");
        assert!(!gpu_relocation_forbidden(), "A's end withdrew it");
    }
}

/// Render the `[GC] tlab-guard:` line for the shutdown summary.
///
/// `line` is [`crate::tlab::tlab_census_lines`]'s second element; `urgent` is
/// [`crate::tlab::tlab_guard_line_is_urgent`], i.e. "at least one tripwire
/// fired".
///
/// # Why the two cases are rendered differently
///
/// `gengc-alloc-tlab-instruments-are-write-only-FIXED-20260924.md` filed these two
/// counters as write-only, and the reason a write-only detector is worse than
/// no detector is that its silence reads as a green light. Printing the line
/// at all fixes half of that; the other half is that a fired tripwire must not
/// look like one more gauge in a block of thirty gauges. So:
///
/// * not urgent — the bare `key=value` line, which is the "the tripwire says
///   no" reading that a missing line cannot give you;
/// * urgent — the SAME `key=value` prefix (so a grep for
///   `filler_over_object=` keeps working either way) plus a suffix naming the
///   hazard.
///
/// Split out of `print_gc_summary` so the shape is testable without a heap and
/// without depending on the process-wide counters, which `cargo test` races.
fn render_tlab_guard_line(line: &str, urgent: bool) -> String {
    if !urgent {
        return line.to_string();
    }
    format!(
        "{line} — TRIPWIRE FIRED: a TLAB tail was filled over a word that \
         still held an object (`filler_over_object`), or a refill handed a \
         thread a chunk that already held one (`refill_over_object`). Either \
         way a live object is buried and will be read back as an all-zero \
         header. This is a use-after-free in waiting, not tuning — the \
         per-event detail is on `tracing` target `cratonvm::gc::guard` (the \
         first eight occurrences and then powers of two only, so this count is \
         the authoritative total); see \
         docs/feature-designs/generative-gc-architecture.md"
    )
}

/// The `getCollectionTime()` figure of any source that keeps a microsecond
/// pause SUM — G1's accumulator, the Generational fallback, and the per-VM
/// pause-duration accumulator ZGC answers from (gc-common w6-f): the whole
/// milliseconds of the sum, converted once, as HotSpot's
/// `GCMemoryManager::_accumulated_timer` (and `gc_metrics::serial_collectors`
/// since gen r5w3/obs7).
///
/// gce e1/o (`gce-e1o-g1-zgc-collection-time-adds-a-millisecond-per-pause`):
/// this added one millisecond per collection so the figure moved on every
/// pause, which on ZGC's `ZGC Pauses` bean made the time roughly the pause
/// COUNT (1 000 pauses of 100 µs read 1 100 ms where HotSpot reads 100). A
/// sub-millisecond pause now leaves it unchanged, as on HotSpot; a caller
/// polling it for change (H2's `Utils.collectGarbage()`) sees it move once the
/// summed pauses cross the next millisecond.
pub fn pause_sum_as_collection_time_ms(total_pause_us: u64) -> u64 {
    total_pause_us / 1_000
}

#[cfg(test)]
mod header_if_object_start_tests {
    use super::*;

    /// Interpreter round i1 wave 51, lane L7: the one-call header read
    /// answers what the two `_validated` twins answer for an object start on
    /// every backend, and nothing for an address the validator refuses.
    #[test]
    fn i51_l7_one_header_read_agrees_with_the_twins() {
        const MB: usize = 1024 * 1024;
        #[allow(unused_mut)]
        let mut heaps = vec![
            VmHeap::new(GcBackend::Generational, 16 * MB),
            VmHeap::new(GcBackend::G1, 64 * MB),
        ];
        #[cfg(feature = "zgc")]
        heaps.push(VmHeap::new(GcBackend::Zgc, 16 * MB));
        for heap in &heaps {
            let obj = heap.alloc_object(ClassId::new(7), 2);
            let arr = heap.alloc_array(ClassId::new(9), ArrayElementType::Long, 3);
            for o in [obj, arr] {
                let h = heap
                    .header_if_object_start(o.as_ptr() as usize)
                    .expect("an object start");
                assert_eq!(h.kind(), heap.kind_of_validated(o));
                assert_eq!(h.class_id, heap.class_id_of_validated(o));
                if h.kind() == ObjectKind::Array {
                    assert_eq!(h.element_type(), heap.element_type_of_validated(o));
                }
            }
            // Exactly the validator's answer elsewhere (an interior word,
            // null, a misaligned address).
            for addr in [obj.as_ptr() as usize + 8, arr.as_ptr() as usize + 4, 0] {
                assert_eq!(
                    heap.header_if_object_start(addr).is_some(),
                    heap.is_object_start(addr),
                    "addr {addr:#x}"
                );
            }
            assert!(heap.header_if_object_start(0).is_none());
        }
    }
}

#[cfg(test)]
mod collection_time_tests {
    use super::*;

    /// gce e1/o: the JMX collection time is the floor of the microsecond
    /// pause sum (HotSpot's accumulated timer), not the sum plus one
    /// millisecond per collection.
    #[test]
    fn collection_time_is_the_floor_of_the_pause_sum() {
        assert_eq!(pause_sum_as_collection_time_ms(0), 0);
        // A sub-millisecond pause does not move it (HotSpot's behaviour)...
        assert_eq!(pause_sum_as_collection_time_ms(400), 0);
        // ...the sum does, once it crosses the next millisecond.
        assert_eq!(pause_sum_as_collection_time_ms(1_200), 1);
        // 1 000 pauses of 100 µs: 100 ms, not 1 100.
        assert_eq!(pause_sum_as_collection_time_ms(1_000 * 100), 100);
        assert_eq!(pause_sum_as_collection_time_ms(125_400), 125);
        assert_eq!(pause_sum_as_collection_time_ms(u64::MAX), u64::MAX / 1_000);
    }

    /// A fresh heap reports zero where the backend keeps time, and ZGC —
    /// which keeps none — says so rather than inventing a figure.
    #[test]
    fn a_fresh_heap_reports_zero_time_or_none() {
        const MB: usize = 1024 * 1024;
        let generational = VmHeap::new(GcBackend::Generational, 16 * MB);
        assert_eq!(generational.collection_time_ms(), Some(0));
        let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
        assert_eq!(g1.collection_time_ms(), Some(0));
        assert_eq!(g1.collection_count(), 0);
        #[cfg(feature = "zgc")]
        assert_eq!(
            VmHeap::new(GcBackend::Zgc, 16 * MB).collection_time_ms(),
            None
        );
    }

    /// gc-common w7-f: the dispatcher's JMX half. G1 carries its bean state in
    /// the heap (per heap, so per VM), and its pool samples are three valid
    /// `MemoryUsage`s that partition the committed heap; Generational has no
    /// backend state (its beans are its own); ZGC samples its one pool (its
    /// bean state lives in the VM's `HeapRealm`).
    #[test]
    fn w7f_backend_bean_state_and_pool_samples() {
        use crate::gc_metrics::BackendBeanShape;
        const MB: usize = 1024 * 1024;
        let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
        let beans = g1.backend_gc_beans().expect("G1 books its beans");
        assert_eq!(beans.shape(), BackendBeanShape::G1);
        let samples = g1.backend_pool_samples();
        assert_eq!(samples.len(), BackendBeanShape::G1.pools().len());
        for s in &samples {
            assert!(s.used <= s.committed, "{s:?}");
            assert!(s.max.map_or(true, |m| s.committed <= m), "{s:?}");
        }
        let committed: u64 = samples.iter().map(|s| s.committed).sum();
        assert!(
            committed >= g1.committed_bytes() as u64,
            "the three G1 pools cover the committed heap ({committed} < {})",
            g1.committed_bytes()
        );
        let pools = beans.pools(&samples);
        assert_eq!(pools[2].name, "G1 Old Gen");
        assert_eq!(pools[2].max, Some(g1.heap_capacity() as u64));
        // A second heap has its own state: per heap, never shared.
        let other = VmHeap::new(GcBackend::G1, 64 * MB);
        beans.note_collection(1_000);
        assert_eq!(beans.collectors()[0].count, 1);
        assert_eq!(other.backend_gc_beans().unwrap().collectors()[0].count, 0);

        let generational = VmHeap::new(GcBackend::Generational, 16 * MB);
        assert!(generational.backend_gc_beans().is_none());
        assert!(generational.backend_pool_samples().is_empty());
        #[cfg(feature = "zgc")]
        {
            let z = VmHeap::new(GcBackend::Zgc, 16 * MB);
            assert!(z.backend_gc_beans().is_none(), "ZGC's state is the VM's");
            let s = z.backend_pool_samples();
            assert_eq!(s.len(), BackendBeanShape::Zgc.pools().len());
            assert_eq!(s[0].used, 0, "nothing allocated yet");
            assert_eq!(s[0].init, Some(0));
            assert!(
                s[1].init.is_some(),
                "the old pool carries the startup commit"
            );
            assert!(s[1].used <= s[1].committed);
            // zr z3/t2: under `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE` (default
            // on) old is `committed == used` and young carries the rest of the
            // heap's commit; under `=0` young is empty and old carries it all.
            // Either way the pools' committed sum to the heap's, as HotSpot 25
            // ZGC's do (`Zr3T2HeapVsPools`, host).
            assert_eq!(
                s[0].committed + s[1].committed,
                z.committed_bytes() as u64,
                "pools' committed sum to the heap's"
            );
        }
    }

    /// gci i14/mp finding 4: a G1 humongous array checks `G1 Old Gen`'s usage
    /// threshold at once (HotSpot's `detect_low_memory_for_collected_pools`
    /// after the allocation); nothing is checked while no threshold is armed,
    /// and an ordinary (young) array does not check it.
    #[test]
    fn gci_i14_mp_a_g1_humongous_array_checks_the_old_usage_threshold() {
        use crate::gc_metrics::{PoolThresholdKind, JMX_POOL_G1_OLD};
        const MB: usize = 1024 * 1024;
        let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
        let beans = g1.backend_gc_beans().expect("G1 books its beans");
        let thresholds = beans.notifications().pool_thresholds();
        // Unarmed: a humongous allocation queues nothing.
        let big = g1.try_alloc_array(ClassId::new(1), ArrayElementType::Byte, 4 * MB);
        assert!(big.is_some(), "a 4 MiB array fits a 64 MiB G1 heap");
        assert!(!thresholds.has_pending());
        // Armed below the old pool's usage now: the next humongous array
        // crosses it without a collection.
        thresholds.set(JMX_POOL_G1_OLD, PoolThresholdKind::Usage, 1);
        let small = g1.try_alloc_array(ClassId::new(1), ArrayElementType::Byte, 64);
        assert!(small.is_some());
        assert!(!thresholds.has_pending(), "a young array does not grow the old pool");
        let big2 = g1.try_alloc_array(ClassId::new(1), ArrayElementType::Byte, 4 * MB);
        assert!(big2.is_some());
        assert!(thresholds.has_pending(), "the humongous array crossed the usage threshold");
        let requests = thresholds.take_requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].pool, JMX_POOL_G1_OLD);
        assert!(requests[0].trigger_usage.is_some(), "a trigger, not a clear");
    }

    /// zr z3/t2 (host follow-up): the stall classification is the door's.
    /// A failure site is a stall even with a trigger rule pending (the z3a
    /// regression: `stall_total=0` at `-Xmx48m`); an occupancy site is not;
    /// `=0` counts every door but the refill trigger's; off ZGC never.
    #[cfg(feature = "zgc")]
    #[test]
    fn zr3t2_rule_pending_and_a_failure_site_is_a_stall() {
        use cratonvm_types::ClassId;
        let z = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
        let VmHeap::Zgc(h) = &z else { unreachable!() };
        let mut held = Vec::new();
        for _ in 0..40_000 {
            if h.needs_gc() {
                break;
            }
            held.push(h.alloc_object(ClassId::new(1), 64));
        }
        assert!(h.needs_gc(), "a rule is pending");
        assert!(
            z.zgc_forced_collection_is_stall(false, false),
            "failure site"
        );
        assert_eq!(z.zgc_forced_door_gc_cause(false), Some("Allocation Stall"));
        assert!(
            !z.zgc_forced_collection_is_stall(true, false),
            "occupancy site"
        );
        assert!(
            !z.zgc_forced_collection_is_stall(true, true),
            "refill trigger"
        );
        assert_ne!(z.zgc_forced_door_gc_cause(true), Some("Allocation Stall"));
        h.counters_for_test_set_stall_classification(false);
        assert!(
            z.zgc_forced_collection_is_stall(true, false),
            "=0: z2 reading"
        );
        assert!(
            !z.zgc_forced_collection_is_stall(true, true),
            "=0: refill excluded"
        );
        drop(held);
        let g1 = VmHeap::new(GcBackend::G1, 64 * 1024 * 1024);
        assert!(!g1.zgc_forced_collection_is_stall(false, false));
        assert_eq!(g1.zgc_forced_door_gc_cause(false), None);
    }

    /// zr z3/t2: HotSpot 25 ZGC's young committed (host, -Xmx256m:
    /// heap 268435456, old 4194304 -> young 264241152), never below young
    /// used, and saturating when old exceeds the commit.
    #[cfg(feature = "zgc")]
    #[test]
    fn zr3t2_zgc_young_committed_is_heap_less_old() {
        const M: usize = 1024 * 1024;
        assert_eq!(
            super::zgc_young_pool_committed(256 * M, 2 * M, 4 * M),
            252 * M
        );
        assert_eq!(super::zgc_young_pool_committed(8 * M, 6 * M, 4 * M), 6 * M);
        assert_eq!(super::zgc_young_pool_committed(4 * M, 1 * M, 8 * M), 1 * M);
    }

    /// zr z3/t2: on a heap with survivors and fresh garbage the two pools
    /// still sum to the heap's committed, and old stays `committed == used`.
    #[cfg(feature = "zgc")]
    #[test]
    fn zr3t2_zgc_pools_sum_to_heap_committed_after_a_collection() {
        use cratonvm_types::ClassId;
        let z = VmHeap::new(GcBackend::Zgc, 32 * 1024 * 1024);
        let VmHeap::Zgc(h) = &z else { unreachable!() };
        let keep = h.alloc_object(ClassId::new(1), 8);
        for _ in 0..2_000 {
            let _ = h.alloc_object(ClassId::new(1), 8);
        }
        struct NoMonitors;
        impl crate::collector::MonitorCleanup for NoMonitors {
            fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
        }
        // SAFETY: single-threaded unit test.
        let stw = unsafe { crate::collector::StopTheWorldToken::new() };
        let mut roots = [keep];
        let _ = crate::collector::GarbageCollector::collect_garbage(
            h.as_ref(),
            &stw,
            &mut roots,
            &NoMonitors,
        );
        for _ in 0..500 {
            let _ = h.alloc_object(ClassId::new(1), 8);
        }
        let s = z.backend_pool_samples();
        if h.jmx_young_old_split().is_some() {
            assert_eq!(s[1].committed, s[1].used, "old: committed == used");
            assert!(s[0].committed >= s[0].used);
        }
        assert_eq!(
            s[0].committed + s[1].committed,
            (z.committed_bytes() as u64).max(s[0].used + s[1].used)
        );
    }
}

#[cfg(test)]
mod tlab_guard_line_tests {
    use super::render_tlab_guard_line;

    /// The urgent rendering must keep the routine one's grep surface and add
    /// to it — never replace it. A reader's `grep filler_over_object=` has to
    /// match in BOTH states, or the only run that matters is the one where the
    /// grep stops working.
    #[test]
    fn the_urgent_guard_line_extends_the_routine_one_rather_than_replacing_it() {
        let routine = "[GC] tlab-guard: filler_over_object=0 refill_over_object=0";
        assert_eq!(render_tlab_guard_line(routine, false), routine);

        let fired = "[GC] tlab-guard: filler_over_object=3 refill_over_object=0";
        let urgent = render_tlab_guard_line(fired, true);
        assert!(
            urgent.starts_with(fired),
            "the urgent line must begin with the exact routine line so the same \
             grep matches both: {urgent}"
        );
        assert!(
            urgent.len() > fired.len(),
            "a fired tripwire that renders identically to a quiet one is the \
             write-only instrument this page was filed over, one step removed"
        );
        assert!(
            urgent.contains("TRIPWIRE FIRED"),
            "the urgent marker is what a soak log greps for: {urgent}"
        );
    }

    /// The real formatter and this renderer must agree: `tlab.rs` owns the
    /// wording of the line, `vm_heap.rs` owns only the suffix. If the census
    /// ever stops producing a `tlab-guard` line, the suffix would be pinned to
    /// nothing and this test says so rather than letting the summary print a
    /// decorated empty string.
    #[test]
    fn the_renderer_is_fed_by_the_real_census_formatter() {
        let [_, guard] = crate::tlab::tlab_census_lines();
        assert!(
            guard.starts_with("[GC] tlab-guard: filler_over_object="),
            "unexpected census wording, which `print_gc_summary` prints verbatim: {guard}"
        );
        assert!(render_tlab_guard_line(&guard, true).starts_with(&guard));
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
        // `tlab.rs` and `evac_pool.rs` are in the corpus as well as this file.
        // `tlab_census_lines()` and `evac_pool_census_line()` FORMAT `[GC]`
        // lines there and `print_gc_summary` PRINTS them, so they are part of
        // the same summary and owe the same uniqueness — but scanning only
        // this file would never see them, and a key colliding across that seam
        // is precisely the failure this test exists for. Any future summary
        // line formatted outside `vm_heap.rs` belongs here too.
        //
        // Establish the corpus before concluding anything from it: a file that
        // stopped carrying the summary would make this pass vacuously.
        let mut lines: Vec<&str> = Vec::new();
        for src in [
            include_str!("vm_heap.rs"),
            include_str!("tlab.rs"),
            include_str!("evac_pool.rs"),
        ] {
            for (i, _) in src.match_indices("\"[GC]") {
                let rest = &src[i + 1..];
                if let Some(end) = rest.find('"') {
                    lines.push(&rest[..end]);
                }
            }
        }
        // gen r4w2/obs: `print_gc_summary` also prints
        // `GenerationalHeap::young_trigger_census_line()`, formatted in
        // `gen_heap.rs` (`YoungTriggerCensus::line`). Only THAT literal is
        // taken from `gen_heap.rs` — the file's other `"[GC]` strings are
        // per-collection lines (`--verbose:gc`), not the summary.
        // gen r4w4/alloc4: and `GenerationalHeap::alloc_path_census_line()`
        // (`[GC] gen-alloc:`), printed right after it.
        // gcd d9/b: and `young_trigger_wedged_census_line()` /
        // `oome_ladder_census_line()`.
        for prefix in [
            "\"[GC] young_trigger:",
            "\"[GC] gen-alloc:",
            "\"[GC] young_trigger_wedged:",
            "\"[GC] oome_ladder:",
            "\"[GC] pinned_copy_defrag:",
            "\"[GC] young_premium:",
            // gcj j1/yg: `term4_offer_census_line()`.
            "\"[GC] young_term4_offer:",
            // gcj yg3-residuals R3: `young_geometry_census_line()`.
            "\"[GC] young_geometry:",
        ] {
            let gen_src = include_str!("gen_heap.rs");
            let starts: Vec<usize> = gen_src.match_indices(prefix).map(|(i, _)| i).collect();
            // (A unit test in gen_heap.rs asserts on the rendered prefix; that
            // literal carries no `key={` and adds nothing to the scan.)
            assert!(
                !starts.is_empty(),
                "no {prefix} format string in gen_heap.rs -- the corpus would \
                 silently stop covering that summary line"
            );
            for i in starts {
                let rest = &gen_src[i + 1..];
                if let Some(end) = rest.find('"') {
                    lines.push(&rest[..end]);
                }
            }
        }
        // gen r4w3/obs2: likewise `GcNotificationCensus::summary_line`, the
        // one `[GC]` summary line `gc_metrics.rs` formats for this function.
        {
            let metrics_src = include_str!("gc_metrics.rs");
            let starts: Vec<usize> = metrics_src
                .match_indices("\"[GC] gc_notifications:")
                .map(|(i, _)| i)
                .collect();
            assert!(
                !starts.is_empty(),
                "no `[GC] gc_notifications:` format string in gc_metrics.rs -- the \
                 corpus would silently stop covering that summary line"
            );
            for i in starts {
                let rest = &metrics_src[i + 1..];
                if let Some(end) = rest.find('"') {
                    lines.push(&rest[..end]);
                }
            }
        }
        assert!(
            lines.len() > 20,
            "expected the [GC] summary to have many lines, found {} -- this scan is reading the wrong text and its verdict means nothing",
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
            "these [GC] summary keys appear on more than one line, so grepping the summary for them returns unrelated quantities and `| tail -1` picks an arbitrary one: {collisions:?}. Give the newer line's field a prefix of its own."
        );
    }

    /// gc-common w3-f (2026-09-23): no `[GC]` literal in this file carries a
    /// long run of spaces BETWEEN two words.
    ///
    /// Eleven did. Each was a `"… \` + newline + indent` continuation whose
    /// trailing backslash had been lost (a reflow joined the lines), so the
    /// indentation became part of the string: `[GC] zgc-trigger: stress=3
    /// live_bytes_threshold=…` followed by 22 spaces and the next key, on
    /// every exit census — and the same in one `tracing::warn!` message and
    /// five assertion messages, which this test does not scan. `split_whitespace` scrapers never noticed,
    /// which is why it survived; a human reading the census, or a `grep`
    /// for `key=value key2=`, did. Checked on the source because the lines
    /// are printed only at exit on one backend each.
    #[test]
    fn no_gc_literal_carries_a_collapsed_line_continuation() {
        let src = include_str!("vm_heap.rs");
        let mut bad: Vec<String> = Vec::new();
        for (i, _) in src.match_indices("\"[GC]") {
            let rest = &src[i + 1..];
            let Some(end) = rest.find('"') else { continue };
            for line in rest[..end].lines() {
                let body = line.trim();
                let (mut run, mut widest) = (0usize, 0usize);
                for ch in body.chars() {
                    if ch == ' ' {
                        run += 1;
                        widest = widest.max(run);
                    } else {
                        run = 0;
                    }
                }
                if widest >= 8 {
                    bad.push(body.chars().take(72).collect());
                }
            }
        }
        assert!(
            bad.is_empty(),
            "`[GC]` literal(s) with a mid-line run of spaces -- a line \
             continuation lost its trailing backslash: {bad:?}"
        );
    }

    /// Lines of `src` on which an ordinary (non-raw) string literal carries a
    /// run of 12+ spaces between two non-blank characters — the shape a `\`
    /// line continuation leaves when its backslash is lost (the newline goes,
    /// the next line's indentation stays IN the text). Comments, char
    /// literals and raw strings are skipped; a run right after a `\n` / `\t`
    /// escape is an intended line break's indentation and is not counted.
    fn collapsed_literal_lines(src: &str) -> Vec<usize> {
        literal_space_run_lines(src, false)
    }

    /// Lines of `src` on which an ordinary string literal carries a `\n`
    /// escape followed by a run of 12+ spaces and then text — a `\` line
    /// continuation TYPED AS `\n`, so the message prints a line break and the
    /// next source line's indentation (gc-common w6-f; the `zgc.rs`
    /// remembered-set error and the G1 `CLAMPED a holder's element walk`
    /// warning both had it). [`collapsed_literal_lines`] deliberately skips
    /// this shape, because a short run after `\n` is an intended break's
    /// indentation; 12+ spaces inside GC messages never was.
    fn typed_newline_continuation_lines(src: &str) -> Vec<usize> {
        literal_space_run_lines(src, true)
    }

    /// The shared lexer of the two scanners above: `after_newline_escape`
    /// selects which runs count (only those right after `\n`, or only those
    /// that are NOT after a `\n` / `\t` escape).
    fn literal_space_run_lines(src: &str, after_newline_escape: bool) -> Vec<usize> {
        const MIN_RUN: usize = 12;
        let b = src.as_bytes();
        let n = b.len();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
        let blank = |c: u8| c == b' ' || c == b'\n' || c == b'\t';
        let mut out: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < n {
            let c = b[i];
            if c == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if c == b'/' && b.get(i + 1) == Some(&b'*') {
                let mut depth = 1usize;
                i += 2;
                while i < n && depth > 0 {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            if c == b'r'
                && (i == 0 || !ident(b[i - 1]) || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
            {
                let mut j = i + 1;
                while j < n && b[j] == b'#' {
                    j += 1;
                }
                if j < n && b[j] == b'"' {
                    // `"` then as many `#` as opened it.
                    let mut close = vec![b'"'];
                    close.resize(j - i, b'#');
                    let mut k = j + 1;
                    while k < n && !b[k..].starts_with(&close) {
                        k += 1;
                    }
                    i = (k + close.len()).min(n);
                    continue;
                }
            }
            if c == b'\'' {
                if b.get(i + 1) == Some(&b'\\') {
                    let mut k = i + 3;
                    while k < n && b[k] != b'\'' {
                        k += 1;
                    }
                    i = (k + 1).min(n);
                    continue;
                }
                if i + 2 < n && b[i + 1] < 0x80 && b[i + 2] == b'\'' {
                    i += 3;
                    continue;
                }
                if i + 1 < n && b[i + 1] >= 0xc0 {
                    let mut j = i + 2;
                    while j < n && (0x80..=0xbf).contains(&b[j]) {
                        j += 1;
                    }
                    if j > i + 2 && j < n && b[j] == b'\'' && j - i <= 5 {
                        i = j + 1;
                        continue;
                    }
                }
                i += 1; // a lifetime
                continue;
            }
            if c == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < n && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = j.min(n);
                let mut k = start;
                while k < end {
                    if b[k] != b' ' {
                        k += 1;
                        continue;
                    }
                    let run_start = k;
                    while k < end && b[k] == b' ' {
                        k += 1;
                    }
                    let escaped_break = run_start >= start + 2
                        && (b[run_start - 2..run_start] == *b"\\n"
                            || b[run_start - 2..run_start] == *b"\\t");
                    let escaped_newline =
                        run_start >= start + 2 && b[run_start - 2..run_start] == *b"\\n";
                    let counted = if after_newline_escape {
                        escaped_newline
                    } else {
                        run_start > start && !blank(b[run_start - 1]) && !escaped_break
                    };
                    if k - run_start >= MIN_RUN && counted && k < end && !blank(b[k]) {
                        let line = 1 + b[..run_start].iter().filter(|&&x| x == b'\n').count();
                        if out.last() != Some(&line) {
                            out.push(line);
                        }
                    }
                }
                i = end + 1;
                continue;
            }
            i += 1;
        }
        out.dedup();
        out
    }

    /// gc-common w4-f — the widened guard of
    /// `common-w3f-collapsed-string-continuations-in-gc-messages-FIXED-20260923.md`:
    /// every `.rs` file under `gc/src`, every literal (not only `"[GC]` ones).
    /// W4-F collapsed ~140 such lines in `zgc.rs`, `g1.rs`, `gen_heap.rs`,
    /// `g1_concurrent.rs`, `g1_cards.rs` and `region.rs`; the three files
    /// below belonged to another lane that wave and are frozen at their
    /// count (a ratchet: lower it when a file is cleaned). Since w5-f it also
    /// scans `gc/tests`.
    #[test]
    fn no_gc_crate_literal_carries_a_collapsed_line_continuation() {
        const BASELINE: &[(&str, usize)] = &[];
        fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read gc/src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect(&root, &mut files);
        assert!(files.len() > 20, "fixture: gc/src must be found ({root:?})");
        let mut bad: Vec<String> = Vec::new();
        for path in files {
            let src = std::fs::read_to_string(&path).expect("read a gc/src file");
            let lines = collapsed_literal_lines(&src);
            let rel = path
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let allowed = BASELINE
                .iter()
                .find(|(f, _)| *f == rel)
                .map_or(0, |(_, n)| *n);
            if lines.len() > allowed {
                bad.push(format!("{rel}: lines {lines:?} (allowed {allowed})"));
            }
            // gc-common w6-f: the `\n`-typed continuation, no allowance.
            let typed = typed_newline_continuation_lines(&src);
            if !typed.is_empty() {
                bad.push(format!(
                    "{rel}: `\\n`-typed continuation on lines {typed:?}"
                ));
            }
        }
        // gc-common w5-f: the crate's integration tests too (their assertion
        // messages are what a red CI run prints). Six such lines were collapsed
        // in w5-f; the one allowed site is a deliberately aligned `println!`
        // (`per-worker scanned             = …` lines up under the `=` of the
        // `per-worker bytes (driver first) = …` line above it).
        const TESTS_ALLOWED: &[(&str, usize)] = &[("g1_w6p_mixed_parallel_equivalence.rs", 1)];
        let tests_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
        let mut test_files = Vec::new();
        collect(&tests_root, &mut test_files);
        assert!(
            test_files.len() > 20,
            "fixture: gc/tests must be found ({tests_root:?})"
        );
        for path in test_files {
            let src = std::fs::read_to_string(&path).expect("read a gc/tests file");
            let lines = collapsed_literal_lines(&src);
            let rel = path
                .strip_prefix(&tests_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let allowed = TESTS_ALLOWED
                .iter()
                .find(|(f, _)| *f == rel)
                .map_or(0, |(_, n)| *n);
            if lines.len() > allowed {
                bad.push(format!("tests/{rel}: lines {lines:?} (allowed {allowed})"));
            }
            let typed = typed_newline_continuation_lines(&src);
            if !typed.is_empty() {
                bad.push(format!(
                    "tests/{rel}: `\\n`-typed continuation on lines {typed:?}"
                ));
            }
        }
        assert!(
            bad.is_empty(),
            "string literal(s) with a mid-line run of 12+ spaces -- a `\\` line \
             continuation lost its backslash (end the line with `\\` or use one \
             space): {bad:#?}"
        );
    }

    #[test]
    fn collapsed_literal_scanner_sees_only_literals() {
        let q = '"';
        let gap = " ".repeat(14);
        let lost = format!("let a = {q}first{gap}second{q};\n");
        let comment = format!("// first{gap}second\n");
        let escaped = format!("let b = {q}one\\n{gap}two{q};\n");
        let raw = format!("let c = r#{q}first{gap}second{q}#;\n");
        let src = format!("{comment}{escaped}{raw}{lost}");
        assert_eq!(collapsed_literal_lines(&src), vec![4]);
        // The other scanner sees exactly the `\n`-escaped run, and neither a
        // short indentation after `\n` nor a run after `\t`.
        assert_eq!(typed_newline_continuation_lines(&src), vec![2]);
        let short = format!("let d = {q}one\\n    two{q};\nlet e = {q}one\\t{gap}two{q};\n");
        assert!(typed_newline_continuation_lines(&short).is_empty());
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

    /// Every ZGC collection through `VmHeap` records a collector decision, and
    /// the decision agrees with what the cycle actually did.
    ///
    /// Before 2026-09-23 ZGC recorded nothing, so the default backend's
    /// `collector_decision_report()` was empty however much it collected.
    /// Asserted as AGREEMENT with the slide counter rather than as a fixed
    /// verdict: whether a given cycle compacts depends on the cost gate and the
    /// relocation defaults, and a test pinned to one of those would be testing
    /// the gate, not the record.
    #[cfg(feature = "zgc")]
    #[test]
    fn every_zgc_collection_records_a_decision_that_matches_the_slide() {
        use crate::gc_metrics::{decision_reason, last_collector_decision};
        let heap = VmHeap::new(GcBackend::Zgc, 8 * 1024 * 1024);
        let VmHeap::Zgc(z) = &heap else {
            unreachable!("constructed as Zgc")
        };
        let keep = heap.alloc_object(ClassId::new(1), 1);
        for _ in 0..256 {
            let _ = heap.alloc_object(ClassId::new(1), 4);
        }
        let seq_before = last_collector_decision().map_or(0, |d| d.sequence);
        let compactions_before = z.feature_engagement().1;
        // SAFETY: this test is the only mutator.
        let stw = unsafe { crate::collector::StopTheWorldToken::new() };
        let mut roots = [keep];
        let _ = heap.collect_garbage(&stw, &mut roots, &R6NoMonitors);

        let d = last_collector_decision().expect("a ZGC collection must record a decision");
        assert_eq!(
            d.sequence,
            seq_before + 1,
            "exactly one record per collection"
        );
        assert_eq!(d.backend, "zgc");
        let compacted = z.feature_engagement().1 > compactions_before;
        assert_eq!(
            d.young_moving,
            compacted,
            "the record must say MOVING iff the slide ran (reason={})",
            decision_reason::label(d.reason),
        );
        if compacted {
            assert_eq!(d.reason, decision_reason::MOVING_BACKEND_COMPACTED);
        }

        // ...and the finalizer-aware door records too: production reaches the
        // collector through it, not through `collect_garbage`.
        let _ = heap.collect_garbage_with_finalizers(&stw, &mut roots, &[], &R6NoMonitors);
        let d2 = last_collector_decision().expect("recorded");
        assert_eq!(d2.sequence, seq_before + 2);
        assert_eq!(d2.backend, "zgc");
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

    /// gci i11/gm finding 13 — an explicit `-XX:G1HeapRegionSize` follows
    /// HotSpot 25 (measured on 25.0.3 with `-Xmx2g`): below 1 MiB rounds up to
    /// 1 MiB, a non-power of two rounds up, 64 MiB stays 64 MiB (the ergonomic
    /// 32 MiB cap does not apply), 512 MiB is the ceiling.
    #[test]
    fn gci_i11gm_explicit_region_size_follows_hotspot_bounds() {
        const MIB: usize = 1024 * 1024;
        let heap = 2048 * MIB;
        assert_eq!(explicit_region_size(512 * 1024, heap), MIB);
        assert_eq!(explicit_region_size(1, heap), MIB);
        assert_eq!(explicit_region_size(MIB, heap), MIB);
        assert_eq!(explicit_region_size(3 * MIB, heap), 4 * MIB);
        assert_eq!(explicit_region_size(64 * MIB, heap), 64 * MIB);
        assert_eq!(explicit_region_size(512 * MIB, heap), 512 * MIB);
        assert_eq!(explicit_region_size(300 * MIB, heap), 512 * MIB);
        // Above the ceiling (the launcher refuses first; the clamp is a backstop).
        assert_eq!(explicit_region_size(1024 * MIB, heap), 512 * MIB);
        // Never larger than the heap's largest power of two (one region at least).
        assert_eq!(explicit_region_size(64 * MIB, 48 * MIB), 32 * MIB);
        assert_eq!(explicit_region_size(4 * MIB, 8 * MIB), 4 * MIB);
        // The ergonomic arm keeps HotSpot's 32 MiB ergonomic ceiling.
        assert_eq!(g1_ergonomic_region_size(1usize << 40), G1_MAX_REGION_SIZE);
    }

    /// gci i11/gm finding 13 — end to end through the constructor: `512k` gives
    /// 1 MiB regions, as on HotSpot.
    #[test]
    fn gci_i11gm_a_sub_mib_explicit_region_size_builds_one_mib_regions() {
        let ov = G1ConfigOverrides {
            region_size: Some(512 * 1024),
            ..Default::default()
        };
        let h = VmHeap::new_with_overrides(GcBackend::G1, 8 * 1024 * 1024, ov);
        assert_eq!(g1_state(&h).unwrap().num_regions(), 8);
    }

    /// `-Xms` reaches every backend arm of the constructor, each arm says what
    /// it did with it, and as of 2026-09-21 every arm COMMITS.
    ///
    /// Two `Dropped` answers were the finding this test was written to state
    /// (`docs/feature-designs/generative-gc-architecture.md`);
    /// both are gone. The shape is what made that edit a one-line change — the
    /// previous spelling, a field on `G1ConfigOverrides` that two arms could
    /// not read, had nothing to edit — so the assertions stay here rather than
    /// being retired with the page.
    #[test]
    fn every_backend_reports_what_it_did_with_xms() {
        let xmx = 8 * 1024 * 1024;
        let xms = 4 * 1024 * 1024;
        let ov = G1ConfigOverrides::default();

        for backend in backends_under_test() {
            // Absent and zero are both "nothing was asked for", never a commit
            // of nothing.
            for none_ish in [None, Some(0usize)] {
                let (_, d) = VmHeap::new_with_heap_sizing(backend, xmx, none_ish, ov);
                assert_eq!(
                    d,
                    XmsDisposition::NotSupplied,
                    "{backend:?}: an absent -Xms must not read as a commit",
                );
            }

            let (_, d) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xms), ov);
            assert_eq!(
                d,
                XmsDisposition::Committed(xms),
                "{backend:?}: -Xms must be honoured, not dropped",
            );

            // `-Xms` above `-Xmx` is clamped, not refused.
            let (_, d) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xmx * 4), ov);
            assert_eq!(
                d,
                XmsDisposition::Committed(xmx),
                "{backend:?}: an oversized -Xms is clamped to -Xmx",
            );
        }
    }

    /// The backends this build has. ZGC is feature-gated, and a loop over a
    /// hardcoded pair would silently stop covering it.
    fn backends_under_test() -> Vec<GcBackend> {
        let mut v = vec![GcBackend::Generational, GcBackend::G1];
        #[cfg(feature = "zgc")]
        v.push(GcBackend::Zgc);
        v
    }

    /// `-Xms` moves COMMITTED BYTES, on every backend, and does not move the
    /// heap's capacity.
    ///
    /// The disposition test above asserts what each arm *says*. This one
    /// asserts that the saying is backed: a `Committed(n)` that committed
    /// nothing would pass that test and would be exactly the silent drop the
    /// known-issues page was opened about.
    ///
    /// # Why the assertion is a comparison and not a threshold
    ///
    /// How many bytes a fresh heap has committed depends on the store
    /// (`CRATONVM_GC_RESERVE=0` gives a wholly-committed one, where every arena
    /// starts at its capacity and nothing can rise), on the granule size, and
    /// on what the constructor itself touches. So the test builds two heaps of
    /// the SAME `-Xmx` and requires the `-Xms` one to be committed at least as
    /// much — never less — and requires `-Xms` to leave `heap_capacity`
    /// untouched, which is the half that would break if a backend "honoured"
    /// the flag by sizing its reservation from it.
    #[test]
    fn xms_commits_bytes_without_resizing_the_heap() {
        let xmx = 64 * 1024 * 1024;
        let xms = 32 * 1024 * 1024;
        let ov = G1ConfigOverrides::default();

        for backend in backends_under_test() {
            let (cold, _) = VmHeap::new_with_heap_sizing(backend, xmx, None, ov);
            let (warm, d) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xms), ov);
            assert_eq!(d, XmsDisposition::Committed(xms));

            // g1sz: G1's `heap_capacity` is the sizing policy's soft capacity
            // (default ON since 2026-10-26), which starts at `-Xms` exactly as
            // HotSpot's committed heap does; the heap -Xms must not size is
            // its `-Xmx` grid.
            let max_capacity = |h: &VmHeap| match h {
                VmHeap::G1(g) => g.collector.heap_target_bytes_within(false),
                _ => h.heap_capacity(),
            };
            assert_eq!(
                max_capacity(&warm),
                max_capacity(&cold),
                "{backend:?}: -Xms must size the startup COMMIT, never the heap. A capacity that moved with -Xms is the \"alias for -Xmx\" mistake: -Xms512m -Xmx8g would allocate 512m and then fail.",
            );
            assert!(
                warm.os_committed_bytes() >= cold.os_committed_bytes(),
                "{backend:?}: -Xms committed {} bytes, no more than the {} a heap with no -Xms at all starts with — the flag is being accepted and dropped",
                warm.os_committed_bytes(),
                cold.os_committed_bytes(),
            );

            // On a RESERVING store the `>=` above is not enough: a backend that
            // dropped the flag would satisfy it with equality, which is the
            // defect. The precondition is the store's own: a heap that already
            // has its whole capacity committed cannot commit more, and the
            // wholly-committed fallback (`CRATONVM_GC_RESERVE=0`, and any
            // platform without a reservation) is exactly that heap.
            if cold.os_committed_bytes() < cold.heap_capacity() {
                assert!(
                    warm.os_committed_bytes() > cold.os_committed_bytes(),
                    "{backend:?}: the store reserves ({} of {} committed with no -Xms), so an -Xms of {xms} must raise the committed bytes; it left them at {}",
                    cold.os_committed_bytes(),
                    cold.heap_capacity(),
                    warm.os_committed_bytes(),
                );
            }
        }
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

    /// gci i15/ca: the full collection's abort joins the marker, leaves the
    /// marking phase, closes the SATB queue and strands no worker; a second
    /// abort (and one with no cycle) does nothing; a fresh cycle then starts
    /// and completes normally. Filter: `-p cratonvm-gc gci_i15ca`.
    #[test]
    fn gci_i15ca_abort_joins_the_marker_and_a_fresh_cycle_follows() {
        let heap = make_g1_heap();
        assert!(!heap.g1_abort_concurrent_mark(&stw()), "no cycle: nothing to abort");
        let arc_before = Arc::strong_count(&g1_state(&heap).unwrap().collector);

        heap.g1_start_concurrent_mark(&stw());
        assert!(heap.g1_is_marking_active());
        assert!(g1_state(&heap).unwrap().has_active_controller());

        assert!(heap.g1_abort_concurrent_mark(&stw()), "the open cycle is aborted");
        assert!(!heap.g1_is_marking_active(), "abort leaves the marking phase");
        assert!(
            !g1_state(&heap).unwrap().has_active_controller(),
            "abort joins and drops the marker"
        );
        assert!(
            !g1_state(&heap).unwrap().collector.satb_queue().is_active(),
            "abort closes the SATB queue"
        );
        assert_eq!(
            Arc::strong_count(&g1_state(&heap).unwrap().collector),
            arc_before,
            "the joined worker holds no collector clone"
        );
        assert!(!heap.g1_abort_concurrent_mark(&stw()), "a second abort does nothing");

        // The full collection's own cycle: starts, marks, completes.
        heap.g1_start_concurrent_mark(&stw());
        assert!(heap.g1_is_marking_active());
        assert!(g1_state(&heap).unwrap().collector.satb_queue().is_active());
        heap.g1_signal_marking_complete(&stw());
        assert!(!heap.g1_is_marking_active());
        assert!(!g1_state(&heap).unwrap().has_active_controller());
    }

    /// gci i15/ca: nothing the abort leaves behind gives a verdict from the
    /// half-built cycle. A logged SATB entry is discarded (the queue is empty
    /// and closed), the referent-slot skip set is dropped, and
    /// `is_live_after_mark` answers LIVE for an object the aborted cycle never
    /// marked (the mark-start snapshot is gone, so no bitmap bit is read).
    #[test]
    fn gci_i15ca_abort_leaves_no_half_built_mark_state() {
        let heap = make_g1_heap();
        let holder = heap.alloc_object(cratonvm_types::ClassId::new(1), 0);
        let unmarked = heap.alloc_object(cratonvm_types::ClassId::new(2), 0);
        let holder_addr = holder.as_ptr() as usize;
        let unmarked_addr = unmarked.as_ptr() as usize;
        heap.g1_start_concurrent_mark_with_reference_skip(&stw(), &[holder_addr]);
        let state = g1_state(&heap).unwrap();
        assert_eq!(state.collector.dbg_reference_skip_len(), 1);
        heap.satb_barrier(Value::Object(Some(unmarked)));
        heap.flush_thread_satb();

        assert!(heap.g1_abort_concurrent_mark(&stw()));
        let queue = state.collector.satb_queue();
        assert!(!queue.is_active());
        assert_eq!(queue.len(), 0, "the aborted cycle's SATB entries are discarded");
        assert_eq!(state.collector.dbg_reference_skip_len(), 0, "the skip set is per cycle");
        assert!(
            state.collector.is_live_after_mark(unmarked_addr),
            "no dead verdict from an aborted cycle's bitmap"
        );
        assert!(state.collector.is_live_after_mark(holder_addr));
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

    /// G1's soft-ref headroom is the whole heap's, not the unused tails of the
    /// regions that currently happen to be Eden or Old.
    ///
    /// The pre-2026-09-23 arm fed G1's `eden_stats` / `old_gen_stats` into the
    /// generational formula. Those capacities count only regions OF THAT TYPE,
    /// so Free regions — G1's actual headroom — were counted nowhere, and on a
    /// nearly empty heap the answer was `0` MB: maximum soft-reference
    /// pressure, every `SoftReference` cleared at every pause.
    #[test]
    fn soft_ref_policy_free_mb_on_g1_counts_free_regions() {
        const MB: usize = 1024 * 1024;
        let heap = VmHeap::new(GcBackend::G1, 64 * MB);
        // A few objects, so at least one region is claimed and the old formula
        // had something (a part-full region) to report the tail of.
        for _ in 0..64 {
            let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
        }
        let reported = heap.soft_ref_policy_free_mb();
        // g1sz: against the `-Xmx` grid (HotSpot's `MaxHeapSize`), not the
        // sizing policy's soft capacity, which starts at `-Xms`.
        let max = match &heap {
            VmHeap::G1(h) => h.collector.heap_target_bytes_within(false),
            _ => unreachable!("built as G1"),
        };
        let whole_heap_free_mb = max.saturating_sub(heap.allocated_bytes()) / MB;
        assert_eq!(
            reported, whole_heap_free_mb,
            "G1 has one allocation pool: its soft-ref headroom is max capacity - allocated"
        );
        assert!(
            reported >= 32,
            "a 64 MiB G1 heap holding a few KiB must not report maximum soft-ref \
             pressure (got {reported} MB) — that clears every SoftReference at every pause"
        );
    }

    /// ZGC's figure did not change with the G1 repair: it was already the
    /// whole-heap headroom, reached through `young_gen_stats() == (0, 0)`.
    #[cfg(feature = "zgc")]
    #[test]
    fn soft_ref_policy_free_mb_on_zgc_is_whole_heap_headroom() {
        const MB: usize = 1024 * 1024;
        let heap = VmHeap::new(GcBackend::Zgc, 64 * MB);
        let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
        assert_eq!(
            heap.soft_ref_policy_free_mb(),
            heap.heap_capacity().saturating_sub(heap.allocated_bytes()) / MB,
        );
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

    /// `committed_bytes` is a count of committed granules/regions, not an
    /// occupancy, and `Runtime.totalMemory()` rests on that: allocating inside
    /// the committed prefix must move `allocated_bytes` and leave
    /// `committed_bytes` alone. A version that tracked occupancy would report a
    /// `totalMemory()` that changes on every collection, which callers written
    /// as "gc until totalMemory settles" read as "the heap is still resizing".
    ///
    /// All three backends since 2026-09-23 (the arms were Generational and G1
    /// only while ZGC's answer was a constant envelope). `-Xms` 16 MiB so the
    /// few hundred KiB the fixture allocates land inside what is already
    /// committed on every backend.
    #[test]
    fn committed_bytes_does_not_track_occupancy() {
        const MB: usize = 1024 * 1024;
        let ov = G1ConfigOverrides::default();
        for backend in backends_under_test() {
            let (heap, _) = VmHeap::new_with_heap_sizing(backend, 64 * MB, Some(16 * MB), ov);
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

    /// `Runtime.totalMemory()` is the COMMITTED heap, not the reservation
    /// (`XmsProbe -Xms64m -Xmx512m`: HotSpot `total=64m`; CratonVM printed
    /// `total=512m` on every backend until 2026-09-23). A fresh heap with a
    /// small `-Xms` under a large `-Xmx` must report roughly the `-Xms`, on
    /// every backend.
    ///
    /// Precondition, as in `xms_commits_bytes_without_resizing_the_heap`: the
    /// store must be a RESERVING one. The wholly-committed fallback
    /// (`CRATONVM_GC_RESERVE=0`, or a platform that refused the reservation)
    /// commits every arena at its capacity, and there the reservation IS the
    /// committed heap.
    #[test]
    fn committed_bytes_is_the_committed_heap_not_the_reservation() {
        const MB: usize = 1024 * 1024;
        let xmx = 256 * MB;
        let xms = 16 * MB;
        let ov = G1ConfigOverrides::default();
        for backend in backends_under_test() {
            let (heap, _) = VmHeap::new_with_heap_sizing(backend, xmx, Some(xms), ov);
            if heap.os_committed_bytes() >= heap.heap_capacity() {
                continue; // wholly-committed store: nothing to distinguish
            }
            let committed = heap.committed_bytes();
            // gen r5w2/obs6: Generational's committed heap excludes the copy
            // reserve (HotSpot Serial's `capacity()` leaves its to-space out
            // too). Under the even split (`CRATONVM_GEN_XMS_USABLE_FIRST=0`)
            // `commit_initial_heap` puts half of a 16 MiB `-Xms` (all of it
            // young here) in the to-space: committed, not usable — hence the
            // `xms / 2` floor. The default since gen r5w4/defaults8 is the
            // usable-first split (one survivor in the to-space, ~14 MiB usable
            // here), which clears the same floor with room.
            let floor = if backend == GcBackend::Generational {
                xms / 2
            } else {
                xms
            };
            assert!(
                committed >= floor,
                "{backend:?}: -Xms {xms} was committed at startup, but \
                 committed_bytes reports only {committed} (floor {floor})"
            );
            assert!(
                committed < xmx / 2,
                "{backend:?}: committed_bytes reports {committed} of a {xmx} \
                 reservation with a {xms} -Xms — that is the reservation, \
                 which is what Runtime.totalMemory() must not report"
            );
            assert!(
                committed <= heap.os_committed_bytes(),
                "{backend:?}: committed_bytes ({committed}) above what the \
                 stores have asked the OS to back ({})",
                heap.os_committed_bytes()
            );
        }
    }

    /// `docs/feature-designs/concurrent-gc-maturation.md`:
    /// a humongous object must be counted in the G1 old-generation figure, and
    /// the young + old capacities must cover the whole heap (Free regions
    /// included) rather than only the regions currently typed Eden or Old.
    #[test]
    fn g1_old_gen_stats_count_humongous_and_free_regions() {
        const MB: usize = 1024 * 1024;
        let heap = VmHeap::new(GcBackend::G1, 64 * MB);
        // 4 MiB of int — larger than half of any region size the 64 MiB
        // ergonomic picks, so it is a humongous span.
        let len = MB;
        let arr = heap.try_alloc_array(cratonvm_types::ClassId::new(0), ArrayElementType::Int, len);
        assert!(
            arr.is_some(),
            "fixture: the humongous allocation must succeed"
        );
        let (young_used, young_cap) = heap.young_gen_stats();
        let (old_used, old_cap) = heap.old_gen_stats();
        assert!(
            old_used >= 4 * len,
            "G1 old_gen_stats reports {old_used} bytes used with a {}-byte \
             humongous array live — humongous regions are not being counted",
            4 * len
        );
        assert!(
            young_cap + old_cap >= heap.heap_capacity(),
            "G1 young+old capacity ({young_cap} + {old_cap}) does not cover the \
             heap ({}) — Free regions are not being counted",
            heap.heap_capacity()
        );
        assert!(
            young_used + old_used >= heap.allocated_bytes(),
            "G1 young+old used ({young_used} + {old_used}) below the heap's \
             allocated bytes ({})",
            heap.allocated_bytes()
        );
    }

    /// gc-common w4-f (`docs/feature-designs/concurrent-gc-maturation.md`): Survivor
    /// regions are HotSpot's "G1 Survivor Space", a YOUNG pool — they count in
    /// `young_gen_stats`, not `old_gen_stats` (where the wave-2 complement of
    /// Eden had put them). And `region_census` partitions the region table.
    #[test]
    fn g1_survivor_regions_are_on_the_young_side() {
        use crate::region::RegionType;
        const MB: usize = 1024 * 1024;
        const SURVIVOR_BYTES: usize = 4096;
        let heap = VmHeap::new(GcBackend::G1, 64 * MB);
        let VmHeap::G1(h) = &heap else {
            unreachable!("constructed as G1")
        };
        // A few ordinary objects (Eden) ...
        for _ in 0..64 {
            let _ = heap.try_alloc_object(cratonvm_types::ClassId::new(0), 8);
        }
        let (young_before, _) = heap.young_gen_stats();
        let (old_before, _) = heap.old_gen_stats();
        // ... and one Free region staged as a part-full Survivor, the state a
        // young pause with survivors leaves. Only the cursor is read by the
        // census, so no byte of the region is touched.
        let mut staged = false;
        h.with_regions_mut(|regions| {
            if let Some(r) = regions
                .iter_mut()
                .find(|r| r.region_type() == RegionType::Free)
            {
                r.set_age(1);
                r.set_region_type(RegionType::Survivor);
                r.set_cursor(SURVIVOR_BYTES);
                staged = true;
            }
        });
        assert!(
            staged,
            "fixture: a 64 MiB G1 heap has a Free region to stage"
        );

        let c = h.region_census();
        assert_eq!(c.survivor, (SURVIVOR_BYTES, 1));
        assert_eq!(
            c.eden.1 + c.survivor.1 + c.old.1 + c.humongous.1 + c.free,
            h.num_regions(),
            "the census must partition the region table"
        );
        let (young_used, young_cap) = heap.young_gen_stats();
        let (old_used, old_cap) = heap.old_gen_stats();
        assert_eq!(
            young_used,
            young_before + SURVIVOR_BYTES,
            "Survivor bytes must be on the young side"
        );
        assert_eq!(old_used, old_before, "... and not on the old side");
        assert!(
            young_cap >= (c.eden.1 + 1) * c.region_size,
            "young capacity must include the Survivor region"
        );
        assert!(
            young_cap + old_cap >= heap.heap_capacity(),
            "G1 young+old capacity ({young_cap} + {old_cap}) must still cover \
             the heap ({})",
            heap.heap_capacity()
        );
        assert_eq!(
            young_used + old_used,
            heap.allocated_bytes(),
            "young + old used must be every region's cursor, exactly once"
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
            // The target lives in the second word; the mark word says FORWARDED.
            std::ptr::write(
                obj.as_ptr().add(cratonvm_types::FORWARDING_TARGET_OFFSET) as *mut u64,
                0xFFFF_FFFF_FFFF_FFF8u64,
            );
            header.mark_word.store(
                cratonvm_types::MARK_FORWARDED,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        assert_eq!(
            heap.load_and_forward(obj).as_ptr(),
            obj.as_ptr(),
            "an implausible forwarding target must fall back to the original pointer"
        );
    }

    /// gci i1/wr (`r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`, step
    /// 2): through `VmHeap`, a G1 `cleanup` (which can free whole Old regions
    /// in place) moves `address_epoch()` while `collection_count()` stays, so
    /// the native address memos that key on it miss after a cleanup.
    #[test]
    fn gci_wr_g1_cleanup_moves_the_vm_heap_address_epoch_not_the_count() {
        let heap = make_g1_heap();
        let collections = heap.collection_count();
        let epoch = heap.address_epoch();
        let g1 = g1_state(&heap).expect("a G1 heap");
        g1.start_concurrent_mark(&stw());
        g1.remark(&stw(), &[]);
        while !g1.concurrent_mark_step(usize::MAX) {}
        g1.cleanup(&stw());
        assert_eq!(heap.collection_count(), collections, "cleanup books no collection");
        assert_ne!(heap.address_epoch(), epoch, "cleanup must move the address epoch");
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
    /// `docs/feature-designs/jit-ir-optimizations.md`
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

/// JIT round 23 wave 1, lane L4 (w17-L2 item 2): `load_ref_field_compact`
/// answers what `get_field` answers for a compact reference field, and
/// declines every other shape.
#[cfg(test)]
mod r23_w1_l4_load_ref_field_compact_tests {
    use super::*;
    use cratonvm_types::{CompactLayout, FieldStorageKind};

    fn exercise(backend: GcBackend, cid: u32) {
        let heap = VmHeap::new(backend, 64 * 1024 * 1024);
        let domain = match &heap {
            VmHeap::Generational(h) => h.layout_domain(),
            VmHeap::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.layout_domain(),
        };
        // Wide references only: the narrow width is a process-wide switch
        // tests must not flip (`narrow_oop::disable_for_test`).
        assert!(!cratonvm_types::narrow_oop::narrow_oops_enabled());
        cratonvm_types::register_class_layout(
            domain,
            cid,
            Arc::new(CompactLayout {
                field_disps: vec![8, 16],
                is_ref: vec![false, true],
                field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
                ref_disps: vec![16],
                total_size: 24,
            }),
        );
        let obj = heap.alloc_object(ClassId::new(cid), 2);
        assert!(cratonvm_types::is_compact_object(heap.get_header(obj)));
        let child = heap.alloc_object(ClassId::new(cid + 1), 0);
        // SAFETY (every call below): live objects of this heap, no collection.
        unsafe {
            assert_eq!(heap.load_ref_field_compact(obj, 1), Some(0), "null");
            heap.set_field(obj, 1, Value::Object(Some(child)));
            assert_eq!(
                heap.load_ref_field_compact(obj, 1),
                Some(child.as_ptr() as u64)
            );
            assert_eq!(heap.load_ref_field_compact(obj, 0), None, "an int row");
            assert_eq!(heap.load_ref_field_compact(obj, 2), None, "no such slot");
            let legacy = heap.alloc_object(ClassId::new(cid + 2), 2);
            assert_eq!(heap.load_ref_field_compact(legacy, 1), None, "legacy body");
            let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
            assert_eq!(heap.load_ref_field_compact(arr, 1), None, "array");
        }
        assert_eq!(heap.get_field(obj, 1), Value::Object(Some(child)));
    }

    #[test]
    fn r23_w1_l4_load_ref_field_compact_matches_get_field() {
        exercise(GcBackend::Generational, 0x5_2301);
        exercise(GcBackend::G1, 0x5_2311);
        #[cfg(feature = "zgc")]
        exercise(GcBackend::Zgc, 0x5_2321);
    }
}

/// [`VmHeap::copy_compact_object_fields`]: the copy is the slot-by-slot copy,
/// on every backend, and a refused copy writes nothing.
/// Filter: `cargo test -j 5 -p cratonvm-gc --lib compact_clone_copy_tests`.
#[cfg(test)]
mod compact_clone_copy_tests {
    use super::*;
    use cratonvm_types::{CompactLayout, FieldStorageKind};

    fn domain_of(heap: &VmHeap) -> u32 {
        match heap {
            VmHeap::Generational(h) => h.layout_domain(),
            VmHeap::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.layout_domain(),
        }
    }

    fn exercise(backend: GcBackend, cid: u32) {
        let heap = VmHeap::new(backend, 64 * 1024 * 1024);
        let domain = domain_of(&heap);
        // int, boolean, Object, long, double, Object, byte, Object.
        cratonvm_types::register_class_layout(
            domain,
            cid,
            Arc::new(CompactLayout {
                field_disps: vec![8, 12, 16, 24, 32, 40, 48, 56],
                is_ref: vec![false, false, true, false, false, true, false, true],
                field_kinds: vec![
                    FieldStorageKind::Int,
                    FieldStorageKind::Boolean,
                    FieldStorageKind::Reference,
                    FieldStorageKind::Long,
                    FieldStorageKind::Double,
                    FieldStorageKind::Reference,
                    FieldStorageKind::Byte,
                    FieldStorageKind::Reference,
                ],
                ref_disps: vec![16, 40, 56],
                total_size: 64,
            }),
        );
        let src = heap.alloc_object(ClassId::new(cid), 8);
        assert!(cratonvm_types::is_compact_object(heap.get_header(src)));
        let child = heap.alloc_object(ClassId::new(cid + 1), 0);
        for (slot, value, desc) in [
            (0, Value::Int(-41), b'I'),
            (1, Value::Int(1), b'Z'),
            (2, Value::Object(Some(child)), b'L'),
            (3, Value::Long(i64::MIN + 3), b'J'),
            (4, Value::Double(-2.5), b'D'),
            (6, Value::Int(-7), b'B'),
        ] {
            heap.set_field_as(src, slot, value, desc);
        }
        // Slot 5 stays null; slot 7 holds a primitive boxed into a reference
        // slot, which the verbatim copy must carry as the slot-by-slot one does.
        heap.set_field(src, 7, Value::Int(123));

        let copied = heap.alloc_object(ClassId::new(cid), 8);
        let by_slot = heap.alloc_object(ClassId::new(cid), 8);
        assert_eq!(heap.copy_compact_object_fields(src, copied), Some(()));
        for slot in 0..8 {
            heap.set_field(by_slot, slot, heap.get_field(src, slot));
        }
        for slot in 0..8 {
            assert_eq!(
                heap.get_field(copied, slot),
                heap.get_field(by_slot, slot),
                "slot {slot} on {backend:?}"
            );
            assert_eq!(heap.get_field(copied, slot), heap.get_field(src, slot));
        }
        assert_eq!(heap.get_field(copied, 2), Value::Object(Some(child)));
        assert_eq!(heap.get_field(copied, 5), Value::Object(None));

        // The oop-map reader answers what `get_field` answers as references:
        // slot 2's child; slot 5 is null and slot 7's word unboxes to an int.
        let by_get_field = |from: usize| -> Vec<usize> {
            (from..8)
                .filter_map(|slot| match heap.get_field(src, slot) {
                    Value::Object(Some(r)) => Some(r.as_ptr() as usize),
                    _ => None,
                })
                .collect()
        };
        for from in [0, 3] {
            let mut words = vec![0xdead];
            // SAFETY: `src` is live and nothing collects in this test.
            assert!(unsafe { heap.push_compact_reference_words(src, from, &mut words) });
            assert_eq!(words[0], 0xdead, "what was on the stack stays");
            assert_eq!(
                words[1..],
                by_get_field(from)[..],
                "from slot {from} on {backend:?}"
            );
        }
        assert_eq!(by_get_field(0), vec![child.as_ptr() as usize]);
        let real = heap.alloc_object(ClassId::new(cid + 1), 0);
        heap.set_field(src, 7, Value::Object(Some(real)));
        let mut words = Vec::new();
        // SAFETY: as above.
        assert!(unsafe { heap.push_compact_reference_words(src, 0, &mut words) });
        assert_eq!(
            words,
            by_get_field(0),
            "a real reference in the screened slot is pushed"
        );

        // Refused, and nothing written: another class, a legacy body, an array.
        cratonvm_types::register_class_layout(
            domain,
            cid + 2,
            Arc::new(CompactLayout {
                field_disps: vec![8, 12, 16, 24, 32, 40, 48, 56],
                is_ref: vec![false; 8],
                field_kinds: vec![FieldStorageKind::Int; 8],
                ref_disps: vec![],
                total_size: 64,
            }),
        );
        let other = heap.alloc_object(ClassId::new(cid + 2), 8);
        assert_eq!(heap.copy_compact_object_fields(src, other), None);
        assert_eq!(heap.get_field_as(other, 0, b'I'), Value::Int(0));
        let legacy = heap.alloc_object(ClassId::new(cid + 3), 8);
        assert!(!cratonvm_types::is_compact_object(heap.get_header(legacy)));
        assert_eq!(heap.copy_compact_object_fields(legacy, legacy), None);
        let mut words = vec![1];
        // SAFETY: as above.
        assert!(!unsafe { heap.push_compact_reference_words(legacy, 0, &mut words) });
        assert_eq!(words, vec![1], "a refusal pushes nothing");
        let array = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 8);
        assert_eq!(heap.copy_compact_object_fields(array, array), None);

        // A large layout keeps the slot-by-slot copy (G1's humongous backing).
        cratonvm_types::register_class_layout(
            domain,
            cid + 4,
            Arc::new(CompactLayout {
                field_disps: vec![8, 1024],
                is_ref: vec![false, false],
                field_kinds: vec![FieldStorageKind::Int; 2],
                ref_disps: vec![],
                total_size: 1032,
            }),
        );
        let big = heap.alloc_object(ClassId::new(cid + 4), 2);
        let big_copy = heap.alloc_object(ClassId::new(cid + 4), 2);
        heap.set_field_as(big, 1, Value::Int(5), b'I');
        assert_eq!(heap.copy_compact_object_fields(big, big_copy), None);
        assert_eq!(heap.get_field_as(big_copy, 1, b'I'), Value::Int(0));

        // j27 wave 4 (lane CLONE2): the class-level answer, asked before any
        // copy exists, agrees with the copy: the copied classes are admitted,
        // the large layout and the class with no layout are not.
        assert!(heap.compact_object_copy_ready(), "{backend:?}");
        assert!(heap.compact_object_copy_admits_class(ClassId::new(cid)), "{backend:?}");
        assert!(heap.compact_object_copy_admits_class(ClassId::new(cid + 2)));
        assert!(!heap.compact_object_copy_admits_class(ClassId::new(cid + 3)));
        assert!(!heap.compact_object_copy_admits_class(ClassId::new(cid + 4)));
    }

    // `exercise` registers layouts for `cid ..= cid + 4` in the process-wide
    // registry, and the three tests run in parallel: their ranges must not
    // overlap.
    #[test]
    fn generational_clone_copy_is_the_slot_by_slot_copy() {
        exercise(GcBackend::Generational, 61_530);
    }
    #[test]
    fn g1_clone_copy_is_the_slot_by_slot_copy() {
        exercise(GcBackend::G1, 61_540);
    }
    #[cfg(feature = "zgc")]
    #[test]
    fn zgc_clone_copy_is_the_slot_by_slot_copy() {
        exercise(GcBackend::Zgc, 61_550);
    }
}

/// The stack-walker residual-costs page, item 2: a batch holding references
/// (`VmHeap::try_set_compact_fields`) leaves each slot exactly as the scalar
/// `set_field_as` leaves it, on every collector, and declines (writing
/// nothing) where the scalar accessor would box or the plan does not hold.
#[cfg(test)]
mod compact_mixed_batch_tests {
    use super::*;
    use cratonvm_types::{CompactLayout, FieldStorageKind};

    fn exercise(backend: GcBackend, cid: u32) {
        let heap = VmHeap::new(backend, 64 * 1024 * 1024);
        let domain = match &heap {
            VmHeap::Generational(h) => h.layout_domain(),
            VmHeap::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.layout_domain(),
        };
        // StackFrameInfo's shape: references and ints interleaved.
        cratonvm_types::register_class_layout(
            domain,
            cid,
            Arc::new(CompactLayout {
                field_disps: vec![8, 16, 24, 32, 40, 48],
                is_ref: vec![true, false, true, false, true, true],
                field_kinds: vec![
                    FieldStorageKind::Reference,
                    FieldStorageKind::Int,
                    FieldStorageKind::Reference,
                    FieldStorageKind::Int,
                    FieldStorageKind::Reference,
                    FieldStorageKind::Reference,
                ],
                ref_disps: vec![8, 24, 40, 48],
                total_size: 56,
            }),
        );
        let actual = heap.alloc_object(ClassId::new(cid), 6);
        let expected = heap.alloc_object(ClassId::new(cid), 6);
        assert!(cratonvm_types::is_compact_object(heap.get_header(actual)));
        let a = heap.alloc_object(ClassId::new(cid + 10), 0);
        let b = heap.alloc_object(ClassId::new(cid + 11), 0);
        let slots = [(0, b'L'), (1, b'I'), (2, b'L'), (3, b'I'), (4, b'L'), (5, b'L')];
        let values = [
            Value::Object(Some(a)),
            Value::Int(0x40),
            Value::Object(Some(b)),
            Value::Int(-1),
            Value::Object(None),
            Value::Object(Some(a)),
        ];
        for ((index, desc), value) in slots.iter().zip(&values) {
            heap.set_field_as(expected, *index, *value, *desc);
        }
        assert_eq!(heap.try_set_compact_fields(actual, &slots, &values), Some(actual));
        for (index, desc) in slots {
            assert_eq!(
                heap.get_field_as(actual, index, desc),
                heap.get_field_as(expected, index, desc),
                "slot {index}"
            );
        }
        // A primitive into a reference slot is boxed by the scalar accessor:
        // the batch declines and writes nothing, the int slot included.
        assert_eq!(
            heap.try_set_compact_fields(
                actual,
                &[(1, b'I'), (2, b'L')],
                &[Value::Int(7), Value::Int(5)]
            ),
            None
        );
        assert_eq!(heap.get_field_as(actual, 1, b'I'), Value::Int(0x40));
        // An object into a primitive slot declines.
        assert_eq!(
            heap.try_set_compact_fields(
                actual,
                &[(0, b'L'), (1, b'I')],
                &[Value::Object(Some(b)), Value::Object(Some(a))]
            ),
            None
        );
        assert_eq!(heap.get_field_as(actual, 0, b'L'), Value::Object(Some(a)));
        // A slot past the layout declines before the first store.
        assert_eq!(
            heap.try_set_compact_fields(
                actual,
                &[(0, b'L'), (6, b'L')],
                &[Value::Object(Some(b)), Value::Object(None)]
            ),
            None
        );
        assert_eq!(heap.get_field_as(actual, 0, b'L'), Value::Object(Some(a)));
        // A legacy receiver and an array keep the scalar path.
        let legacy = heap.alloc_object(ClassId::new(cid + 12), 6);
        assert_eq!(
            heap.try_set_compact_fields(
                legacy,
                &[(0, b'L'), (1, b'I')],
                &[Value::Object(None), Value::Int(1)]
            ),
            None
        );
        let array = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 6);
        assert_eq!(
            heap.try_set_compact_fields(array, &[(0, b'L'), (1, b'L')], &[Value::Object(None); 2]),
            None
        );
    }

    #[test]
    fn generational_compact_mixed_batch_preserves_scalar_semantics() {
        exercise(GcBackend::Generational, 62_741);
    }
    #[test]
    fn g1_compact_mixed_batch_preserves_scalar_semantics() {
        exercise(GcBackend::G1, 62_761);
    }
    #[cfg(feature = "zgc")]
    #[test]
    fn zgc_compact_mixed_batch_preserves_scalar_semantics() {
        exercise(GcBackend::Zgc, 62_781);
    }
}

#[cfg(test)]
mod compact_primitive_batch_tests {
    use super::*;
    use cratonvm_types::{CompactLayout, FieldStorageKind};

    fn exercise(backend: GcBackend, cid: u32) {
        let heap = VmHeap::new(backend, 64 * 1024 * 1024);
        let domain = match &heap {
            VmHeap::Generational(h) => h.layout_domain(),
            VmHeap::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.layout_domain(),
        };
        cratonvm_types::register_class_layout(
            domain,
            cid,
            Arc::new(CompactLayout {
                field_disps: vec![8, 12, 16, 24, 32, 40],
                is_ref: vec![false, false, false, false, false, true],
                field_kinds: vec![
                    FieldStorageKind::Int,
                    FieldStorageKind::Boolean,
                    FieldStorageKind::Long,
                    FieldStorageKind::Float,
                    FieldStorageKind::Double,
                    FieldStorageKind::Reference,
                ],
                ref_disps: vec![40],
                total_size: 48,
            }),
        );
        let actual = heap.alloc_object(ClassId::new(cid), 6);
        let expected = heap.alloc_object(ClassId::new(cid), 6);
        assert!(cratonvm_types::is_compact_object(heap.get_header(actual)));
        let slots = [(0, b'I'), (1, b'Z'), (2, b'J'), (3, b'F'), (4, b'D')];
        let values = [
            Value::Int(41),
            Value::Int(2),
            Value::Int(-17),
            Value::Float(1.25),
            Value::Double(-2.5),
        ];
        for ((index, desc), value) in slots.iter().zip(&values) {
            heap.set_field_as(expected, *index, *value, *desc);
        }
        assert_eq!(
            heap.try_set_compact_primitive_fields(actual, &slots, &values),
            Some(actual)
        );
        for (index, desc) in slots {
            assert_eq!(
                heap.get_field_as(actual, index, desc),
                heap.get_field_as(expected, index, desc)
            );
        }
        let read_slots = [
            (0, b'I'),
            (1, b'Z'),
            (2, b'J'),
            (3, b'F'),
            (4, b'D'),
            (5, b'L'),
        ];
        let mut batch = [Value::Uninitialized; 6];
        assert_eq!(
            heap.try_get_compact_fields(actual, &read_slots, &mut batch),
            Some(actual)
        );
        for (i, (slot, desc)) in read_slots.iter().enumerate() {
            assert_eq!(batch[i], heap.get_field_as(actual, *slot, *desc));
        }
        // An invalid later slot must not publish a partially-read output.
        let mut untouched = [Value::Int(77); 2];
        assert_eq!(
            heap.try_get_compact_fields(actual, &[(0, b'I'), (6, b'I')], &mut untouched),
            None
        );
        assert_eq!(untouched, [Value::Int(77); 2]);
        // The actual reference kind overrides a misleading primitive hint.
        let child = heap.alloc_object(ClassId::new(cid + 10), 0);
        heap.set_field_as(actual, 5, Value::Object(Some(child)), b'L');
        assert_eq!(
            heap.try_set_compact_primitive_fields(
                actual,
                &[(0, b'I'), (5, b'I')],
                &[Value::Int(99), Value::Int(7)]
            ),
            None
        );
        assert_eq!(heap.get_field_as(actual, 0, b'I'), Value::Int(41));
        assert_eq!(
            heap.get_field_as(actual, 5, b'L'),
            Value::Object(Some(child))
        );
        // Validate every slot before the first store: a declined batch leaves
        // its prefix untouched for the caller's ordinary per-slot replay.
        assert_eq!(
            heap.try_set_compact_primitive_fields(
                actual,
                &[(0, b'I'), (6, b'I')],
                &[Value::Int(99), Value::Int(7)]
            ),
            None
        );
        assert_eq!(heap.get_field_as(actual, 0, b'I'), Value::Int(41));
        assert_eq!(
            heap.try_set_compact_primitive_fields(
                actual,
                &[(0, b'I'), (1, b'Z')],
                &[Value::Object(Some(child)), Value::Int(1)]
            ),
            None
        );
        let legacy = heap.alloc_object(ClassId::new(cid + 11), 6);
        assert_eq!(
            heap.try_set_compact_primitive_fields(
                legacy,
                &[(0, b'I'), (1, b'I')],
                &[Value::Int(1), Value::Int(2)]
            ),
            None
        );
        // Reference reads must still unwrap the compatibility wrapper, before
        // descriptor coercion; compare the existing scalar accessor.
        heap.set_field(actual, 5, Value::Int(123));
        assert_eq!(
            heap.try_get_compact_fields(actual, &[(0, b'I'), (5, b'I')], &mut untouched),
            Some(actual)
        );
        assert_eq!(untouched[1], heap.get_field_as(actual, 5, b'I'));
        assert_eq!(untouched[1], Value::Int(123));
        assert_eq!(
            heap.try_get_compact_fields(legacy, &[(0, b'I'), (1, b'I')], &mut untouched),
            None
        );
        // Sparse/large layouts keep the scalar path (including fragmented G1).
        cratonvm_types::register_class_layout(
            domain,
            cid + 12,
            Arc::new(CompactLayout {
                field_disps: vec![8, 1024],
                is_ref: vec![false, false],
                field_kinds: vec![FieldStorageKind::Int; 2],
                ref_disps: vec![],
                total_size: 1032,
            }),
        );
        let sparse = heap.alloc_object(ClassId::new(cid + 12), 2);
        assert_eq!(
            heap.try_get_compact_fields(sparse, &[(0, b'I'), (1, b'I')], &mut untouched),
            None
        );
        assert_eq!(
            heap.try_set_compact_primitive_fields(
                sparse,
                &[(0, b'I'), (1, b'I')],
                &[Value::Int(1); 2]
            ),
            None
        );
        let array = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 6);
        assert_eq!(
            heap.try_get_compact_fields(array, &[(0, b'I'), (1, b'I')], &mut untouched),
            None
        );
        assert_eq!(
            heap.try_set_compact_primitive_fields(
                array,
                &[(0, b'I'), (1, b'I')],
                &[Value::Int(1), Value::Int(2)]
            ),
            None
        );
    }

    #[test]
    fn generational_compact_primitive_batch_preserves_scalar_semantics() {
        exercise(GcBackend::Generational, 61_431);
    }
    #[test]
    fn g1_compact_primitive_batch_preserves_scalar_semantics() {
        exercise(GcBackend::G1, 61_432);
    }
    #[cfg(feature = "zgc")]
    #[test]
    fn zgc_compact_primitive_batch_preserves_scalar_semantics() {
        exercise(GcBackend::Zgc, 61_433);
    }

    /// zt t18/zb1 (zr1br A2a): the batch read reads reference slots raw, so an
    /// ARMED ZGC heap declines it to the per-slot accessors (which barrier),
    /// and the per-slot answers are the batch's unarmed answers.
    /// `zgc_load_barrier_armed` follows the heap, and is false on the others.
    #[cfg(feature = "zgc")]
    #[test]
    fn zt18zb1_an_armed_zgc_heap_declines_the_compact_batch_read() {
        let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
        let VmHeap::Zgc(z) = &heap else {
            panic!("a ZGC heap");
        };
        let cid = 61_434;
        cratonvm_types::register_class_layout(
            z.layout_domain(),
            cid,
            Arc::new(CompactLayout {
                field_disps: vec![8, 16],
                is_ref: vec![false, true],
                field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
                ref_disps: vec![16],
                total_size: 24,
            }),
        );
        let target = heap.alloc_object(ClassId::new(1), 0);
        let obj = heap.alloc_object(ClassId::new(cid), 2);
        assert!(cratonvm_types::is_compact_object(heap.get_header(obj)));
        heap.set_field_as(obj, 0, Value::Int(5), b'I');
        heap.set_field_as(obj, 1, Value::Object(Some(target)), b'L');
        let slots = [(0, b'I'), (1, b'L')];

        let mut unarmed = [Value::Uninitialized; 2];
        let served = heap.try_get_compact_fields(obj, &slots, &mut unarmed);
        assert!(!heap.zgc_load_barrier_armed());

        assert!(z.arm_load_barrier_plain_stores(false));
        let armed_flag = heap.zgc_load_barrier_armed();
        let mut armed = [Value::Uninitialized; 2];
        let declined = heap.try_get_compact_fields(obj, &slots, &mut armed);
        let scalar: Vec<Value> = slots
            .iter()
            .map(|&(i, d)| heap.get_field_as(obj, i, d))
            .collect();
        z.set_barrier_color(None);

        assert_eq!(served, Some(obj), "unarmed: the batch serves");
        assert!(armed_flag, "armed: the predicate follows the heap");
        assert_eq!(declined, None, "armed: the batch declines");
        assert_eq!(armed, [Value::Uninitialized; 2], "and writes nothing");
        assert_eq!(
            scalar,
            unarmed.to_vec(),
            "the per-slot path answers the same"
        );
        assert_eq!(unarmed[1], Value::Object(Some(target)));
    }

    /// zt t19/zb2 (zr1br A2b): the JIT helpers' read seam
    /// (`load_ref_slot_barriered`, what `jit_aaload` and `jit_getfield`'s
    /// compact reference arm read through) decodes a COLORED element to its
    /// object and null to 0, and `zgc_load_barrier_colored_stores` follows
    /// the heap's mode: off unarmed, off in plain-store mode, on in
    /// colored-store mode.
    #[cfg(feature = "zgc")]
    #[test]
    fn zt19zb2_the_jit_read_seam_decodes_colored_elements() {
        // zt t20/zb3: one colored-store heap per process at a time (the
        // colored-slot decode base is owned).
        let _colored = crate::zgc::tests::colored_arming_test_lock();
        let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
        let VmHeap::Zgc(z) = &heap else {
            panic!("a ZGC heap");
        };
        let target = heap.alloc_object(ClassId::new(1), 0);
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
        let unarmed = heap.zgc_load_barrier_colored_stores();
        assert!(z.arm_load_barrier_plain_stores(false));
        let plain_mode = heap.zgc_load_barrier_colored_stores();
        z.set_barrier_color(None);

        assert!(z.arm_load_barrier_colored_stores());
        let colored_mode = heap.zgc_load_barrier_colored_stores();
        heap.set_array_element(arr, 0, Value::Object(Some(target)))
            .expect("in bounds");
        let slot0 = heap.array_data_ptr(arr).expect("an array") as *const u8;
        // SAFETY: element 1 of the same live two-element reference array.
        let slot1 = unsafe { slot0.add(8) };
        // SAFETY: both are live 8-aligned reference elements.
        let word0 = unsafe { std::ptr::read_volatile(slot0 as *const u64) };
        let read0 = unsafe { heap.load_ref_slot_barriered(slot0) };
        let read1 = unsafe { heap.load_ref_slot_barriered(slot1) };
        z.set_barrier_color(None);

        assert!(
            !unarmed && !plain_mode && colored_mode,
            "the predicate follows the mode"
        );
        assert!(
            crate::zgc::vaddr::is_colored_word(word0),
            "the element holds a colored word: {word0:#x}"
        );
        assert_eq!(read0, target.as_ptr() as u64, "decoded to its object");
        assert_eq!(read1, 0, "null stays 0");
    }
}

#[cfg(test)]
mod gcj2jt2_post_only_completion_tests {
    use super::*;

    /// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): the take-over's
    /// completion is a no-op until a post-only site is counted in the card
    /// view, then dirties the card of an in-range (old-generation) register
    /// word and ignores the rest; G1 never completes anything (it publishes no
    /// generational card view, so no post-only site can exist for it).
    #[test]
    fn gcj2jt2_the_completion_waits_for_a_post_only_site_and_is_generational_only() {
        const MB: usize = 1024 * 1024;
        let heap = VmHeap::new(GcBackend::Generational, 16 * MB);
        let (view, old_base, old_end) = heap
            .jit_card_table_info()
            .expect("a generational card view");
        assert!(old_base + 64 < old_end);
        let word = old_base + 64;
        assert_eq!(heap.jit_post_only_card_sites(), 0);
        assert_eq!(
            heap.takeover_complete_post_only_card_marks(&[word]),
            0,
            "no post-only site compiled yet: nothing owed"
        );
        // The compiler's bump (the JIT writes this word through the view).
        // SAFETY: `view` is the live table's boxed `JitCardView`, which lives
        // as long as `heap`.
        let v = unsafe { &*(view as *const crate::card_table::JitCardView) };
        v.post_only_sites
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        assert_eq!(heap.jit_post_only_card_sites(), 1);
        assert_eq!(
            heap.takeover_complete_post_only_card_marks(&[0, word, old_end]),
            1,
            "only the old-generation word"
        );
        let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
        assert_eq!(g1.jit_post_only_card_sites(), 0);
        assert_eq!(g1.takeover_complete_post_only_card_marks(&[word]), 0);
    }
}
