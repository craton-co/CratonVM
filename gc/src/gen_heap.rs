// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational garbage-collected heap.
//!
//! Combines a young generation (semi-space copying) with an old generation
//! (non-moving free-list mark-sweep) and a card table for efficient
//! old→young reference tracking.
//!
//! ## Allocation
//! All new objects are allocated in the young generation's from-space via
//! bump-pointer allocation. Objects are promoted to the old generation
//! after surviving [`PROMOTION_AGE`] minor GC cycles.
//!
//! ## Minor GC
//! Triggered when young from-space exceeds [`YOUNG_GC_THRESHOLD_PERCENT`]%.
//! Copies live young objects to young to-space (incrementing age), or
//! promotes them to old gen if their age reaches the threshold.
//! Dirty card table entries are scanned for old→young references as
//! additional roots.
//!
//! ## Major GC
//! Triggered when old gen runs out of space during promotion. Performs a
//! mark-sweep of the old generation, freeing unreachable objects back to
//! the free list.

use std::backtrace::Backtrace;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use std::sync::Arc;

use crate::arena::Arena;
use crate::card_table::CardTable;
use crate::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::ConcurrentGcState;
use crate::gc::GcResult;
use crate::heap::{
    array_data_size, read_prim_element, write_prim_element, ArrayElementType, ObjectHeader,
    ObjectKind, AUTOBOX_CLASS_ID, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use crate::old_gen::OldGen;
use crate::satb::SatbQueue;
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of each young-generation semi-space.
///
/// Bumped from 16 MB to 64 MB to reduce minor-GC frequency under
/// allocation-heavy workloads (the RealWorldBench "GC stress" phase
/// was 26× slower than HotSpot because it triggered full GC after
/// every ~16 MB of allocation). HotSpot defaults to ~25% of max heap
/// for the young gen; 64 MB for a 256 MB max heap is the same ratio.
const DEFAULT_YOUNG_SEMI_SIZE: usize = 64 * 1024 * 1024;

/// Size of the old generation (128 MB).
const DEFAULT_OLD_GEN_SIZE: usize = 128 * 1024 * 1024;

/// Number of minor GC survivals before an object is promoted to old gen.
const PROMOTION_AGE: u8 = 3;

/// GC threshold: trigger minor GC when young from-space usage exceeds this %.
const YOUNG_GC_THRESHOLD_PERCENT: usize = 50;

/// "Humongous" object threshold as a percentage of the young semi-space
/// capacity.  Allocations whose total in-memory footprint
/// (`HEADER_SIZE + array_data_size`) exceeds this fraction of one young
/// semi-space are routed directly to the old generation, bypassing
/// the young from-space entirely (G1-style humongous handling).
///
/// Without this routing, a single large array of size `A` would force
/// the user to size `--Xmx` to at least `4 * A` just so the array can
/// fit in *one* young semi-space (each semi is `Xmx / 4` —
/// see [`with_capacity`]).  A 2 GiB int[] at `--Xmx 16g` (4 GiB
/// semi) fits; at `--Xmx 12g` (3 GiB semi) it would not, even though
/// the heap has 12 GiB free.  HotSpot avoids the same trap by
/// sending oversized arrays straight to old gen / a humongous region.
///
/// Threshold chosen at 50%: large enough that small surviving sets
/// in young can still copy without colliding with a humongous tail
/// (Cheney needs to be able to fit copies into to-space), small enough
/// that any single array bigger than half the young semi-space takes
/// the humongous path instead of guaranteeing a copying-collector OOM.
const HUMONGOUS_YOUNG_FRACTION_PERCENT: usize = 50;

/// Maximum allowed heap expansion factor (4x the initial size).
const MAX_HEAP_EXPANSION_FACTOR: usize = 4;

/// If GC reclaims less than this fraction of young gen, expand the heap.
const GC_EXPANSION_THRESHOLD_PERCENT: usize = 25;

/// If GC reclaims less than this fraction of young gen, arm the
/// "promote-on-pressure" flag so the NEXT minor GC promotes ALL
/// survivors to old gen regardless of age. Mirrors HotSpot's
/// "premature promotion" / "always tenure" behaviour for high-survival
/// cycles: when the survival rate is this high (>75%) the working set
/// is effectively long-lived, and another semi→semi copy would just
/// repeat the same problem on the next cycle.
const GC_PROMOTE_PRESSURE_PERCENT: usize = 25;

// ---------------------------------------------------------------------------
// GenerationalHeap
// ---------------------------------------------------------------------------

/// Statistics for the generational heap.  All counters are
/// monotonically non-decreasing; use [`HeapStats::snapshot`] to capture a
/// consistent view at one point in time.  This is Phase H (RH.1) — a
/// hardening-only addition so tests can assert that allocation pressure
/// does not corrupt promotion bookkeeping (every object is accounted
/// for exactly once).
/// DBG: count of corrupt headers the non-moving sweep has detected. The VM's
/// `maybe_gc` reads this and, under `CRATONVM_DBG_CORRUPT_FRAMES`, dumps the
/// mutator's Java stack the first time it increases — close to the corruptor
/// when run with a tiny young gen (frequent GC).
pub static SWEEP_CORRUPTION_HITS: AtomicU64 = AtomicU64::new(0);

/// DBG: optional young-GC stress threshold (bytes) from CRATONVM_DBG_GC_STRESS.
fn gc_stress_threshold() -> Option<usize> {
    use std::sync::OnceLock;
    static S: OnceLock<Option<usize>> = OnceLock::new();
    *S.get_or_init(|| {
        std::env::var("CRATONVM_DBG_GC_STRESS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&v| v > 0)
    })
}

#[derive(Default, Debug)]
pub struct HeapStats {
    /// Number of minor GC cycles completed.
    minor_gc_count: AtomicU64,
    /// Number of major (old-gen mark-sweep) GC cycles completed.
    major_gc_count: AtomicU64,
    /// Total bytes copied in the young gen across all minor GCs.
    bytes_copied_young: AtomicU64,
    /// Total objects copied in the young gen across all minor GCs.
    objects_copied_young: AtomicU64,
    /// Total bytes promoted from young→old across all minor GCs.
    bytes_promoted: AtomicU64,
    /// Total objects promoted from young→old across all minor GCs.
    objects_promoted: AtomicU64,
    /// Total bytes freed by major GC sweep phases.
    bytes_freed_old: AtomicU64,
    /// Number of young allocations since startup.
    young_allocations: AtomicU64,
    /// Number of old-gen direct allocations (e.g. humongous) since
    /// startup.  Currently all allocations go through young, but the
    /// counter is exposed for future humongous handling.
    old_allocations: AtomicU64,
}

/// Immutable snapshot of [`HeapStats`] at a moment in time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStatsSnapshot {
    pub minor_gc_count: u64,
    pub major_gc_count: u64,
    pub bytes_copied_young: u64,
    pub objects_copied_young: u64,
    pub bytes_promoted: u64,
    pub objects_promoted: u64,
    pub bytes_freed_old: u64,
    pub young_allocations: u64,
    pub old_allocations: u64,
}

impl HeapStats {
    pub fn snapshot(&self) -> HeapStatsSnapshot {
        // `Relaxed` is fine for monotonic counters sampled from outside
        // a GC pause — snapshot consistency is not guaranteed and
        // callers only use this for observability / regression tests.
        HeapStatsSnapshot {
            minor_gc_count: self.minor_gc_count.load(Ordering::Relaxed),
            major_gc_count: self.major_gc_count.load(Ordering::Relaxed),
            bytes_copied_young: self.bytes_copied_young.load(Ordering::Relaxed),
            objects_copied_young: self.objects_copied_young.load(Ordering::Relaxed),
            bytes_promoted: self.bytes_promoted.load(Ordering::Relaxed),
            objects_promoted: self.objects_promoted.load(Ordering::Relaxed),
            bytes_freed_old: self.bytes_freed_old.load(Ordering::Relaxed),
            young_allocations: self.young_allocations.load(Ordering::Relaxed),
            old_allocations: self.old_allocations.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters to zero.  Useful in tests that want to
    /// observe a single GC cycle in isolation.
    pub fn reset(&self) {
        self.minor_gc_count.store(0, Ordering::Relaxed);
        self.major_gc_count.store(0, Ordering::Relaxed);
        self.bytes_copied_young.store(0, Ordering::Relaxed);
        self.objects_copied_young.store(0, Ordering::Relaxed);
        self.bytes_promoted.store(0, Ordering::Relaxed);
        self.objects_promoted.store(0, Ordering::Relaxed);
        self.bytes_freed_old.store(0, Ordering::Relaxed);
        self.young_allocations.store(0, Ordering::Relaxed);
        self.old_allocations.store(0, Ordering::Relaxed);
    }
}

/// A generational garbage-collected heap.
///
/// Young generation: two semi-spaces (from/to) using Cheney copying.
/// Old generation: non-moving free-list allocator with mark-sweep.
/// Card table: tracks old→young cross-generation references.
pub struct GenerationalHeap {
    /// Young generation from-space (allocation target).
    young_from: Mutex<Arena>,
    /// Young generation to-space (GC copy target).
    young_to: Mutex<Arena>,
    /// Old generation (promoted objects).
    old_gen: Mutex<OldGen>,
    /// Lock-free cached address bounds `[base, end)` of the three storage
    /// regions (young from-space, young to-space, old gen), published whenever
    /// the regions are (re)allocated so [`is_object_address`] can do its
    /// region-containment check WITHOUT taking the three arena mutexes.
    ///
    /// Motivation: `is_object_address` is called per operand-stack object on
    /// every object-returning native call (via `update_root_snapshot`) — under
    /// load that is millions of calls, and the old `young_from.lock() ||
    /// young_to.lock() || old_gen.lock()` triple-lock contended hard with the
    /// concurrent GC/allocator, blowing per-call cost up ~1000x during embedded
    /// webapp deployment. Reading these atomics instead removes the contention.
    ///
    /// Correctness (NO false negatives — a missed live region would drop a root):
    /// the bounds are refreshed (a) at construction and (b) at the start AND end
    /// of every GC cycle (the only place the young arenas swap/grow; the old gen
    /// never reallocates). Refreshing at GC start captures pre-collection bounds
    /// for the marking phase; the single `young_to.grow` happens near GC end on
    /// the *empty* to-space (no live objects to miss), after which the end
    /// refresh republishes. Mutators only read between GC cycles (GC is STW), so
    /// they always observe current bounds. Acquire/Release ordering pairs the
    /// GC-side stores with the mutator-side loads. Initialised to the empty
    /// range `[0,0)` so a load before the first publish matches nothing.
    region_bounds: [(AtomicUsize, AtomicUsize); 3],
    /// Card table covering the old generation's address space.
    ///
    /// T5.5.2 (HIGH-1 fix): the table now uses interior mutability for
    /// all state, so no outer `Mutex` is needed. The mutator write
    /// barrier calls [`CardTable::thread_local_dirty_addr`] on the
    /// shared reference (per-thread buffer, no shared lock); the
    /// collector calls [`CardTable::flush_all`] +
    /// [`CardTable::drain_pending`] at GC start to fold queued offsets
    /// into the authoritative bitmap before the dirty-card scan.
    card_table: CardTable,
    /// Next identity hash code.
    next_hash_code: AtomicI32,
    /// Young GC threshold in bytes.
    young_gc_threshold: Mutex<usize>,
    // Volatile field access uses `SeqCst` fences inside
    // `get_field_volatile`/`set_field_volatile`; no global lock is
    // needed (and the previous `Mutex<()>` here serialised every
    // volatile access across the entire heap, mirroring nothing in
    // the JMM). Removed to match the fence-only approach used by
    // `heap::Heap`.
    /// Global SATB queue for concurrent GC write barrier logging.
    /// Shared with the concurrent marker; `None` if concurrent GC is not enabled.
    satb_queue: Option<Arc<SatbQueue>>,
    /// Concurrent GC phase tracker. Shared with the concurrent marker.
    concurrent_gc_state: Option<Arc<ConcurrentGcState>>,
    /// Maximum young semi-space size (limits growth).
    max_young_semi_size: usize,
    /// Primary NUMA node hint for the young-gen slow path.
    ///
    /// Records the topology's preferred home node for this heap (defaults
    /// to 0 on single-node hosts, which covers every current test target).
    /// On a multi-node host this is the node whose arena the slow path
    /// would prefer if/when [`try_alloc_young`]/[`refill_tlab`] grow into a
    /// per-node arena layout.
    ///
    /// TODO(numa-multi-arena): replace the single `young_from`/`young_to`
    /// pair with `Arc<Vec<Mutex<Arena>>>` keyed by node id, and route the
    /// slow path through `numa::current_thread_node()` for the local arena.
    /// The fast path (TLAB) is per-thread so already NUMA-local by
    /// construction; only the refill/spill paths need plumbing. Today this
    /// field is consumed only by the debug/tracing stub in
    /// [`numa_slow_path_hint`] so the wiring lands without churning the
    /// arena layout — see numa_node_hint accessor below.
    numa_node_hint: usize,
    /// Total number of NUMA nodes seen at construction time (>=1).
    ///
    /// Cached so the slow path doesn't have to re-query
    /// [`crate::numa::global_topology`] on every allocation. Used by the
    /// stubbed hint logic to short-circuit on single-node hosts (preserving
    /// existing behavior exactly).
    numa_num_nodes: usize,
    /// Phase H (RH.1) statistics — updated during every minor/major GC.
    stats: HeapStats,
    /// Promote-on-pressure flag: when the previous minor GC reclaimed
    /// less than [`GC_PROMOTE_PRESSURE_PERCENT`]% of young from-space
    /// (i.e. survival rate was very high), the NEXT minor GC promotes
    /// every survivor to old gen regardless of its age. This breaks the
    /// "semispace death spiral" that occurs with long-lived heap shapes
    /// like the classic binary-trees benchmark — a single ~tens-of-MB
    /// tree that's kept live for the entire run would otherwise be
    /// copied semi→semi on every minor GC forever, eventually OOM'ing
    /// when the long-lived data plus the young allocations no longer
    /// fit in a single semi-space.
    ///
    /// `AtomicBool` so the flag can be read/written without going
    /// through the per-arena mutex.
    force_promote_all: std::sync::atomic::AtomicBool,
}

// SAFETY: Same reasoning as Heap — raw pointers are to internally owned
// memory and the `Mutex` on each sub-allocator serialises access.
//
// HIGH-soundness audit: the moving-collection entry points
// (`collect_garbage`, `collect_garbage_with_finalizers`) now require
// `&StopTheWorldToken` so cross-thread callers cannot trigger evacuation
// without first parking every other mutator. The blanket `unsafe impl` is
// retained because the heap's internal raw pointers are still `!Send` /
// `!Sync` on their own; the impl asserts that the STW-token gate plus the
// per-arena `Mutex` make shared `&GenerationalHeap` usage sound across
// threads.
unsafe impl Send for GenerationalHeap {}
unsafe impl Sync for GenerationalHeap {}

impl GenerationalHeap {
    /// Create a new generational heap with default sizes.
    pub fn new() -> Self {
        Self::with_sizes(DEFAULT_YOUNG_SEMI_SIZE, DEFAULT_OLD_GEN_SIZE)
    }

    /// Create a generational heap with custom young semi-space and old gen sizes.
    pub fn with_sizes(young_semi_size: usize, old_gen_size: usize) -> Self {
        let young_semi_size = young_semi_size.max(1024);
        let old_gen_size = old_gen_size.max(1024);
        let threshold = young_semi_size * YOUNG_GC_THRESHOLD_PERCENT / 100;
        let max_young = young_semi_size * MAX_HEAP_EXPANSION_FACTOR;

        let old_gen = OldGen::new(old_gen_size);
        let card_table = CardTable::new(old_gen.base_ptr() as usize, old_gen_size);

        // Cache the host NUMA shape at construction. The slow-path
        // allocator consults this through `numa_slow_path_hint`. On the
        // ~all-current-targets single-node case the hint is just 0 and
        // we behave exactly as before (no per-node arena yet — see TODO
        // on `numa_node_hint`).
        let topology = crate::numa::global_topology();
        let numa_num_nodes = topology.num_nodes.max(1);
        let numa_node_hint = if numa_num_nodes == 1 {
            0
        } else {
            // Multi-node: prefer the node of the constructing thread so
            // long-lived heap metadata lands near whoever booted the VM.
            // Falls back to 0 if the platform's current-thread probe is
            // unavailable, which keeps single-arena behavior stable.
            let n = crate::numa::NumaTopology::current_thread_node();
            if n < numa_num_nodes { n } else { 0 }
        };

        let heap = Self {
            young_from: Mutex::new(Arena::new(young_semi_size)),
            young_to: Mutex::new(Arena::new(young_semi_size)),
            old_gen: Mutex::new(old_gen),
            region_bounds: [
                (AtomicUsize::new(0), AtomicUsize::new(0)),
                (AtomicUsize::new(0), AtomicUsize::new(0)),
                (AtomicUsize::new(0), AtomicUsize::new(0)),
            ],
            card_table,
            next_hash_code: AtomicI32::new(1),
            young_gc_threshold: Mutex::new(threshold),
            satb_queue: None,
            concurrent_gc_state: None,
            max_young_semi_size: max_young,
            numa_node_hint,
            numa_num_nodes,
            stats: HeapStats::default(),
            force_promote_all: std::sync::atomic::AtomicBool::new(false),
        };
        // Publish the initial region bounds so the lock-free
        // `is_object_address` containment check is correct from the first
        // allocation (before any GC has run to refresh them).
        heap.refresh_region_bounds();
        heap
    }

    /// Republish the lock-free [`region_bounds`] cache from the live arenas.
    ///
    /// Locks each region briefly to read its current `[base, base+capacity)`
    /// and stores it with `Release` ordering. Called at construction and at the
    /// start/end of every GC cycle — the only points where a young arena's
    /// backing can move (swap/grow) or the heap is first sized. Cheap (3 short
    /// lock/read/store), and never on the hot mutator path.
    fn refresh_region_bounds(&self) {
        let yf = self.young_from.lock();
        let yt = self.young_to.lock();
        let og = self.old_gen.lock();
        self.store_region_bounds_locked(&yf, &yt, &og);
    }

    /// Store the three regions' `[base, end)` into [`region_bounds`] from
    /// already-held guards (used inside GC, where the arenas are locked and
    /// re-locking would deadlock). Mirror of [`refresh_region_bounds`].
    fn store_region_bounds_locked(&self, yf: &Arena, yt: &Arena, og: &OldGen) {
        let pairs = [
            (yf.base_ptr() as usize, yf.capacity()),
            (yt.base_ptr() as usize, yt.capacity()),
            (og.base_ptr() as usize, og.capacity()),
        ];
        for (slot, (base, cap)) in self.region_bounds.iter().zip(pairs) {
            slot.0.store(base, Ordering::Release);
            slot.1.store(base.wrapping_add(cap), Ordering::Release);
        }
    }

    /// Return the primary NUMA node hint recorded at construction.
    ///
    /// Public observer so tests and profiling code can confirm the heap
    /// picked up the expected node. The field itself remains private to
    /// keep room for the multi-arena refactor.
    pub fn numa_node_hint(&self) -> usize {
        self.numa_node_hint
    }

    /// Number of NUMA nodes the heap was constructed for (>=1).
    pub fn numa_num_nodes(&self) -> usize {
        self.numa_num_nodes
    }

    /// Compute the node the *calling* thread would prefer for a young-gen
    /// slow-path allocation, falling back to the heap's primary hint.
    ///
    /// This is the seam where the multi-arena refactor will plug in: it
    /// returns the index that `try_alloc_young`/`refill_tlab` would use to
    /// pick a per-node arena. Today there is only one arena pair, so the
    /// return value is consumed only by the tracing stub below — but the
    /// query path is exercised on every slow-path allocation, which means
    /// the platform probe and topology cache are validated in production
    /// long before we flip the multi-arena switch.
    #[inline]
    fn numa_slow_path_hint(&self) -> usize {
        if self.numa_num_nodes <= 1 {
            return self.numa_node_hint;
        }
        let n = crate::numa::NumaTopology::current_thread_node();
        if n < self.numa_num_nodes {
            n
        } else {
            self.numa_node_hint
        }
    }

    /// Return a handle to the GC statistics counters.  Counters are
    /// updated during GC cycles and allocation fast-paths.
    pub fn stats(&self) -> &HeapStats {
        &self.stats
    }

    /// Create a generational heap with a total capacity split proportionally.
    ///
    /// HotSpot-equivalent sizing: young gen takes ~50% of the total heap
    /// (the from+to semi-space pair together), split across two
    /// semi-spaces. So each semi-space is ~25% of -Xmx. HotSpot's default
    /// is closer to NewRatio=2 (young = 1/3 of heap) but our copying
    /// collector trades old-gen room for young-gen room more aggressively
    /// because (a) the non-moving sweep that runs while JIT frames are
    /// active cannot promote, and so depends on the young semi being big
    /// enough to hold the entire transient working set; and (b) Cheney
    /// copy cost scales with *survivors*, not capacity, so a larger
    /// young semi is essentially free unless the working set actually
    /// grows to fill it.
    ///
    /// For the common ranges:
    ///   - 64 KiB heap (test):   young_semi = 16 KiB
    ///   - 256 MiB heap (def.):  young_semi = 64 MiB
    ///   - 1 GiB heap:           young_semi = 256 MiB
    ///   - 4 GiB heap (-Xmx4g):  young_semi = 1 GiB
    ///   - 16 GiB heap:          young_semi = 4 GiB
    ///
    /// The young semi-space can still *grow* up to
    /// `young_semi * MAX_HEAP_EXPANSION_FACTOR` after a low-reclamation
    /// minor GC (see the expansion logic in `collect_garbage_inner`).
    pub fn with_capacity(total_bytes: usize) -> Self {
        let total = total_bytes.max(4096);
        // Young takes 1/2 of total, split across from+to semi-spaces (so
        // each semi gets 1/4 of total). The other half goes to old gen.
        let young_semi_raw = total / 4;
        // Clamp: floor at 512 bytes (the smallest test heap) so very
        // tiny test heaps still produce a non-trivial arena. No ceiling —
        // the user passed -Xmx N to get N bytes of heap, not "N capped
        // at some arbitrary internal constant".
        const YOUNG_SEMI_MIN: usize = 512;
        let young_semi = young_semi_raw.max(YOUNG_SEMI_MIN);
        // Old gen gets whatever's left after the young (from+to). Use
        // `saturating_sub` so a tiny `total` (`max(4096)`) doesn't
        // underflow when the clamped young is larger than half of it.
        let young_pair = young_semi.saturating_mul(2);
        let old_size = total.saturating_sub(young_pair).max(512);
        Self::with_sizes(young_semi, old_size)
    }

    // ----- Allocation --------------------------------------------------------

    /// Allocate a new Java object in the young generation.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        // Checked arithmetic — an overflowed `total_size` would size the
        // allocation incorrectly. Mirrors `try_alloc_object`'s checked path.
        let total_size = num_fields
            .checked_mul(SLOT_SIZE)
            .and_then(|fields_size| HEADER_SIZE.checked_add(fields_size))
            .unwrap_or_else(|| {
                eprintln!(
                    "FATAL: object size overflow in gen_heap alloc_object \
                     (num_fields={})",
                    num_fields
                );
                std::process::abort();
            });
        // The slot count MUST fit the `u32` header field. Clamping to
        // `u32::MAX` would write a count that disagrees with `total_size`,
        // causing later GC scans to walk off the end of the object.
        let num_slots_u32 = u32::try_from(num_fields).unwrap_or_else(|_| {
            eprintln!(
                "FATAL: object field count exceeds u32 in gen_heap alloc_object \
                 (num_fields={})",
                num_fields
            );
            std::process::abort();
        });
        // Young fast path; on exhaustion spill to old gen (non-moving) BEFORE
        // the hard abort. This panicking entry point is used by the native
        // `ctx.new_*` allocators, which cannot safely GC-and-retry (a moving
        // young GC would dangle their unrooted local ObjectRefs). See
        // [`try_alloc_object_old`]. Only when old gen is also full does
        // `alloc_young` fire the OOM diagnostic and abort.
        let ptr = match self.try_alloc_young(total_size) {
            Some(p) => p,
            None => {
                if let Some(obj) = self.try_alloc_object_old(class_id, num_fields) {
                    return obj;
                }
                self.alloc_young(total_size)
            }
        };

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            num_slots_u32,
        );

        // SAFETY: `ptr` was just bump-allocated from the young arena with sufficient
        // size (`HEADER_SIZE + num_fields * SLOT_SIZE`) and 8-byte alignment, so
        // writing an `ObjectHeader` at its start is valid. The pointer is non-null
        // and exclusively owned by this allocation; wrapping it in `ObjectRef` is
        // sound because the header has been fully initialized.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    /// Allocate a new Java object and initialize primitive-typed slots to
    /// their spec-mandated typed zero based on `descriptor_bytes`.
    ///
    /// See [`crate::heap::default_value_for_descriptor`] for the
    /// descriptor-byte-to-default-Value mapping. This method is the
    /// canonical allocation entry point for the VM's `new` bytecode path —
    /// it fixes a subtle correctness bug where primitive fields on a
    /// freshly-allocated object decoded as `Value::Object(None)` instead
    /// of the correctly-tagged zero, breaking `Unsafe.compareAndSetInt`
    /// comparisons against `Int(0)`.
    ///
    /// Fields beyond `descriptor_bytes.len()`, reference-typed (`L`/`[`)
    /// fields, and unknown-descriptor fields are initialized to an explicit
    /// `Value::Object(None)`, the reference-slot spec default.
    ///
    /// R-niche fix: zeroed memory no longer decodes as `Object(None)` (after
    /// the `NonNull` niche it decodes as `Int(0)`), so the `null` default must
    /// be written explicitly rather than left to `alloc_zeroed`. See
    /// [`crate::heap::Heap::alloc_object_with_descriptors`] for the full
    /// rationale.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| crate::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        obj
    }

    /// Fallible variant of [`Self::alloc_object_with_descriptors`] that
    /// returns `None` on young-gen exhaustion (caller should trigger GC
    /// and retry).
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        // R-niche fix: write every slot's default explicitly (primitive typed
        // zero, else `Object(None)`) — zeroed memory no longer decodes as null.
        // See `alloc_object_with_descriptors` for the rationale.
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| crate::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        Some(obj)
    }

    /// Allocate a new Java array in the young generation.
    ///
    /// Arrays use compact element sizes: 1 byte for boolean/byte, 2 for char/short,
    /// 4 for int/float, 8 for long/double/reference.
    ///
    /// **Humongous routing** (see [`HUMONGOUS_YOUNG_FRACTION_PERCENT`]):
    /// when `HEADER_SIZE + array_data_size` exceeds half of one young
    /// semi-space, the array is allocated directly in the old generation
    /// instead of young.  This mirrors HotSpot G1's humongous handling and
    /// unblocks the case where a single huge array would force the user
    /// to over-size `--Xmx` to ≥ 4× the array size just to make it fit in
    /// one young semi-space.
    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        let data_size = array_data_size(length, element_type)
            .unwrap_or_else(|_| { eprintln!("FATAL: array data size overflow in gen_heap alloc_array (length={}, element_type={:?})", length, element_type); std::process::abort(); });
        let total_size = HEADER_SIZE.checked_add(data_size).unwrap_or_else(|| {
            eprintln!(
                "FATAL: array size overflow in gen_heap alloc_array (length={}, element_type={:?})",
                length, element_type
            );
            std::process::abort();
        });
        // The length MUST fit the `u32` header field. Clamping to `u32::MAX`
        // would record a length that disagrees with the allocated `data_size`,
        // causing later GC scans to walk off the end of the array.
        let length_u32 = u32::try_from(length).unwrap_or_else(|_| {
            eprintln!(
                "FATAL: array length exceeds u32 in gen_heap alloc_array (length={}, element_type={:?})",
                length, element_type
            );
            std::process::abort();
        });

        // Humongous path: skip young, allocate straight into old gen.
        if self.is_humongous(total_size) {
            if let Some(obj) = self.try_alloc_array_humongous(class_id, element_type, length_u32) {
                return obj;
            }
            // Old gen was full — fall through to the young-gen path so the
            // standard OOM diagnostic fires (`alloc_young` aborts hard with
            // a young-gen-exhaustion message; a future humongous-OOM
            // diagnostic would live here).
        }

        // Young fast path; on exhaustion spill into old gen (non-moving) BEFORE
        // the hard abort. The panicking `alloc_array` is used by the native
        // `ctx.new_array`/`new_ref_array` allocators, which cannot safely
        // GC-and-retry (a moving young GC would dangle their unrooted local
        // ObjectRefs). `try_alloc_array_humongous` allocates in old gen
        // regardless of size; only when old gen is also full does `alloc_young`
        // fire the OOM diagnostic and abort. See [`try_alloc_object_old`].
        let ptr = match self.try_alloc_young(total_size) {
            Some(p) => p,
            None => {
                if let Some(obj) =
                    self.try_alloc_array_humongous(class_id, element_type, length_u32)
                {
                    return obj;
                }
                self.alloc_young(total_size)
            }
        };

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            length_u32,
            length_u32,
        );

        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // (`HEADER_SIZE + data_size`) and 8-byte alignment. Writing the header is valid
        // because the region is exclusively owned and properly sized. The data region
        // is already zeroed by `try_alloc_young()`. Wrapping in `ObjectRef` is sound
        // because the header is fully initialized.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            // Data region already zeroed by try_alloc_young() — no redundant memset needed.
            ObjectRef::from_raw(ptr)
        }
    }

    /// Try to allocate a Java object. Returns `None` if young gen is exhausted.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // M6 (round-12 gc): make the `+ HEADER_SIZE` add checked too, so a
        // near-`usize::MAX` field count can't wrap past the checked multiply.
        let total_size = HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?;
        let ptr = self.try_alloc_young(total_size)?;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).ok()?,
        );
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // and 8-byte alignment via `try_alloc_young`. The pointer is exclusively owned,
        // so writing the header and creating an `ObjectRef` are sound.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate a Java array. Returns `None` if young gen (or, for
    /// humongous arrays, the old gen) is exhausted.
    ///
    /// **Humongous routing** (see [`HUMONGOUS_YOUNG_FRACTION_PERCENT`]):
    /// when `HEADER_SIZE + array_data_size` exceeds half of one young
    /// semi-space, the array is allocated directly in the old generation
    /// (G1-style humongous handling).  Without this routing a single
    /// huge array would have to fit in one young semi (each is `Xmx / 4`),
    /// forcing the user to size `--Xmx` to at least `4 * array_size`.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;
        let length_u32 = u32::try_from(length).ok()?;

        // Humongous path: route straight to old gen so a single array
        // larger than half of one young semi-space doesn't force the
        // caller to over-size `--Xmx`.  See module docs on
        // `HUMONGOUS_YOUNG_FRACTION_PERCENT` for the rationale.
        if self.is_humongous(total_size) {
            if let Some(obj) =
                self.try_alloc_array_humongous(class_id, element_type, length_u32)
            {
                return Some(obj);
            }
            // Old gen full — fall through to the young path. If young is
            // also too small, the caller (`gc_alloc_array`) will trigger
            // a GC and retry, which may free old-gen space; if that still
            // can't satisfy the request, the standard OOM fires.
        }

        let ptr = self.try_alloc_young(total_size)?;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            length_u32,
            length_u32,
        );
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // for the array header + data and 8-byte alignment. The pointer is exclusively
        // owned, so writing the header and creating an `ObjectRef` are sound.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Returns `true` if a `total_size`-byte allocation should bypass
    /// young-gen and go straight to the old generation.
    ///
    /// Threshold = [`HUMONGOUS_YOUNG_FRACTION_PERCENT`] of one young
    /// semi-space capacity.  Computed from a live read of the
    /// `young_from` arena so the cap correctly tracks any in-flight
    /// expansion (see `collect_garbage_inner`'s adaptive growth path).
    #[inline]
    fn is_humongous(&self, total_size: usize) -> bool {
        let semi = self.young_from.lock().capacity();
        // Saturating arithmetic: a tiny semi-space (e.g. 1 KiB test heap)
        // can produce a 0-byte threshold under integer truncation — clamp
        // up to at least one allocation so the humongous path doesn't fire
        // on every small allocation in pathological cases.
        let threshold = (semi / 100).saturating_mul(HUMONGOUS_YOUNG_FRACTION_PERCENT);
        total_size > threshold.max(HEADER_SIZE)
    }

    /// Allocate a humongous array directly in the old generation.
    ///
    /// The returned object has `GC_FLAG_OLD_GEN` already set on its
    /// header, so the next minor GC will not try to copy it as if it
    /// lived in young from-space.  Returns `None` if old gen is full.
    fn try_alloc_array_humongous(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length_u32: u32,
    ) -> Option<ObjectRef> {
        // Recompute total_size from the (already-validated) length —
        // callers have all run `array_data_size(length, ..)?` upstream so
        // a fresh `array_data_size` cannot overflow here either, but use
        // checked arithmetic just in case the validated path is ever
        // narrowed in a future refactor.
        let data_size =
            array_data_size(length_u32 as usize, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;

        let ptr = {
            let mut og = self.old_gen.lock();
            og.alloc(total_size, 8)?
        };

        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            length_u32,
            length_u32,
        );
        // Mark as already-promoted so minor GC's `forward_object` does not
        // try to relocate this object — it lives in old gen, not the
        // young from-space arena that gets reset every minor cycle.
        // `OldGen::alloc` zeroed the data region; the header overwrite
        // below initializes the rest of the bookkeeping.
        header.gc_flags |= GC_FLAG_OLD_GEN;

        // SAFETY: `OldGen::alloc` returned a pointer to `total_size` bytes
        // of zeroed, 8-byte-aligned memory exclusive to this allocation.
        // Writing the header is in-bounds and the resulting `ObjectRef`
        // wraps a fully-initialized header.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
        }
        self.stats.old_allocations.fetch_add(1, Ordering::Relaxed);
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    /// Allocate a Java object DIRECTLY in the old generation (non-moving),
    /// used as an overflow fallback when the young from-space is exhausted.
    /// Returns `None` if the old gen is also full.
    ///
    /// This is the object analogue of [`try_alloc_array_humongous`] and exists
    /// for the same safety reason that motivates routing the *panicking*
    /// `alloc_object`/`alloc_array` here on young-full: those entry points are
    /// used by the convenience native allocators (`ctx.new_object`/`new_array`/
    /// `new_ref_array`, `alloc_concurrent_synthetic`, …). A native holds raw
    /// `ObjectRef`s in Rust locals that are NOT in any GC root set, so we cannot
    /// trigger a moving/promoting young GC from there (it would relocate those
    /// objects and leave the native's locals dangling — exactly the stale-ref
    /// class of SEGV). The old-gen allocator never relocates a live object, so
    /// spilling the single allocation into old gen lets a young-full native call
    /// succeed without GC instead of `std::process::abort()`-ing the whole VM.
    /// The `GC_FLAG_OLD_GEN` mark keeps minor GC from trying to forward it.
    fn try_alloc_object_old(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let total_size = HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?;
        let ptr = {
            let mut og = self.old_gen.lock();
            og.alloc(total_size, 8)?
        };
        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).ok()?,
        );
        header.gc_flags |= GC_FLAG_OLD_GEN;
        // SAFETY: `OldGen::alloc` returned `total_size` bytes of zeroed,
        // 8-byte-aligned memory exclusive to this allocation; writing the
        // header is in-bounds and the resulting `ObjectRef` is fully valid.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
        }
        self.stats.old_allocations.fetch_add(1, Ordering::Relaxed);
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    // ----- Header access -----------------------------------------------------

    /// Read the object header from a heap reference.
    pub fn get_header(&self, obj_ref: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj_ref` was created by one of this heap's `alloc_*` methods (or
        // forwarded during GC), so its pointer targets a valid, fully initialized
        // `ObjectHeader` within a heap-owned arena. The reference lifetime is bounded
        // by `&self`, ensuring the arena stays alive.
        unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
    }

    /// Get the class id of a heap object.
    pub fn class_id_of(&self, obj_ref: ObjectRef) -> ClassId {
        self.get_header(obj_ref).class_id
    }

    /// Get the kind (Object or Array) of a heap allocation.
    pub fn kind_of(&self, obj_ref: ObjectRef) -> ObjectKind {
        self.get_header(obj_ref).kind
    }

    /// Get the element type of an array object.
    pub fn element_type_of(&self, obj_ref: ObjectRef) -> ArrayElementType {
        self.get_header(obj_ref).element_type
    }

    /// Get the identity hash code of a heap object.
    pub fn identity_hash_code(&self, obj_ref: ObjectRef) -> i32 {
        self.get_header(obj_ref).identity_hash_code
    }

    /// Conservative validity check for a *raw address* — used by NEW-1.5
    /// JIT frame root scanning to filter spurious stack values.
    ///
    /// Returns `Some(ObjectRef)` if `addr` lands on a live object header in
    /// any of this heap's three storage regions (young from-space, young
    /// to-space, or old-gen). Returns `None` for any address that is null,
    /// outside every region, or fails the header sanity checks (alignment,
    /// kind tag, plausible num_slots).
    ///
    /// This is intentionally a structural check only — we do not consult
    /// the live-object marking bitmap or the class manager. False positives
    /// are acceptable because the caller treats every returned `ObjectRef`
    /// as a *root* (which only inflates retention; it cannot cause incorrect
    /// behavior). False negatives (missing a real object) would be wrong, so
    /// we err on the inclusive side.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        // Reject obvious garbage.
        if addr == 0 {
            return None;
        }
        // Object headers are 8-byte aligned (and HEADER_SIZE itself is a
        // multiple of 8). A pointer to a real object never has its low 3
        // bits set.
        if addr & 0x7 != 0 {
            return None;
        }
        let raw = addr as *const u8;

        // Region check: must fall inside one of the three arenas. LOCK-FREE —
        // read the cached `[base, end)` bounds (published under lock at
        // construction and at GC start/end; see `region_bounds`). The old
        // triple-mutex check (`young_from.lock() || young_to.lock() ||
        // old_gen.lock()`) ran per operand-stack object on every
        // object-returning native call and contended catastrophically with the
        // concurrent GC/allocator. The bounds can only change during a STW GC,
        // when no mutator is reading; the `Acquire` loads pair with the GC's
        // `Release` stores.
        let in_region = self.region_bounds.iter().any(|(base, end)| {
            let b = base.load(Ordering::Acquire);
            let e = end.load(Ordering::Acquire);
            addr >= b && addr < e
        });
        if !in_region {
            return None;
        }

        // SAFETY: The region check above confirmed `raw` is inside one of the
        // three arenas, so reading `HEADER_SIZE` bytes from it is valid memory.
        // The reference is short-lived and the arena locks are held.
        let header = unsafe { &*(raw as *const ObjectHeader) };

        // Validate the discriminated-union tag. Object/Array are the only
        // valid kinds; anything else means we landed in the middle of a
        // field or in stale memory. HumongousFiller is a synthetic
        // walker-sentinel (round-9 gc CRIT-1) and never represents a
        // real object reachable from a root.
        match header.kind {
            ObjectKind::Object | ObjectKind::Array => {}
            ObjectKind::HumongousFiller => return None,
        }
        // Cap num_slots at a sanity limit so a stale word can't fool us
        // into "validating" a slot count that would exceed the arena.
        // Multi-array reloc fix (2026-05-22): for arrays, `num_slots` is a
        // mirror of `array_length` (see `alloc_array` and `try_alloc_array`),
        // so a legitimate 256 MB int[] has num_slots = 2^26 > 1<<24 and
        // would be falsely rejected here. Bound num_slots only for non-arrays.
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24; // 16M slots → 256 MB obj
        let is_array = matches!(header.kind, ObjectKind::Array);
        if !is_array && header.num_slots > MAX_PLAUSIBLE_SLOTS {
            return None;
        }
        // Array length, if it's an array, must also be plausible. The JVM
        // spec caps arrays at `Integer.MAX_VALUE` elements, so use that as
        // the upper bound (matches `MAX_REASONABLE_ARRAY_LEN` in
        // `array_length()`).
        if is_array && header.array_length > i32::MAX as u32 {
            return None;
        }

        // SAFETY: `raw` passed the region containment and header sanity checks above,
        // so it points to a valid object header within a heap arena.
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    /// Loose validity check: alignment + region containment only.
    ///
    /// Unlike [`Self::is_object_address`] this does **not** read the object
    /// header. It is intended for GC root scanning of ambiguous slots (JVM
    /// long vs jobject smuggled as jlong) where the header may not yet be
    /// initialised, the pointer may target an interior offset, or the slot
    /// may transiently look like a pointer mid-construction.
    ///
    /// False positives (passing a non-object aligned heap-range address) are
    /// safe: the generational collector's `forward_object` already drops
    /// "suspected false roots" whose computed extent runs off the end of the
    /// from-space arena, so the worst case is over-retention rather than a
    /// deref of garbage.
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        let raw = addr as *const u8;
        let in_region = self.young_from.lock().contains(raw)
            || self.young_to.lock().contains(raw)
            || self.old_gen.lock().contains(raw);
        if !in_region {
            return None;
        }
        // SAFETY: alignment + region containment guarantee a valid heap
        // pointer; constructing an ObjectRef from it is safe — only deref
        // through it is gated by downstream header sanity checks.
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    // ----- Field access ------------------------------------------------------

    /// Get the value of a field at the given index.
    pub fn get_field(&self, obj_ref: ObjectRef, index: usize) -> Value {
        // KC16 SIGSEGV audit: runtime (not debug-only) bounds check. A
        // corrupted ObjectRef whose fake header has a huge num_slots would
        // happily pass the old debug_assert in release and then read
        // arbitrary memory.  For the suspect-header case we still panic
        // (that's a real bug).  For in-bounds-of-reality but out-of-bounds
        // of this object's layout (the common case when synthetic and
        // real-JDK class layouts disagree), return Object(None) — the
        // interpreter's primitive-field coercion will further normalize
        // it.  This converts what would be a silent SIGSEGV / panic into
        // a benign null read, matching HotSpot's behavior when an object
        // is accessed through a Reflection path that resolved a
        // larger-than-actual layout.
        let header = self.get_header(obj_ref);
        let num_slots = header.num_slots as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                kind_byte = header.kind as u8,
                gc_flags = format!("{:x}", header.gc_flags),
                "gen_heap::get_field: suspect header (returning null)",
            );
            // Rate-limited warn — do NOT panic. Returning null lets the
            // caller raise a normal Java exception / continue in a degraded
            // mode, matching how `array_length` is handled above. Without
            // this, Keycloak / Kafka tests abort the JVM on a corrupt
            // synthetic-receiver dispatch instead of surfacing a real
            // error to the Java side.
            return Value::Object(None);
        }
        if index >= num_slots {
            // Out-of-bounds field read. Resolve the class name + real
            // declared field count so the orchestrator can spot which of
            // two distinct bug classes this is:
            //
            //   (A) `real_field_count > num_slots` — TRUE undersized layout.
            //       The class declares more instance fields than the
            //       allocation site asked for, so even an own-class
            //       `getfield` can read past the object. This is the real
            //       allocator-side bug (the `PrintStream` 1-field-stub fix
            //       in commit cf1b478 was this case).
            //
            //   (B) `real_field_count <= num_slots` (typically equal) —
            //       CALLER-SIDE wrong-slot dispatch. The class layout is
            //       correct, but the caller used a slot index past the
            //       receiver's layout. The canonical pattern is a
            //       speculative collection-layout probe in
            //       `collect_collection_elements` / `read_java_string`
            //       reading slot 1 or 2 on a 1-slot object (e.g.
            //       `TypeList$Generic$Empty`, `RegularImmutableList`,
            //       `IdentityHashMap$Values`, `Collections$EmptyList`)
            //       or slot 0 on a 0-slot object (e.g. cglib's
            //       `MethodInterceptorGenerator`).
            //
            // Both cases return `Value::Object(None)` here so the caller
            // sees a benign null read instead of a SIGSEGV. The two cases
            // are distinguished by log level (error vs. warn) and message
            // text so the orchestrator's `grep undersized` continues to
            // flag (A) while (B) is triageable as a separate workstream.
            //
            // Rate-limit the diagnostics. `resolve_class_info` allocates a String
            // and the warn!/error! formats on EVERY OOB read, but the benign
            // case-(B) caller-side speculative probe (e.g. a collection-layout
            // probe landing on `Collections$EmptyMap`) fires thousands of times per
            // JUnit discovery — that unconditional per-call cost dominated kafka
            // `consumer.internals` discovery wall time. Cap the diagnostics to the
            // first N occurrences globally (a persistent case-(A) undersized-layout
            // bug surfaces well within N); past the cap the OOB read still returns a
            // benign null, just without the per-call logging. `CRATONVM_DBG_OOBFIELD`
            // forces full diagnostics regardless.
            static OOB_DIAG_COUNT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            const OOB_DIAG_CAP: u64 = 512;
            let oob_dbg = std::env::var_os("CRATONVM_DBG_OOBFIELD").is_some();
            if oob_dbg
                || OOB_DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    < OOB_DIAG_CAP
            {
            let (class_name, real_fields) =
                match crate::gc::resolve_class_info(header.class_id.as_u32()) {
                    Some((name, n)) => (name, Some(n)),
                    None => ("<unresolved>".to_string(), None),
                };
            // CRATONVM_DBG_OOBFIELD=<substr>: dump a backtrace for OOB field
            // reads whose class name contains <substr>, to localize the reader.
            if let Ok(want) = std::env::var("CRATONVM_DBG_OOBFIELD") {
                if !want.is_empty() && class_name.contains(&want) {
                    eprintln!(
                        "[OOBFIELD_ASRTAG_V1 READ] class={} index={} num_slots={}\n{}",
                        class_name,
                        index,
                        num_slots,
                        std::backtrace::Backtrace::force_capture()
                    );
                }
            }
            let is_true_undersized = real_fields.is_some_and(|n| n > num_slots);
            if is_true_undersized {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    obj = ?obj_ref.as_ptr(),
                    index,
                    num_slots,
                    class_id = ?header.class_id,
                    class_name = %class_name,
                    real_field_count = ?real_fields,
                    "gen_heap::get_field: out-of-bounds field read dropped \
                     (undersized object layout — class declares more fields \
                     than the object was allocated with)",
                );
            } else {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    obj = ?obj_ref.as_ptr(),
                    index,
                    num_slots,
                    class_id = ?header.class_id,
                    class_name = %class_name,
                    real_field_count = ?real_fields,
                    "gen_heap::get_field: out-of-bounds field read dropped \
                     (caller used slot index past receiver's layout — \
                     class layout is correct; the bug is in the caller's \
                     slot computation, typically a speculative \
                     collection-layout probe dispatched on a non-matching \
                     receiver type)",
                );
            }
            if std::env::var("CRATONVM_DBG_OOBFIELD").is_ok() {
                eprintln!(
                    "[OOBFIELD_ASRTAG_V1 READ] class={class_name} index={index} num_slots={num_slots}\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
            }
            } // end rate-limited OOB-read diagnostics
            return Value::Object(None);
        }
        // SAFETY: `obj_ref` points to a valid heap object and `index` is within
        // `num_slots` (checked above). `slot_ptr` computes
        // `obj_ref + HEADER_SIZE + index * SLOT_SIZE`, which is within the
        // object's allocated region. `read_slot` reads a `Value` from that address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            read_slot(ptr)
        }
    }

    /// Set the value of a field at the given index.
    ///
    /// Automatically fires the write barrier for generational GC correctness.
    /// Callers do NOT need to call `write_barrier` separately.
    pub fn set_field(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        // DIAGNOSTIC (bc math-ec): catch a fake `0x4`-style reference (a
        // collision-long payload) persisted into a heap field via ANY caller of
        // the heap set API (bytecode putfield, native ctx.set_field, reflection).
        if let Value::Object(Some(p)) = value {
            let a = p.as_ptr() as usize;
            if a != 0 && a < 0x1_0000 && std::env::var_os("CRATONVM_DBG_BADREF").is_some() {
                eprintln!(
                    "[BADREF:set_field] recv_class_id={} idx={} ptr=0x{:x}",
                    self.get_header(obj_ref).class_id.as_u32(), index, a,
                );
            }
        }
        // KC16 SIGSEGV audit: runtime bounds check.  Silently drop writes
        // that fall past the object's declared layout (layout mismatch
        // between synthetic and real JDK class shapes) rather than
        // overflowing into the neighboring object.  Still panic on
        // clearly-corrupt headers.
        let header = self.get_header(obj_ref);
        let num_slots = header.num_slots as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                kind_byte = header.kind as u8,
                gc_flags = format!("{:x}", header.gc_flags),
                value = ?value,
                "gen_heap::set_field: suspect header (returning silently)",
            );
            // Rate-limited warn — do NOT panic. See matching comment in
            // `get_field` above.  Returning without writing keeps the
            // JVM alive so the Java side can recover or surface a real
            // exception.
            return;
        }
        if index >= num_slots {
            // Out-of-bounds writes are dropped rather than corrupting the
            // neighboring object. The diagnostic distinguishes between two
            // distinct bug classes (matches the same split in `get_field`
            // above):
            //
            //   (A) `real_field_count > num_slots` — TRUE undersized layout.
            //       The class declares more instance fields than the
            //       allocation site asked for. This is the original smoking-
            //       gun case (PrintStream 1-field stub, commit cf1b478) and
            //       is still logged at `error!` level.
            //
            //   (B) `real_field_count <= num_slots` — CALLER-SIDE wrong slot.
            //       The class layout is correct; a writer used a slot index
            //       past the receiver's layout. Canonical instance: a
            //       JIT/interpreter path writes Class-mirror slots 96/97 on
            //       a 19-slot Class (`ignite.err`). Logged at `warn!` so the
            //       orchestrator's `grep -E "undersized"` still surfaces (A) —
            //       the real allocator bugs — while (B) is a separate triage
            //       workstream.
            let (class_name, real_fields) =
                match crate::gc::resolve_class_info(header.class_id.as_u32()) {
                    Some((name, n)) => (name, Some(n)),
                    None => ("<unresolved>".to_string(), None),
                };
            // CRATONVM_DBG_OOBFIELD=<substr>: dump a backtrace for OOB field
            // accesses whose class name contains <substr>, to localize the writer.
            if let Ok(want) = std::env::var("CRATONVM_DBG_OOBFIELD") {
                if !want.is_empty() && class_name.contains(&want) {
                    eprintln!(
                        "[OOBFIELD_ASRTAG_V1 WRITE] class={} index={} num_slots={} value={:?}\n{}",
                        class_name,
                        index,
                        num_slots,
                        value,
                        std::backtrace::Backtrace::force_capture()
                    );
                }
            }
            let is_true_undersized = real_fields.is_some_and(|n| n > num_slots);
            if is_true_undersized {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    obj = ?obj_ref.as_ptr(),
                    index,
                    num_slots,
                    class_id = ?header.class_id,
                    class_name = %class_name,
                    real_field_count = ?real_fields,
                    value = ?value,
                    "gen_heap::set_field: out-of-bounds field write dropped \
                     (undersized object layout — class declares more fields \
                     than the object was allocated with)",
                );
            } else {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    obj = ?obj_ref.as_ptr(),
                    index,
                    num_slots,
                    class_id = ?header.class_id,
                    class_name = %class_name,
                    real_field_count = ?real_fields,
                    value = ?value,
                    "gen_heap::set_field: out-of-bounds field write dropped \
                     (caller used slot index past receiver's layout — \
                     class layout is correct; the bug is in the caller's \
                     slot computation)",
                );
            }
            return;
        }
        debug_assert!(index < self.get_header(obj_ref).num_slots as usize);
        // SAFETY: Same as `get_field` — `index` is within `num_slots` so the
        // computed slot pointer is within the object's allocated region.
        // `write_slot` writes a `Value` at the computed address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            write_slot(ptr, value);
        }
        self.write_barrier(obj_ref, value);
    }

    /// Get the value of a volatile field.
    ///
    /// JLS §17.7 requires reads and writes of volatile long / double
    /// (and any volatile-declared field) to be atomic. The on-heap
    /// `Value` slot is 16 bytes (8-byte tag + 8-byte payload), wider
    /// than any stable Rust atomic primitive on x86-64, so a SeqCst
    /// fence pair gives the required happens-before ordering but does
    /// **not** by itself guarantee atomicity of the slot's read —
    /// a concurrent writer could leave the tag and payload words
    /// momentarily out of sync, surfacing as a torn long/double
    /// (e.g. high 32 bits from the old write, low 32 from the new).
    ///
    /// We close the atomicity hole with the striped-mutex pool in
    /// [`crate::collector::volatile_stripe_lock`]: paired
    /// `set_field_volatile` writers acquire the same stripe, so the
    /// 16-byte read here either observes a fully old or fully new
    /// `Value`. Striping keeps unrelated volatile fields from
    /// serializing across the heap (the previous design used a single
    /// heap-wide mutex which became a global bottleneck).
    pub fn get_field_volatile(&self, obj_ref: ObjectRef, index: usize) -> Value {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let val = self.get_field(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        val
    }

    /// Set the value of a volatile field.
    ///
    /// Acquires the per-slot stripe lock from
    /// [`crate::collector::volatile_stripe_lock`] to make the 16-byte
    /// `Value` write appear atomic to a concurrent volatile reader.
    /// SeqCst fences provide the JMM happens-before edge. The write
    /// barrier still fires inside `set_field`. See
    /// [`Self::get_field_volatile`] for the full rationale.
    pub fn set_field_volatile(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.set_field(obj_ref, index, value); // barrier fires inside set_field
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Descriptor-aware get — normalizes the returned `Value` to the declared
    /// field type. See [`crate::heap::coerce_field_value_by_descriptor`].
    pub fn get_field_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj_ref, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Volatile descriptor-aware get.
    pub fn get_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        desc_byte: u8,
    ) -> Value {
        let raw = self.get_field_volatile(obj_ref, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Descriptor-aware set — normalizes the written `Value` to the declared
    /// field type before the underlying slot write.
    pub fn set_field_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field(obj_ref, index, coerced);
    }

    /// Volatile descriptor-aware set.
    pub fn set_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field_volatile(obj_ref, index, coerced);
    }

    // ----- Array access ------------------------------------------------------

    /// Get the length of an array.
    pub fn array_length(&self, obj_ref: ObjectRef) -> usize {
        let header = self.get_header(obj_ref);
        if header.kind != ObjectKind::Array {
            // Defensive hardening: native/bootstrap code can occasionally pass a
            // plain object into array helpers. Returning 0 keeps startup alive,
            // while diagnostics below pinpoint the exact caller and object shape.
            //
            // Rate-limit the diagnostic: long-running boots (e.g. Keycloak)
            // can hit this path tens of thousands of times. Emitting a full
            // `Backtrace::force_capture` each time produces hundreds of MB
            // of stderr and exhausts disk. Keep the first few one-line
            // warnings (with backtrace gated by `CRATONVM_GC_ARRAY_GUARD_BT`)
            // and silently return 0 thereafter.
            static GUARD_COUNT: AtomicUsize = AtomicUsize::new(0);
            const GUARD_LIMIT: usize = 5;
            let n = GUARD_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < GUARD_LIMIT {
                // Format the entire message into one String and emit via a
                // single locked stderr write. Multi-segment `eprintln!`
                // formatting can interleave with other threads' stderr
                // writes on Windows and trigger the rare
                // "Имеющийся буфер не подходит" (os error 1784) panic on the
                // intermediate write of large {:?} pretty-printed values.
                //
                // CRITICAL: we are formatting fields of a header that we
                // already know is suspect (kind != Array). The raw bytes
                // backing `kind` and `element_type` may be garbage — using
                // `{:?}` on `#[repr(u8)]` enums whose byte value is outside
                // the declared variants is undefined behavior and has been
                // observed to provoke a `capacity overflow` panic deep in
                // `core::fmt` (the Display jump table writing an arbitrary
                // huge slice into the format buffer).
                //
                // Read the raw bytes directly via the object pointer rather
                // than via `&ObjectHeader` field access, so we never go
                // through the unsound enum-value path. The header layout is
                // `#[repr(C)]`: class_id(u32) @0, kind(u8) @4, elem(u8) @5.
                let obj_ptr = obj_ref.as_ptr();
                // SAFETY: `obj_ref` was previously dereferenced via
                // `get_header` above without faulting, so `obj_ptr` plus
                // `HEADER_SIZE` bytes is readable. We read individual bytes
                // and a 4-byte u32 within that range, which is well-defined
                // even when the byte values do not correspond to valid enum
                // discriminants.
                let (kind_byte, elem_byte, class_id_raw, stored_len) = unsafe {
                    let class_id_raw = (obj_ptr as *const u32).read_unaligned();
                    let kind_byte = *obj_ptr.add(4);
                    let elem_byte = *obj_ptr.add(5);
                    let stored_len = (obj_ptr.add(12) as *const u32).read_unaligned();
                    (kind_byte, elem_byte, class_id_raw, stored_len)
                };
                let msg = if std::env::var_os("CRATONVM_GC_ARRAY_GUARD_BT").is_some() {
                    let bt = Backtrace::force_capture();
                    format!(
                        "[GC-ARRAY-GUARD] array_length(non-array): kind_byte={} class_id={} elem_byte={} stored_len={} obj={:p} (#{}/{})\nbacktrace:\n{}\n",
                        kind_byte,
                        class_id_raw,
                        elem_byte,
                        stored_len,
                        obj_ptr,
                        n + 1,
                        GUARD_LIMIT,
                        bt,
                    )
                } else {
                    format!(
                        "[GC-ARRAY-GUARD] array_length(non-array): kind_byte={} class_id={} elem_byte={} stored_len={} obj={:p} (#{}/{}; set CRATONVM_GC_ARRAY_GUARD_BT=1 for backtrace)\n",
                        kind_byte,
                        class_id_raw,
                        elem_byte,
                        stored_len,
                        obj_ptr,
                        n + 1,
                        GUARD_LIMIT,
                    )
                };
                // Atomic write via locked stderr; ignore errors (best-effort
                // diagnostic). Avoids the eprintln panic on Windows when the
                // writer's internal buffer is unhappy.
                use std::io::Write;
                let stderr = std::io::stderr();
                let mut handle = stderr.lock();
                let _ = handle.write_all(msg.as_bytes());
                if n + 1 == GUARD_LIMIT {
                    let _ = handle.write_all(
                        b"[GC-ARRAY-GUARD] suppressing further array_length(non-array) warnings\n",
                    );
                }
            }
            return 0;
        }
        // Guard against a corrupt synthetic header whose length field is
        // garbage. The only sound ceiling is the JVM's own limit: a Java
        // array cannot exceed `Integer.MAX_VALUE` elements, so any larger
        // value is provably a corrupt header. An earlier `1 << 24` cap also
        // rejected *legitimate* arrays larger than 16M elements (e.g. a
        // 2^26-int vector), clobbering their length to 0 and raising a
        // spurious ArrayIndexOutOfBoundsException on every access.
        const MAX_REASONABLE_ARRAY_LEN: u32 = i32::MAX as u32; // JVM array ceiling
        let raw_len = header.array_length;
        if raw_len > MAX_REASONABLE_ARRAY_LEN {
            static SUSPECT_LEN_WARN_COUNT: AtomicU64 = AtomicU64::new(0);
            let n = SUSPECT_LEN_WARN_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 16 {
                // Format with raw byte values to avoid UB on Debug-formatting
                // a potentially-garbage `ArrayElementType` byte (same hazard
                // as the GC-ARRAY-GUARD path above).
                eprintln!(
                    "[GC-ARRAY-LEN-GUARD] suspect array_length={} (>1<<24) class_id={} elem_byte={} obj={:p} (n={})",
                    raw_len,
                    header.class_id.as_u32(),
                    header.element_type as u8,
                    obj_ref.as_ptr(),
                    n,
                );
            }
            return 0;
        }
        raw_len as usize
    }

    /// Bulk-read a char[] array into a `Vec<u16>`.
    ///
    /// Much faster than per-element `get_array_element` for reading Java
    /// String backing arrays — copies the raw 2-byte-per-element data directly.
    pub fn read_char_array_bulk(&self, obj_ref: ObjectRef) -> Vec<u16> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        debug_assert_eq!(header.element_type, ArrayElementType::Char);
        let len = header.array_length as usize;
        let mut out = vec![0u16; len];
        // SAFETY: `obj_ref` is a valid Char array with `len` elements (verified by
        // header assertions above). Each char element is 2 bytes, so `HEADER_SIZE`
        // to `HEADER_SIZE + len * 2` is within the allocation. The destination
        // buffer `out` is freshly allocated with the same length. The regions do
        // not overlap because `out` is on the Rust heap, not in the GC arena.
        unsafe {
            let src = obj_ref.as_ptr().add(HEADER_SIZE);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
        }
        out
    }

    /// Get a raw pointer to the start of the array data region.
    ///
    /// This is the address immediately after the object header. The caller is
    /// responsible for knowing the element type and bounds.
    pub fn array_data_ptr(&self, obj_ref: ObjectRef) -> *mut u8 {
        // SAFETY: `obj_ref` is a valid heap object whose allocation includes
        // `HEADER_SIZE` plus the data region, so advancing by `HEADER_SIZE`
        // yields a pointer within the allocation. Caller is responsible for
        // bounds and element-type correctness.
        unsafe { obj_ref.as_ptr().add(HEADER_SIZE) }
    }

    /// Get an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn get_array_element(&self, obj_ref: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        if index >= header.array_length as usize {
            return Err(index as i32);
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // was allocated with sufficient data space for all elements after the header.
        // `read_prim_element` reads the correctly typed element at the given index.
        unsafe {
            let base = obj_ref.as_ptr().add(HEADER_SIZE);
            Ok(read_prim_element(base, index, header.element_type))
        }
    }

    /// Like `get_array_element`, but auto-unboxes values stored by native
    /// collections. See `Heap::get_array_element_unboxing` for details.
    pub fn get_array_element_unboxing(
        &self,
        obj_ref: ObjectRef,
        index: usize,
    ) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        if index >= header.array_length as usize {
            return Err(index as i32);
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // data region is within the allocation.
        let value = unsafe {
            let base = obj_ref.as_ptr().add(HEADER_SIZE);
            read_prim_element(base, index, header.element_type)
        };
        if header.element_type == ArrayElementType::Reference {
            if let Value::Object(Some(obj)) = value {
                // The stored word is treated as an `ObjectRef`, but a stale or
                // garbage non-zero element could point anywhere. Validate it
                // against the heap arenas (region + alignment + header sanity)
                // before dereferencing it as an `ObjectHeader`. If it does not
                // look like a live heap object, skip the unboxing and return
                // the value as-is rather than performing a wild read.
                if self.is_object_address(obj.as_ptr() as usize).is_some() {
                    // SAFETY: `is_object_address` confirmed `obj` points to a
                    // valid object header inside one of this heap's arenas.
                    let obj_header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
                    if obj_header.class_id == AUTOBOX_CLASS_ID {
                        return Ok(self.get_field(obj, 0));
                    }
                }
            }
        }
        Ok(value)
    }

    /// Set an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn set_array_element(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
    ) -> Result<(), i32> {
        // DIAGNOSTIC (bc math-ec): catch a fake `0x4` reference stored into a
        // reference array element via the heap set API (bytecode aastore,
        // native System.arraycopy / clone / reflection).
        if let Value::Object(Some(p)) = value {
            let a = p.as_ptr() as usize;
            if a != 0 && a < 0x1_0000 && std::env::var_os("CRATONVM_DBG_BADREF").is_some() {
                eprintln!("[BADREF:set_array_element] idx={} ptr=0x{:x}", index, a);
            }
        }
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        if index >= header.array_length as usize {
            return Err(index as i32);
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // data region is within the allocation. `write_prim_element` writes at the
        // correct element offset. For reference arrays, autobox wrappers are
        // allocated on this heap and thus valid.
        unsafe {
            let base = obj_ref.as_ptr().add(HEADER_SIZE);
            // Compact ref arrays only store 8-byte pointers. If a non-Object
            // value is written (e.g. Value::Int from a native collection),
            // auto-box it into a 1-field wrapper object.
            if header.element_type == ArrayElementType::Reference {
                match value {
                    Value::Object(_) => {
                        write_prim_element(base, index, header.element_type, value);
                        // Write barrier: old-gen array storing young-gen ref
                        self.write_barrier(obj_ref, value);
                    }
                    _ => {
                        let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                        self.set_field(wrapper, 0, value);
                        let wrapper_val = Value::Object(Some(wrapper));
                        write_prim_element(base, index, header.element_type, wrapper_val);
                        // Write barrier: track the wrapper reference for gen GC
                        self.write_barrier(obj_ref, Value::Object(Some(wrapper)));
                    }
                }
            } else {
                write_prim_element(base, index, header.element_type, value);
            }
        }
        Ok(())
    }

    // ----- Write barrier -----------------------------------------------------

    /// Write barrier — called after every reference store into a heap object.
    ///
    /// Performs two functions:
    /// 1. **Card marking:** If source is in old gen and target is in young gen,
    ///    marks the card table entry dirty (for minor GC).
    /// 2. **SATB logging:** If concurrent marking is active, logs the *old*
    ///    reference value to the SATB queue (for concurrent GC correctness).
    ///
    /// Round-5 #11 (perf) — the previous implementation dereferenced the
    /// object header twice on the hot path: once to inspect
    /// `header.gc_flags` on the *source*, then a second time on the
    /// *target*. Each dereference is a cache-line load on a separate
    /// object and both are pure waste — the same information is already
    /// encoded in the address ranges of the old-gen arena. The card
    /// table caches those bounds (it must, in order to compute card
    /// indices), so we route the gen-check through it and skip both
    /// header reads entirely. The cost on the common path drops from
    /// "two L1/L2 misses + two flag tests" to "two integer range
    /// comparisons" — the same pattern that round-10 used for
    /// `post_write_barrier_rset` (per-thread region cache).
    #[inline]
    pub fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // FAST PATH: only reference stores can produce a cross-gen edge.
        // Bail out BEFORE touching either object header for primitive/null
        // stores so the barrier cost on the common (primitive) path is
        // a single tag check.
        let target_ref = match stored_value {
            Value::Object(Some(r)) => r,
            _ => return,
        };

        let src_addr = obj.as_ptr() as usize;
        let dst_addr = target_ref.as_ptr() as usize;

        // Round-5 #11 fix: replace the two header dereferences with two
        // pure address-range comparisons against the cached old-gen
        // bounds. The card table already stores `base_addr` and
        // `region_size` for the old-gen arena (it has to — every dirty
        // entry is indexed by `(addr - base) / CARD_SIZE`). Reading
        // those two `usize` fields touches one already-hot cache line
        // (`self.card_table`) instead of two cold object headers.
        let old_base = self.card_table.base_addr();
        let old_end = old_base.wrapping_add(self.card_table.region_size());

        // Source must live in the old-gen arena. If it doesn't, this is
        // either a young→young store (no card needed) or a write into
        // GC-internal scratch space; either way, nothing to record.
        if src_addr < old_base || src_addr >= old_end {
            return;
        }

        // Target must live OUTSIDE the old-gen arena (i.e. in young) for
        // the edge to be cross-generational. Old→old refs are followed
        // by full-heap marking, not the card table.
        if dst_addr >= old_base && dst_addr < old_end {
            return;
        }

        // T5.5.2 — route through the thread-local batched dirty path
        // instead of taking the global card_table mutex on every
        // reference store. The offset is queued in this mutator's
        // per-thread buffer (no shared lock) and flushed into the shared
        // `pending_offsets` either when the buffer hits
        // THREAD_BUFFER_FLUSH_THRESHOLD entries or when the collector
        // drains at GC start. The shared `cards` bitmap is updated by
        // `drain_pending` while the GC holds the card_table mutex
        // exclusively.
        self.card_table.thread_local_dirty_addr(src_addr);
    }

    /// SATB write barrier — called BEFORE a reference field is overwritten.
    ///
    /// If concurrent marking is active, logs the old reference value to the
    /// global SATB queue so the concurrent marker won't miss live objects.
    ///
    /// This should be called by the interpreter/JIT before every putfield/putstatic
    /// that overwrites a reference-typed field.
    #[inline]
    pub fn satb_barrier(&self, old_value: Value) {
        // Fast path: check if concurrent marking is active
        if let Some(ref state) = self.concurrent_gc_state {
            if !state.is_marking_active() {
                return;
            }
        } else {
            return;
        }

        // Only log reference values
        let old_ref = match old_value {
            Value::Object(Some(r)) => r,
            _ => return,
        };

        // Log the old reference to the per-thread SATB buffer. The buffer
        // auto-flushes into the global queue every ~256 entries, so the
        // hot write-barrier path takes no shared lock in the common case.
        if let Some(ref satb) = self.satb_queue {
            crate::satb::satb_thread_local_log(satb, old_ref.as_ptr() as usize);
        }
    }

    /// Enable concurrent GC support by attaching shared SATB and state.
    pub fn enable_concurrent_gc(
        &mut self,
        satb_queue: Arc<SatbQueue>,
        gc_state: Arc<ConcurrentGcState>,
    ) {
        self.satb_queue = Some(satb_queue);
        self.concurrent_gc_state = Some(gc_state);
    }

    /// Round-5 fix (CRIT — UAF): expose the SATB queue handle so the
    /// VM can drain per-thread SATB buffers at safepoint entry. Returns
    /// `None` until `enable_concurrent_gc` has been called (i.e. until
    /// the concurrent old-gen collector is wired up).
    pub fn satb_queue_handle(&self) -> Option<Arc<SatbQueue>> {
        self.satb_queue.clone()
    }

    /// Get the old generation's base pointer and capacity (for creating a ConcurrentMarker).
    pub fn old_gen_info(&self) -> (usize, usize) {
        let og = self.old_gen.lock();
        (og.base_ptr() as usize, og.capacity())
    }

    /// True when `addr` lies inside the old generation arena.
    ///
    /// Used by weak/soft/phantom reference processing after a **young** GC to
    /// decide whether a referent survived. A minor collection never touches the
    /// old generation, so every old-gen object is live for the purpose of
    /// reference clearing — the referent must NOT be cleared just because it was
    /// promoted out of the young space in an earlier cycle.
    ///
    /// Without this, `VmHeap::is_addr_live` returned `false` for the
    /// generational collector and `process_references_after_gc`'s
    /// `pointer_map.contains_key(addr) || is_addr_live(addr)` predicate cleared
    /// EVERY weak reference whose referent had been tenured to old gen. Visible
    /// symptom: a `WeakReference<ClassLoader>` (e.g. WildFly's
    /// `StandardResourceDescriptionResolver.bundleLoader`) read back null after
    /// the first young GC even though the classloader was strongly reachable,
    /// so `ResourceBundle.getBundle(.., null, ..)` threw `MissingResourceException`.
    pub fn is_old_gen_addr(&self, addr: usize) -> bool {
        self.old_gen.lock().contains(addr as *const u8)
    }

    /// Access the old generation directly (for concurrent sweep).
    /// Returns a lock guard.
    pub fn old_gen_lock(&self) -> parking_lot::MutexGuard<'_, OldGen> {
        self.old_gen.lock()
    }

    // ----- GC ----------------------------------------------------------------

    /// Returns true when the young generation should be collected.
    pub fn needs_gc(&self) -> bool {
        let from = self.young_from.lock();
        let used = from.used();
        // DBG: CRATONVM_DBG_GC_STRESS=<bytes> forces a young GC every <bytes>
        // of allocation, so the non-moving sweep's corruption detection fires
        // right after the corrupting write (the corruptor's interpreted caller
        // is then on the mutator stack dumped by CRATONVM_DBG_CORRUPT_FRAMES).
        if let Some(t) = gc_stress_threshold() {
            return used >= t;
        }
        // Trigger on LIVE occupancy, not the raw bump cursor. The non-moving,
        // JIT-frame-safe sweep (`sweep_young_non_moving`) reclaims dead objects
        // into the arena's free list WITHOUT retreating the cursor — it cannot
        // relocate survivors while conservative JIT roots are live — so `used()`
        // (== cursor, the high-water mark) stays pinned near capacity after such
        // a sweep even though most of that span is reusable free-list space that
        // `Arena::alloc` hands straight back out. Keying the trigger off the raw
        // cursor leaves `needs_gc` permanently true once the high-water mark
        // passes the threshold, so a young GC fires on essentially every
        // subsequent allocation: the bintrees18 / AllocLoop thrash (live set
        // fills young, ~150 no-progress sweeps reclaiming nothing, rc=124
        // timeout — `CRATONVM_DBG_SWEEP_EDGES` shows unmarked=0, edges=0 each
        // sweep). Subtracting the free-list bytes makes the metric reflect
        // genuinely-occupied space; the moving collector resets the cursor
        // itself, so this is a no-op there. The alloc-failure→GC-and-retry path
        // remains the hard backstop against fragmentation under-collection.
        let live = used.saturating_sub(from.free_list_bytes());
        live >= *self.young_gc_threshold.lock()
    }

    /// Total bytes currently allocated across young and old generations.
    pub fn allocated_bytes(&self) -> usize {
        self.young_from.lock().used() + self.old_gen.lock().used()
    }

    /// DBG (bc math-ec `0x4`): scan the young from-space for the FIRST object
    /// reference field (or ref-array element) holding `Object(Some(p))` with
    /// `0 < p < 0x1000` — the `0x4` corruption signature, which we proved is
    /// written to a YOUNG object by the mutator (between GCs), NOT by the GC.
    /// Returns `(holder_addr, class_id, field_idx, payload, nbr_disc)` where
    /// `nbr_disc` is the discriminant word of the NEXT cell (field_idx+1) — for
    /// a misaligned-by-8 `Value::Object` write the seed cell reads payload `4`
    /// AND the next cell's disc reads the stray write's real (large) payload,
    /// confirming the misalignment mechanism. `field_idx` has bit 0x4000_0000
    /// set for a ref-array element. Locks `young_from`; call only OUTSIDE a GC.
    pub fn dbg_first_young_small_ref(&self) -> Option<(usize, u32, usize, usize, u64)> {
        let from = self.young_from.lock();
        let base = from.base_ptr();
        let used = from.used();
        let base_a = base as usize;
        let end_a = base_a + used;
        // HEADER-INDEPENDENT brute-force: the corruption scribbles garbage into
        // young object HEADERS too, so any object-walk (even re-syncing) can be
        // desynced past the victim. Instead scan every 8-aligned word for the
        // raw 16-byte `Object(Some(0<p<0x1000))` bit pattern (disc word == 4,
        // payload word in (0,0x1000)), then BACK-VALIDATE the hit sits at a real
        // field-cell offset of a plausible non-array object — which filters out
        // primitive-array `{4, small}` data (the dominant young bytes in EC).
        let mut a = base_a;
        while a + 16 <= end_a {
            let disc = unsafe { std::ptr::read(a as *const u64) };
            if disc == 4 {
                let payload = unsafe { std::ptr::read((a + 8) as *const u64) };
                // Precise signature: the corruption payload is ALWAYS exactly 4
                // (== the Value::Object discriminant landing on a field payload).
                // Requiring ==4 (not just <0x1000) rejects transient
                // primitive-array `{4, n}` data (e.g. the startup 0x2c false
                // positive) and other small-but-not-4 values.
                if payload == 4 {
                    // Back-validate: for each candidate field index `fld`, the
                    // owning header would start at H = a - HEADER_SIZE - fld*16.
                    // Accept the first H that is in-arena, kind=Object,
                    // array_length==0, and num_slots in (fld, 1<<20].
                    let mut found: Option<(usize, u32, usize)> = None;
                    let max_fld = 256usize;
                    for fld in 0..max_fld {
                        let off = HEADER_SIZE + fld * SLOT_SIZE;
                        if a < base_a + off {
                            break;
                        }
                        let h_addr = a - off;
                        let h = unsafe { &*(h_addr as *const ObjectHeader) };
                        if (h.kind as u8) == 0
                            && h.array_length == 0
                            && (h.num_slots as usize) > fld
                            && h.num_slots <= (1 << 20)
                        {
                            // Filter: the candidate header's class must RESOLVE
                            // to a real class AND the corrupted field index `fld`
                            // must be a VALID field of that class (`fld < n`).
                            // This accepts a real victim whose num_slots differs
                            // from the resolved count (synthetic-vs-real layout,
                            // e.g. ECFieldElement$F2m) while rejecting bogus
                            // headers read out of neighbour bytes — notably
                            // cid=0 (java/lang/Object, 0 fields) which `is_some`
                            // alone wrongly accepted at fld[22].
                            let cid = h.class_id.as_u32();
                            let ok = crate::gc::resolve_class_info(cid)
                                .map(|(_, n)| fld < n)
                                .unwrap_or(false);
                            if ok {
                                found = Some((h_addr, cid, fld));
                                break;
                            }
                        }
                    }
                    if let Some((h_addr, cid, fld)) = found {
                        let nbr = unsafe { std::ptr::read((a + 16) as *const u64) };
                        return Some((h_addr, cid, fld, payload as usize, nbr));
                    }
                }
            }
            a += 8;
        }
        None
    }

    /// Check if the old generation is above 75% capacity.
    pub fn old_gen_needs_gc(&self) -> bool {
        let og = self.old_gen.lock();
        og.used() >= og.capacity() * 75 / 100
    }

    /// Run a minor garbage collection cycle.
    ///
    /// Copies live young-gen objects to young to-space (or promotes to old gen
    /// if they have survived enough cycles). Scans dirty card table entries
    /// for old→young references.
    ///
    /// Returns GC statistics and a pointer remapping table.
    /// Like [`collect_garbage`] but keeps dead finalizable objects alive so
    /// their `finalize()` method can be invoked.  Returns the GC result and
    /// the *new* (post-GC) addresses of dead finalizable objects.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage_with_finalizers(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        self.collect_garbage_inner(roots, finalizer_addrs, monitors)
    }

    /// Run a minor (or, if old gen is full, full) garbage-collection cycle.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        self.collect_garbage_inner(roots, &[], monitors).0
    }

    fn collect_garbage_inner(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        // Phase 6 #1: spin-yield until every live `SafepointToken` has
        // dropped. While a kernel is reading a JVM array on the GPU, we
        // must not move that array — a single token held anywhere on
        // any thread defers this collection until it is released.
        // No-op when the `gpu-offload` feature is off.
        crate::vm_heap::wait_for_gpu_critical_drain();

        // SAFETY: If any thread is currently inside a JIT call, we MUST NOT
        // run a *moving* collection. JIT frames hold raw object pointers in
        // their spill slots / registers which are NOT precisely described by
        // an oop map — the GC sees them only through the *conservative*
        // stack scan (`conservative_roots::scan_active_jit_frames`). A Cheney
        // copy would relocate those objects but it cannot safely rewrite a
        // conservatively-discovered slot: a stack word that merely *looks*
        // like a heap pointer might actually be an `i64`, and rewriting it
        // would corrupt the mutator's data.
        //
        // Previously the collector simply *skipped* the cycle entirely while
        // any JIT frame was live. Because a busy program always has a JIT
        // frame on the stack, the young gen would fill and the process would
        // OOM (the user-visible "young gen exhausted" abort).
        //
        // The fix: when JIT frames are active, run a **non-moving**
        // mark-sweep of the young generation instead of skipping. A
        // non-moving collection never relocates an object, so every JIT
        // spill slot stays valid no matter what it points at. Marking is
        // precise enough — it starts from the full root set (interpreter
        // frames, statics, JNI handles, *and* the conservative JIT-frame
        // roots that the caller already folded into `roots`) plus the
        // old→young dirty-card references — and dead young objects are
        // reclaimed into the from-space free list. Conservative false
        // positives only over-retain; they can never free a live object.
        //
        // This reclaims memory while JIT frames are live and eliminates the
        // OOM. A precise compacting collection still runs once every JIT
        // call has returned (quiescence ends), so fragmentation introduced
        // by the non-moving sweep is transient.
        // DBG: CRATONVM_DBG_FORCE_MOVING forces the moving (Cheney) collection
        // even when quiescence says JIT frames are active — to test whether the
        // non-moving sweep (wedged on by a leaked JitEntryGuard) is the
        // heap-corruption source. UNSAFE if a JIT frame is genuinely live
        // (relocates JIT-held raw pointers); diagnostic only.
        let force_moving = std::env::var_os("CRATONVM_DBG_FORCE_MOVING").is_some();
        // Fix A (2026-06-05): under live JIT frames the CORRECT young collector is
        // the NON-MOVING sweep + selective promotion — now DEFAULT-ON (see
        // `selective_on` in `sweep_young_non_moving`), giving bt18 = 68332206 =
        // HotSpot. It marks conservatively (over-marking is safe) and drains by
        // PINNING conservative roots + tenuring only heap-interior nodes, so no
        // live make/check node is ever lost. The moving Cheney UNDER-COUNTS bt18
        // (the long-mislabelled "golden" 67674804): a semispace cannot pin a
        // conservative JIT root nor rewrite a register-resident one, so some live
        // nodes go stale after the swap. `CRATONVM_SHADOW_STACK` tried to make
        // moving safe via precise rewritable roots but is incomplete (68199090)
        // AND its push/reload codegen is incompatible with the non-moving sweep,
        // so it still routes to the moving Cheney here — it is the INFERIOR path;
        // the DEFAULT (no gate) is now the correct one. `CRATONVM_DBG_FORCE_MOVING`
        // also forces the (under-counting) moving cycle for diagnostics.
        let shadow_roots = std::env::var_os("CRATONVM_SHADOW_STACK").is_some();
        if crate::gc_quiescence::is_active() && !force_moving && !shadow_roots {
            tracing::debug!(
                "JIT frames are active (depth={}) — running non-moving \
                 young-gen mark-sweep (compaction deferred until quiescence \
                 ends).",
                crate::gc_quiescence::depth(),
            );
            let result = self.sweep_young_non_moving(roots, finalizer_addrs);
            // BUG-V fix: the non-moving sweep still *relocates* objects via
            // selective promotion (young→old, see `selective_on` in
            // `sweep_young_non_moving`). Those relocations land in
            // `result.0.pointer_map`, and the moving path below remaps the
            // monitor registry with exactly that map at the
            // `monitors.remap_after_gc(&pointer_map)` call. This early return
            // used to skip it, so a `synchronized`-inflated object that got
            // selectively promoted kept its monitor-registry entry keyed to its
            // *old* young address while its copied mark word (at the new old-gen
            // address) still read INFLATED. The next `enter`/`lookup_inflated`
            // at the new address missed the registry → `inflate_locked`
            // returned the "mark inflated but registry entry missing" Err → the
            // `.expect(...)` panicked (monitor.rs registry/mark-word desync).
            // Re-key the registry here, identically to the moving path.
            monitors.remap_after_gc(&result.0.pointer_map);
            return result;
        }

        let mut young_from = self.young_from.lock();
        let mut young_to = self.young_to.lock();
        let mut old_gen = self.old_gen.lock();

        // Publish current (pre-collection) region bounds for the lock-free
        // `is_object_address` used during this cycle's marking/scanning. The
        // young arenas may swap/grow later in this function; the matching end
        // refresh republishes the post-collection bounds.
        self.store_region_bounds_locked(&young_from, &young_to, &old_gen);

        // bc math-ec 0x4 seed-phase bisect (CRATONVM_DBG_SEEDHUNT): count
        // `Object(Some(0<p<0x1000))` slots in old gen at GC ENTRY. Compared
        // against the post-Cheney and post-major counts below to localize the
        // collector path that SEEDS the `0x4`. See the helper docs above.
        let mut sh_printed: usize = 0;
        let sh_cap: usize = 40;
        let sh_entry_old: usize = if seedhunt_enabled() {
            let mut c = 0usize;
            for (op, _) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let h = unsafe { &*(op as *const ObjectHeader) };
                c += seedhunt_scan_obj(op, h, "entry", "O", &mut sh_printed, sh_cap);
            }
            c
        } else {
            0
        };

        // T5.5.2 (HIGH-1 fix): drain every mutator's thread-local card
        // buffer into the authoritative bitmap BEFORE the dirty-card
        // scan. We're inside the STW collector — the safepoint sync
        // barrier (which the caller holds before invoking
        // `collect_garbage`) guarantees every mutator either parked at
        // a safepoint (flushing its buffer on the way in) or completed
        // its barrier call before we got here. `flush_all` drains the
        // calling (collector) thread's buffer for completeness, then
        // `drain_pending` folds every queued offset — including those
        // submitted by mutators via auto-flush or safepoint-entry
        // flush — into the `cards`/`dirty_cards` bitmap.
        self.card_table.flush_all();
        self.card_table.drain_pending();
        let card_table: &CardTable = &self.card_table;

        // ---- DBG (bc math-ec): pre-GC remembered-set audit ----------------
        // For each old-gen object, check every old->young reference edge
        // against the card bitmap (post flush+drain). A CLEAN card on a live
        // old->young edge is a write-barrier / remembered-set MISS: the young
        // referent will be skipped by scan_dirty_cards and reclaimed though
        // still reachable — the FixedPointTest premature-reclamation corruption
        // (ECCurve$Fp/ECFieldElement$Fp fields decaying to ZEROED/OFF-HEAP).
        // Caught at GC entry, BEFORE reclamation, with the offending old obj.
        if std::env::var_os("CRATONVM_DBG_RSET_AUDIT").is_some() {
            let cbase = card_table.base_addr();
            let csize = crate::card_table::CARD_SIZE;
            let mut edges = 0usize;
            let mut misses = 0usize;
            let mut reported = 0usize;
            for (obj_ptr, _sz) in old_gen.walk_objects() {
                let hdr = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let addr = obj_ptr as usize;
                let card_idx = addr.wrapping_sub(cbase) / csize;
                let dirty = card_table.is_dirty(card_idx);
                if hdr.kind == ObjectKind::Array {
                    if hdr.element_type == ArrayElementType::Reference {
                        for i in 0..hdr.array_length as usize {
                            let s_ptr =
                                unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            let raw = unsafe { std::ptr::read(s_ptr as *const u64) };
                            if raw != 0 && raw < 0x1000 {
                                let cn = crate::gc::resolve_class_info(hdr.class_id.as_u32())
                                    .map(|(n, _)| n).unwrap_or_else(|| "<unresolved>".to_string());
                                eprintln!(
                                    "[small4] PRE-GC OLD {} @0x{:x} arr[{}] -> 0x{:x}",
                                    cn, addr, i, raw,
                                );
                            }
                            if raw != 0 && young_from.contains(raw as usize as *mut u8) {
                                edges += 1;
                                if !dirty {
                                    misses += 1;
                                    if reported < 60 {
                                        reported += 1;
                                        let cn = crate::gc::resolve_class_info(
                                            hdr.class_id.as_u32(),
                                        )
                                        .map(|(n, _)| n)
                                        .unwrap_or_else(|| "<unresolved>".to_string());
                                        eprintln!(
                                            "[rset-miss] OLD {} @0x{:x} arr[{}] -> young 0x{:x} CLEAN(card={})",
                                            cn, addr, i, raw, card_idx,
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for slot_idx in 0..hdr.num_slots as usize {
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                        if let Value::Object(Some(ro)) = value {
                            let p = ro.as_ptr() as usize;
                            if p != 0 && p < 0x1000 {
                                let cn = crate::gc::resolve_class_info(hdr.class_id.as_u32())
                                    .map(|(n, _)| n).unwrap_or_else(|| "<unresolved>".to_string());
                                eprintln!(
                                    "[small4] PRE-GC OLD {} @0x{:x} fld[{}] -> 0x{:x}",
                                    cn, addr, slot_idx, p,
                                );
                            }
                            if young_from.contains(ro.as_ptr()) {
                                edges += 1;
                                if !dirty {
                                    misses += 1;
                                    if reported < 60 {
                                        reported += 1;
                                        let cn = crate::gc::resolve_class_info(
                                            hdr.class_id.as_u32(),
                                        )
                                        .map(|(n, _)| n)
                                        .unwrap_or_else(|| "<unresolved>".to_string());
                                        eprintln!(
                                            "[rset-miss] OLD {} @0x{:x} fld[{}] -> young 0x{:x} CLEAN(card={})",
                                            cn, addr, slot_idx, ro.as_ptr() as usize, card_idx,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if edges > 0 {
                eprintln!(
                    "[rset-audit] old->young edges={} clean-card MISSES={}",
                    edges, misses,
                );
            }
            // Pre-GC linear scan of young_from for small-payload object refs
            // (mutator/native-written 0x4). If found here, the 0x4 PRE-EXISTS
            // the GC (it is NOT created by the collector). Linear walk may
            // truncate on a bad header (logged) — but the bump-allocated
            // young space is contiguous so it usually reaches everything.
            let ybase = young_from.base_ptr_mut() as usize;
            let yused = young_from.used();
            let mut ycur = 0usize;
            let mut found4 = 0usize;
            while ycur < yused {
                let h = unsafe { &*((ybase + ycur) as *const ObjectHeader) };
                let size = gen_object_total_size(h);
                if size == 0 || ycur + size > yused {
                    eprintln!("[small4] young walk truncated at off={} used={}", ycur, yused);
                    break;
                }
                let optr = (ybase + ycur) as *mut u8;
                if h.kind == ObjectKind::Object {
                    for si in 0..h.num_slots as usize {
                        let sp = unsafe { optr.add(HEADER_SIZE + si * SLOT_SIZE) };
                        let v = unsafe { std::ptr::read(sp as *const Value) };
                        if let Value::Object(Some(ro)) = v {
                            let p = ro.as_ptr() as usize;
                            if p != 0 && p < 0x1000 && found4 < 40 {
                                found4 += 1;
                                let cn = crate::gc::resolve_class_info(h.class_id.as_u32())
                                    .map(|(n, _)| n).unwrap_or_else(|| "<unresolved>".to_string());
                                eprintln!(
                                    "[small4] PRE-GC YOUNG {} @0x{:x} fld[{}] -> 0x{:x}",
                                    cn, ybase + ycur, si, p,
                                );
                                // bc math-ec 0x4 (2026-06-09): ONE-SHOT hex dump
                                // of the victim ±128 bytes. The surroundings
                                // answer "smear vs surgical": a run of math
                                // longs around the cell = OOB/stale smear; an
                                // otherwise-intact object with ONE flipped
                                // payload = a surgical single write. Words are
                                // u64 at 8-byte stride; the corrupt payload is
                                // marked `<<<<`.
                                static DUMPED: std::sync::atomic::AtomicBool =
                                    std::sync::atomic::AtomicBool::new(false);
                                if !DUMPED.swap(true, Ordering::Relaxed) {
                                    let victim = ybase + ycur;
                                    let cell_payload =
                                        victim + HEADER_SIZE + si * SLOT_SIZE + 8;
                                    let lo = victim.saturating_sub(128).max(ybase);
                                    let hi = (victim + size + 128).min(ybase + yused);
                                    eprintln!(
                                        "[small4] HEXDUMP victim=0x{victim:x} size={size} cell_payload=0x{cell_payload:x}:"
                                    );
                                    let mut a = lo & !7;
                                    while a < hi {
                                        let w = unsafe {
                                            std::ptr::read(a as *const u64)
                                        };
                                        eprintln!(
                                            "[small4]   0x{a:x}: 0x{w:016x}{}{}",
                                            if a == victim { "  <-- victim header" } else { "" },
                                            if a == cell_payload { "  <<<< corrupt payload" } else { "" },
                                        );
                                        a += 8;
                                    }
                                }
                            }
                        }
                    }
                } else if h.kind == ObjectKind::Array
                    && h.element_type == ArrayElementType::Reference
                {
                    for i in 0..h.array_length as usize {
                        let sp = unsafe { optr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw = unsafe { std::ptr::read(sp as *const u64) } as usize;
                        if raw != 0 && raw < 0x1000 && found4 < 40 {
                            found4 += 1;
                            let cn = crate::gc::resolve_class_info(h.class_id.as_u32())
                                .map(|(n, _)| n).unwrap_or_else(|| "<unresolved>".to_string());
                            eprintln!(
                                "[small4] PRE-GC YOUNG {} @0x{:x} arr[{}] -> 0x{:x}",
                                cn, ybase + ycur, i, raw,
                            );
                        }
                    }
                }
                ycur += size;
            }
        }
        // -------------------------------------------------------------------

        let bytes_before = young_from.used();
        let mut objects_copied: usize = 0;
        // Read & clear the promote-on-pressure flag set by the previous
        // minor GC. When true, every survivor of THIS cycle is promoted
        // to old gen regardless of age, breaking the long-lived-tree
        // semispace death spiral. Single AtomicBool::swap so the flag
        // doesn't latch across multiple consecutive cycles unless the
        // pressure persists.
        let force_promote_all = self
            .force_promote_all
            .swap(false, Ordering::Relaxed);
        // CRIT-P2 fix: use FxHashMap to avoid SipHash overhead on every
        // forwarded pointer (N hash ops per GC for N live objects).
        // Converted back to std HashMap at the end for public-API
        // compatibility (`GcResult.pointer_map` and `MonitorCleanup`).
        let mut pointer_map: FxHashMap<usize, usize> = FxHashMap::default();
        // CRIT-P2 fix: explicit worklist of promoted (old-gen) objects awaiting
        // a scan. Replaces the O(promoted^2) filter loop that previously
        // rebuilt `Vec<unscanned>` from `pointer_map.values()` per iteration.
        // Populated by `forward_object` whenever an object is promoted to
        // old gen; popped by the alternating Cheney scan below.
        let mut promoted_worklist: Vec<*mut u8> = Vec::new();

        // Collect additional roots from dirty cards in old gen
        let mut extra_roots: Vec<(ObjectRef, usize, usize)> = Vec::new();
        // (old_gen_obj, slot_index, _) for each old→young reference slot
        Self::scan_dirty_cards(&card_table, &old_gen, &young_from, &mut extra_roots);

        // Phase 1: Forward all root objects
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if !young_from.contains(old_ptr) {
                continue; // Skip roots not in young gen (e.g., old gen objects)
            }
            let new_ptr = Self::forward_object(
                &young_from,
                &mut young_to,
                &mut old_gen,
                old_ptr,
                &mut objects_copied,
                &mut pointer_map,
                &mut promoted_worklist,
                force_promote_all,
            );
            // SAFETY: `new_ptr` was returned by `forward_object`, which allocated
            // space in young_to or old_gen and copied a valid object there.
            *root = unsafe { ObjectRef::from_raw(new_ptr) };
        }

        // Old-gen addresses needing card re-mark after Phase 3's `clear_all()`
        // (applied via `mark_dirty_bulk`). Declared before Phase 1b so a
        // PERSISTENT old→young edge processed via a dirty card whose referent
        // STAYS young is re-remembered — otherwise the edge is remembered for
        // exactly one cycle (the promoting one) and the cycle after the dirty-
        // card fixup forgets it.
        let mut deferred_dirty_cards: Vec<usize> = Vec::new();

        // Phase 1b: Forward old→young references from dirty cards
        // Ref arrays use compact 8-byte pointers; object fields use 16-byte Value.
        for &(old_obj, slot_idx, _) in &extra_roots {
            // SAFETY: `old_obj` is a live old-gen ObjectRef collected from dirty card
            // scanning, so its pointer targets a valid ObjectHeader.
            let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
            let is_ref_array = header.kind == ObjectKind::Array
                && header.element_type == ArrayElementType::Reference;

            if is_ref_array {
                // SAFETY: `old_obj` is a valid old-gen object and `slot_idx` was collected
                // from dirty card scanning (within array bounds). Pointer arithmetic stays
                // within the object's allocation.
                let slot_ptr = unsafe {
                    old_obj
                        .as_ptr()
                        .add(HEADER_SIZE + slot_idx * REF_ELEMENT_SIZE)
                };
                // SAFETY: `slot_ptr` points to a valid 8-byte ref element within the array.
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    let ref_ptr = raw as usize as *mut u8;
                    if young_from.contains(ref_ptr) {
                        let new_ptr = Self::forward_object(
                            &young_from,
                            &mut young_to,
                            &mut old_gen,
                            ref_ptr,
                            &mut objects_copied,
                            &mut pointer_map,
                            &mut promoted_worklist,
                            force_promote_all,
                        );
                        // SAFETY: Writing the forwarded pointer back to the same valid slot.
                        unsafe { std::ptr::write(slot_ptr as *mut u64, new_ptr as u64) };
                        // Persistent old→young edge: re-remember if the referent
                        // stayed young (not promoted), so it survives clear_all().
                        if !old_gen.contains(new_ptr) {
                            deferred_dirty_cards.push(old_obj.as_ptr() as usize);
                        }
                    }
                }
            } else {
                // SAFETY: `old_obj` is a valid old-gen object, `slot_idx` is within
                // `num_slots` (from dirty card scanning). Arithmetic stays in bounds.
                let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: `slot_ptr` points to a valid `Value`-sized slot in the object.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if young_from.contains(ref_ptr) {
                        let new_ptr = Self::forward_object(
                            &young_from,
                            &mut young_to,
                            &mut old_gen,
                            ref_ptr,
                            &mut objects_copied,
                            &mut pointer_map,
                            &mut promoted_worklist,
                            force_promote_all,
                        );
                        // SAFETY: `new_ptr` is a valid forwarded allocation.
                        let new_value =
                            Value::Object(Some(unsafe { ObjectRef::from_raw(new_ptr) }));
                        // SAFETY: Writing updated Value back to the same valid slot.
                        unsafe { std::ptr::write(slot_ptr as *mut Value, new_value) };
                        // Persistent old→young edge: re-remember if the referent
                        // stayed young (see Phase 1b array branch).
                        if !old_gen.contains(new_ptr) {
                            deferred_dirty_cards.push(old_obj.as_ptr() as usize);
                        }
                    }
                }
            }
        }

        // Phase 2 + 2b: Combined Cheney scan and promoted object scan.
        //
        // We alternate between scanning young_to (standard Cheney) and
        // scanning newly promoted old-gen objects until both are fully
        // processed. This is necessary because:
        //   - Scanning a young_to object may forward a reference that gets
        //     promoted to old gen (needs promoted scan).
        //   - Scanning a promoted old-gen object may forward a reference
        //     that lands in young_to (needs Cheney scan) or gets promoted
        //     itself (needs another promoted scan iteration).
        let mut scan_cursor: usize = 0;
        // CRIT-P2 fix: FxHashSet (replaces std HashSet/SipHash) for cheap
        // dedup of promoted-object scans.
        let mut scanned_promoted: FxHashSet<usize> = FxHashSet::default();
        // `deferred_dirty_cards` declared above (before Phase 1b); both the
        // dirty-card fixup and the promoted-object scan accumulate into it.

        loop {
            let mut made_progress = false;

            // Cheney scan: process any unscanned objects in young_to
            while scan_cursor < young_to.used() {
                made_progress = true;
                // SAFETY: `scan_cursor` is within `young_to.used()` and advances by
                // `total_size` per object, so this points to a valid object header
                // in the young to-space arena.
                let obj_ptr = unsafe { young_to.base_ptr_mut().add(scan_cursor) };
                // SAFETY: `obj_ptr` points to a copied/promoted object with a valid header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let total_size = gen_object_total_size(header);

                // Scan ref slots: ref arrays use compact 8-byte pointers,
                // object fields use 16-byte Value.
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for i in 0..header.array_length as usize {
                            // SAFETY: `i` is within `array_length`, so the offset is
                            // within the array's data region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            // SAFETY: `s_ptr` points to a valid 8-byte ref element.
                            let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                            if raw != 0 {
                                let ref_ptr = raw as usize as *mut u8;
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from,
                                        &mut young_to,
                                        &mut old_gen,
                                        ref_ptr,
                                        &mut objects_copied,
                                        &mut pointer_map,
                                        &mut promoted_worklist,
                                        force_promote_all,
                                    );
                                    // SAFETY: Writing forwarded pointer back to the same valid slot.
                                    unsafe {
                                        std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for slot_idx in 0..header.num_slots as usize {
                        // SAFETY: `slot_idx` is within `num_slots`, so the offset is
                        // within the object's field region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        // SAFETY: `s_ptr` points to a valid `Value`-sized slot.
                        let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                        if let Value::Object(Some(ref_obj)) = value {
                            let ref_ptr = ref_obj.as_ptr();
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    force_promote_all,
                                );
                                // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                let new_value = Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(new_ref_ptr)
                                }));
                                // SAFETY: Writing updated Value back to the same valid slot.
                                unsafe { std::ptr::write(s_ptr as *mut Value, new_value) };
                            }
                        }
                    }
                }

                scan_cursor += total_size;
            }

            // Promoted object scan: process any unscanned promoted objects.
            //
            // CRIT-P2 fix: drain the explicit `promoted_worklist` instead of
            // rebuilding a Vec from `pointer_map.values()` on every outer
            // iteration (which was O(promoted^2) until fixpoint). Every
            // `forward_object` call that promotes an object to old gen
            // pushes its new address onto `promoted_worklist`, so popping
            // here is true Cheney-style O(promoted) scanning.
            //
            // `forward_object` only pushes on a fresh promotion (not on the
            // already-forwarded re-encounter path), so the worklist holds
            // each promoted address at most once. The `scanned_promoted`
            // check below is kept as a defensive idempotency guard.
            while let Some(obj_ptr) = promoted_worklist.pop() {
                if !scanned_promoted.insert(obj_ptr as usize) {
                    // already scanned this cycle
                    continue;
                }
                made_progress = true;
                // SAFETY: `obj_ptr` is a promoted object in old gen (it was
                // pushed only when `forward_object` confirmed the allocation
                // landed in `old_gen`), with a valid copied header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

                // Scan ref slots: ref arrays use compact 8-byte pointers,
                // object fields use 16-byte Value.
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for i in 0..header.array_length as usize {
                            // SAFETY: `i` < `array_length`; offset within array data region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            // SAFETY: `s_ptr` points to a valid 8-byte ref element.
                            let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                            if raw != 0 {
                                let ref_ptr = raw as usize as *mut u8;
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from,
                                        &mut young_to,
                                        &mut old_gen,
                                        ref_ptr,
                                        &mut objects_copied,
                                        &mut pointer_map,
                                        &mut promoted_worklist,
                                        force_promote_all,
                                    );
                                    // SAFETY: Writing forwarded pointer back to the same valid slot.
                                    unsafe {
                                        std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64);
                                    }
                                    // Mark card dirty if the forwarded ref landed in young
                                    // to-space — this old→young cross-gen reference must be
                                    // visible to the NEXT minor GC's dirty card scan.
                                    if !old_gen.contains(new_ref_ptr) {
                                        deferred_dirty_cards.push(obj_ptr as usize);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for slot_idx in 0..header.num_slots as usize {
                        // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                        if let Value::Object(Some(ref_obj)) = value {
                            let ref_ptr = ref_obj.as_ptr();
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    force_promote_all,
                                );
                                // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                let new_value = Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(new_ref_ptr)
                                }));
                                // SAFETY: Writing updated Value back to the same valid slot.
                                unsafe { std::ptr::write(s_ptr as *mut Value, new_value) };
                                // Mark card dirty if the forwarded ref landed in young
                                // to-space — this old→young cross-gen reference must be
                                // visible to the NEXT minor GC's dirty card scan.
                                //
                                // BUGFIX: use `deferred_dirty_cards` (re-marked AFTER
                                // Phase 3's `clear_all()` via `mark_dirty_bulk`), NOT a
                                // direct `card_table.mark_dirty` — the latter is wiped by
                                // `clear_all()` and the edge is forgotten next cycle. The
                                // array branch already does this; the object-field branch
                                // didn't, so a promoted old object holding a young object
                                // in a *field* (e.g. BouncyCastle X9ECParametersHolder
                                // .params -> young X9ECParameters) lost its remembered-set
                                // entry → the next minor GC relocated the referent without
                                // updating the field → stale all-zero-header receiver.
                                if !old_gen.contains(new_ref_ptr) {
                                    deferred_dirty_cards.push(obj_ptr as usize);
                                }
                            }
                        }
                    }
                }
            }

            if !made_progress {
                break;
            }
        }

        // Phase 2.5: Resurrect dead finalizable objects — forward any
        // unreachable finalizable objects so finalize() can access them.
        let mut dead_finalizers = Vec::new();
        for &old_addr in finalizer_addrs {
            if pointer_map.contains_key(&old_addr) {
                continue; // already reachable — skip
            }
            let old_ptr = old_addr as *mut u8;
            if !young_from.contains(old_ptr) {
                continue; // not in young gen (e.g. old gen or invalid)
            }
            let new_ptr = Self::forward_object(
                &young_from,
                &mut young_to,
                &mut old_gen,
                old_ptr,
                &mut objects_copied,
                &mut pointer_map,
                &mut promoted_worklist,
                force_promote_all,
            );
            dead_finalizers.push(new_ptr as usize);
        }

        // Phase 2.5b: Continue Cheney scan for resurrected objects and their refs
        if !dead_finalizers.is_empty() {
            loop {
                let mut made_progress = false;
                while scan_cursor < young_to.used() {
                    made_progress = true;
                    // SAFETY: `scan_cursor` is within `young_to.used()`; points to a valid copied object header.
                    let obj_ptr = unsafe { young_to.base_ptr_mut().add(scan_cursor) };
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    let total_size = gen_object_total_size(header);
                    if header.kind == ObjectKind::Array {
                        if header.element_type == ArrayElementType::Reference {
                            for i in 0..header.array_length as usize {
                                // SAFETY: `i` < `array_length`; offset within array data region.
                                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                                let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                                if raw != 0 {
                                    let ref_ptr = raw as usize as *mut u8;
                                    if young_from.contains(ref_ptr) {
                                        let new_ref_ptr = Self::forward_object(
                                            &young_from, &mut young_to, &mut old_gen,
                                            ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                            force_promote_all,
                                        );
                                        // SAFETY: Writing forwarded pointer back to the same valid ref-array slot.
                                        unsafe { std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64); }
                                    }
                                }
                            }
                        }
                    } else {
                        for slot_idx in 0..header.num_slots as usize {
                            // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                            let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                            if let Value::Object(Some(ref_obj)) = value {
                                let ref_ptr = ref_obj.as_ptr();
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from, &mut young_to, &mut old_gen,
                                        ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                        force_promote_all,
                                    );
                                    // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                    let new_value = Value::Object(Some(unsafe {
                                        ObjectRef::from_raw(new_ref_ptr)
                                    }));
                                    // SAFETY: Writing updated Value back to the same valid slot.
                                    unsafe { std::ptr::write(s_ptr as *mut Value, new_value); }
                                }
                            }
                        }
                    }
                    scan_cursor += total_size;
                }
                // Also scan promoted objects from resurrection — drain the
                // shared worklist (see CRIT-P2 note above on the main scan).
                while let Some(obj_ptr) = promoted_worklist.pop() {
                    if !scanned_promoted.insert(obj_ptr as usize) {
                        continue;
                    }
                    made_progress = true;
                    // SAFETY: `obj_ptr` is a promoted old-gen object with a valid copied header.
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    if header.kind == ObjectKind::Array {
                        if header.element_type == ArrayElementType::Reference {
                            for i in 0..header.array_length as usize {
                                // SAFETY: `i` < `array_length`; offset within array data region.
                                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                                let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                                if raw != 0 {
                                    let ref_ptr = raw as usize as *mut u8;
                                    if young_from.contains(ref_ptr) {
                                        let new_ref_ptr = Self::forward_object(
                                            &young_from, &mut young_to, &mut old_gen,
                                            ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                            force_promote_all,
                                        );
                                        // SAFETY: Writing forwarded pointer back to the same valid ref-array slot.
                                        unsafe { std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64); }
                                        // BUGFIX (same class as the object-field fix): use
                                        // `deferred_dirty_cards` (re-applied AFTER clear_all
                                        // via mark_dirty_bulk), NOT a direct
                                        // `card_table.mark_dirty` which clear_all() wipes.
                                        // An old reference ARRAY holding a young object (e.g.
                                        // ArrayList.elementData with the ServiceLoader provider
                                        // instances) promoted via this resurrection drain
                                        // otherwise loses its remembered-set entry → the next
                                        // minor GC relocates/collects the young element →
                                        // "Not able to load any cryptoProvider".
                                        if !old_gen.contains(new_ref_ptr) {
                                            deferred_dirty_cards.push(obj_ptr as usize);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        for slot_idx in 0..header.num_slots as usize {
                            // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                            let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                            if let Value::Object(Some(ref_obj)) = value {
                                let ref_ptr = ref_obj.as_ptr();
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from, &mut young_to, &mut old_gen,
                                        ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                        force_promote_all,
                                    );
                                    // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                    let new_value = Value::Object(Some(unsafe {
                                        ObjectRef::from_raw(new_ref_ptr)
                                    }));
                                    // SAFETY: Writing updated Value back to the same valid slot.
                                    unsafe { std::ptr::write(s_ptr as *mut Value, new_value); }
                                    if !old_gen.contains(new_ref_ptr) {
                                        deferred_dirty_cards.push(obj_ptr as usize);
                                    }
                                }
                            }
                        }
                    }
                }
                if !made_progress {
                    break;
                }
            }
        }

        let bytes_copied = young_to.used();

        // bc math-ec 0x4 seed-phase bisect: count `0x4` slots in the Cheney
        // to-space survivors and in old gen AFTER the minor (Cheney +
        // promotion) collection, BEFORE the possible major GC. A jump above
        // `sh_entry_old` here means the MINOR collector seeded the `0x4`.
        let (sh_cheney_young, sh_cheney_old): (usize, usize) = if seedhunt_enabled() {
            let cy = seedhunt_scan_young(
                young_to.base_ptr(),
                young_to.used(),
                "cheney",
                &mut sh_printed,
                sh_cap,
            );
            let mut co = 0usize;
            for (op, _) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let h = unsafe { &*(op as *const ObjectHeader) };
                co += seedhunt_scan_obj(op, h, "cheney", "O", &mut sh_printed, sh_cap);
            }
            (cy, co)
        } else {
            (0, 0)
        };

        // Phase H (RH.1): compute promotion / young-copy stats from the
        // pointer_map BEFORE major_gc appends its own entries below.
        // Every entry at this point is a minor-GC forward: new_addr is
        // either in old_gen (promoted) or in young_to-space (copied).
        // Walking the map once avoids an expensive counter plumbed
        // through `forward_object`'s 12 call sites.
        let mut bytes_promoted_cycle: u64 = 0;
        let mut objects_promoted_cycle: u64 = 0;
        let mut bytes_copied_young_cycle: u64 = 0;
        let mut objects_copied_young_cycle: u64 = 0;
        let mut bad_forward_count: u64 = 0;
        let mut bad_forward_sample: (usize, usize) = (0, 0);
        for (&old_addr, &new_addr) in pointer_map.iter() {
            let new_ptr = new_addr as *const u8;
            let in_old = old_gen.contains(new_ptr);
            let in_young = young_to.contains(new_ptr);
            // BUG-Z safety: every `pointer_map` value must be a forwarding
            // address inside young_to or old_gen. Under heavy multi-threaded
            // churn (TestFileStoreConcurrency) `forward_object` has been observed
            // to record a *garbage* value (e.g. a small integer) for a valid
            // young_from source — a heap-corruption bug tracked in BUG-Z.
            // Dereferencing such a value here SIGSEGVs in the GC's post-copy
            // stats walk. Skip it (the stats are advisory counters) and record a
            // sample so a single summary can be logged, rather than crashing or
            // spamming a line per entry. NOTE: this does not repair the bad
            // forward — `update_all_roots` still remaps through `pointer_map`;
            // see BUG-Z for the underlying fix.
            if !in_old && !in_young {
                bad_forward_count += 1;
                if bad_forward_sample == (0, 0) {
                    bad_forward_sample = (old_addr, new_addr);
                }
                continue;
            }
            // SAFETY: `new_addr` is confirmed inside young_to or old_gen; both
            // allocations begin with a valid ObjectHeader.
            let header = unsafe { &*(new_addr as *const ObjectHeader) };
            // `gen_object_total_size` is the sum of HEADER_SIZE and the
            // variable-length object body, computed from the header
            // exactly as the copy path does.
            let sz = gen_object_total_size(header) as u64;
            if in_old {
                bytes_promoted_cycle += sz;
                objects_promoted_cycle += 1;
            } else {
                bytes_copied_young_cycle += sz;
                objects_copied_young_cycle += 1;
            }
        }
        if bad_forward_count > 0 {
            // BUG-Z: surface the corruption once per cycle (not per entry).
            eprintln!(
                "[gc] WARNING: {} pointer_map forward(s) pointed outside the heap \
                 (corruption — see BUG-Z); skipped in stats. sample old=0x{:x} -> new=0x{:x}",
                bad_forward_count, bad_forward_sample.0, bad_forward_sample.1,
            );
        }

        // Phase 3: Clear card table and reset young from-space
        card_table.clear_all();
        // Re-mark cards for promoted objects that still reference young gen.
        // These old→young cross-gen references were established during the
        // Phase 2 promoted-object scan and must be visible to the next GC.
        //
        // The actual `mark_dirty_bulk` is DEFERRED until after the possible
        // major GC below: a major GC mark-compacts the old gen, relocating the
        // very referrer objects whose cards we are about to dirty. Marking here
        // (pre-compaction) would leave the remembered-set cards pointing at the
        // stale pre-compaction addresses; the next minor GC's dirty-card scan
        // would then look in the wrong place, miss the old→young edge, and free
        // a still-live young referent (intermittent stale Locale/ClassLoader
        // corruption). We remap each referrer address through `compact_map`
        // first when a major GC runs (see below).
        young_from.reset();

        // CRIT-P2 fix: convert the internal FxHashMap to the std HashMap
        // expected by `MonitorCleanup::remap_after_gc` (defined in
        // `collector.rs`) and `GcResult.pointer_map` (the public-API field
        // in `gc.rs`). The conversion is a single O(N) walk — cheap
        // compared to N SipHash operations across the Cheney scan.
        let mut pointer_map: HashMap<usize, usize> = pointer_map.into_iter().collect();

        // Phase 4: Swap young spaces (monitor remap deferred until after a
        // possible major GC so we can pass the composed pointer_map).
        std::mem::swap(&mut *young_from, &mut *young_to);

        // Phase 5: Check if old gen is getting full — trigger major GC (mark-compact)
        let major_ran = if old_gen.used() >= old_gen.capacity() * 75 / 100 {
            tracing::debug!(
                "Old gen at {}% — running major GC (mark-compact)",
                old_gen.used() * 100 / old_gen.capacity(),
            );
            let old_used_before = old_gen.used();
            let compact_map = Self::major_gc(roots, &young_from, &mut old_gen);
            // CRITICAL FIX (heavy binary-trees GC corruption):
            //
            // Compose `pointer_map` with `compact_map` BEFORE merging. If a
            // minor-GC entry says `young_addr → promoted_addr` AND the major
            // GC compacted `promoted_addr → compacted_addr`, a naive merge
            // leaves the minor entry pointing at the now-stale `promoted_addr`.
            // `update_all_roots` does a single-step lookup per slot — so a
            // frame local that originally held `young_addr` would be rewritten
            // to `promoted_addr`, dereferencing freed/overwritten memory on
            // the next field read (the visible symptom: Node objects come back
            // as `java/lang/Object class_id=0 num_slots=0`).
            //
            // Walk all existing entries and chain any value that appears as a
            // compact_map key through to its final destination, THEN merge the
            // raw compact_map so external roots (statics, JNI, etc.) that
            // pointed directly at an uncompacted old-gen object also get the
            // correct post-compaction target.
            for new_addr in pointer_map.values_mut() {
                if let Some(&final_addr) = compact_map.get(new_addr) {
                    *new_addr = final_addr;
                }
            }
            // Remembered-set fixup across compaction: the deferred old→young
            // referrer addresses were recorded pre-compaction. Remap each
            // through `compact_map` to its post-compaction location before
            // dirtying its card, so the next minor GC's dirty-card scan finds
            // the relocated referrer (and therefore its old→young edge).
            // Referrers that did not move are absent from `compact_map` and
            // keep their original address.
            let remapped_cards: Vec<usize> = deferred_dirty_cards
                .iter()
                .map(|addr| *compact_map.get(addr).unwrap_or(addr))
                .collect();
            card_table.mark_dirty_bulk(&remapped_cards);
            // Merge old-gen compaction relocations into the overall pointer map
            // so the VM can update external roots (statics, JNI, string pool, etc.)
            pointer_map.extend(compact_map);
            let old_used_after = old_gen.used();
            // `used` can rise after compaction if the compactor's metadata
            // overhead exceeds reclaimed garbage; clamp with saturating_sub.
            self.stats
                .bytes_freed_old
                .fetch_add(old_used_before.saturating_sub(old_used_after) as u64, Ordering::Relaxed);
            true
        } else {
            // No major GC: old-gen referrers did not move, so dirty their
            // cards at the addresses recorded during this cycle.
            card_table.mark_dirty_bulk(&deferred_dirty_cards);
            false
        };

        // bc math-ec 0x4 seed-phase bisect: count `0x4` slots in the post-swap
        // young from-space (the just-collected survivors) and in old gen AFTER
        // the possible major GC. A jump in the OLD count from `sh_cheney_old`
        // to here means the MAJOR mark-compact (update_refs_in_object /
        // fixup_young_old_refs) seeded the `0x4` — see handoff §6.1.
        if seedhunt_enabled() {
            let py = seedhunt_scan_young(
                young_from.base_ptr(),
                young_from.used(),
                "post",
                &mut sh_printed,
                sh_cap,
            );
            let mut po = 0usize;
            for (op, _) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let h = unsafe { &*(op as *const ObjectHeader) };
                po += seedhunt_scan_obj(op, h, "post", "O", &mut sh_printed, sh_cap);
            }
            if sh_entry_old | sh_cheney_young | sh_cheney_old | py | po != 0 {
                eprintln!(
                    "[seedhunt] GC entry_old={} | post-cheney young={} old={} | major_ran={} | \
                     post-major young={} old={}",
                    sh_entry_old, sh_cheney_young, sh_cheney_old, major_ran, py, po,
                );
            }
        }

        // Phase 4b (moved): remap monitors with the FINAL composed pointer_map.
        // Doing this after a possible major GC ensures monitor keys for
        // promoted-then-compacted objects are remapped to their final
        // post-compaction addresses, not the intermediate post-promotion ones.
        monitors.remap_after_gc(&pointer_map);

        let bytes_freed = bytes_before.saturating_sub(bytes_copied);

        // Phase 6: Adaptive heap expansion.
        // If GC didn't reclaim enough space (less than 25% of capacity freed),
        // expand the young gen to avoid GC thrashing.
        //
        // After swap: young_from has live data, young_to is empty (was reset).
        // We can only safely grow young_to (empty arena). This means the
        // expansion takes effect on the NEXT GC cycle — when live objects are
        // copied into the now-larger to-space, which then becomes from-space.
        let freed_percent = if bytes_before > 0 {
            bytes_freed * 100 / bytes_before
        } else {
            100
        };
        // Promote-on-pressure: if survival was very high this cycle,
        // arm the flag so the NEXT minor GC promotes ALL survivors to
        // old gen regardless of age. Otherwise a long-lived working set
        // gets copied semi→semi forever (binary-trees death spiral).
        //
        // Only arm when the *from* itself is at high occupancy. The
        // freed_percent metric measures young_to.used vs young_from.used,
        // but a low percentage when bytes_before is tiny (e.g. a partial-
        // allocation cycle) doesn't indicate real pressure. Gating on
        // bytes_before >= 1/2 of the from capacity rules out those
        // false-positives. force_promote_all was already consumed (and
        // cleared) at the top of this function via `swap`; setting it
        // here arms the *next* cycle.
        // `young_to` post-swap is the arena we just *collected* — its
        // capacity is what `bytes_before` was measured against.
        let from_cap_before = young_to.capacity();
        let high_survival = freed_percent < GC_PROMOTE_PRESSURE_PERCENT
            && bytes_before >= from_cap_before / 2;
        if high_survival {
            tracing::debug!(
                "GC: high survival ({}% freed of {} bytes) — \
                 arming promote-on-pressure for next minor GC",
                freed_percent,
                bytes_before,
            );
            self.force_promote_all.store(true, Ordering::Relaxed);
        }

        if freed_percent < GC_EXPANSION_THRESHOLD_PERCENT {
            let current_cap = young_to.capacity();
            let new_cap = (current_cap * 2).min(self.max_young_semi_size);
            if new_cap > current_cap {
                tracing::debug!(
                    "GC: low reclamation ({}% freed) — expanding young to-space {} → {} bytes",
                    freed_percent,
                    current_cap,
                    new_cap,
                );
                // young_to is empty after reset+swap, safe to grow
                young_to.grow(new_cap);
                // Update threshold based on the upcoming larger from-space
                *self.young_gc_threshold.lock() = new_cap * YOUNG_GC_THRESHOLD_PERCENT / 100;
            }
        }

        // Republish region bounds: the arenas were swapped (and to-space may
        // have been grown to a new backing) above, so the lock-free
        // `is_object_address` cache must reflect the post-collection
        // `[base, end)` before any mutator resumes. Guards still held.
        self.store_region_bounds_locked(&young_from, &young_to, &old_gen);

        // Phase H (RH.1): commit per-cycle counters to the lifetime
        // accumulator. Do this at the end so tests can observe GC
        // statistics after the call returns.
        self.stats.minor_gc_count.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_promoted
            .fetch_add(bytes_promoted_cycle, Ordering::Relaxed);
        self.stats
            .objects_promoted
            .fetch_add(objects_promoted_cycle, Ordering::Relaxed);
        self.stats
            .bytes_copied_young
            .fetch_add(bytes_copied_young_cycle, Ordering::Relaxed);
        self.stats
            .objects_copied_young
            .fetch_add(objects_copied_young_cycle, Ordering::Relaxed);
        if major_ran {
            self.stats.major_gc_count.fetch_add(1, Ordering::Relaxed);
        }

        (
            GcResult {
                stats: crate::gc::GcStats {
                    objects_copied,
                    bytes_copied,
                    bytes_freed,
                },
                pointer_map,
            },
            dead_finalizers,
        )
    }

    /// Non-moving young-generation mark-sweep, used when JIT frames are
    /// active and a moving (Cheney) collection would be unsafe.
    ///
    /// Unlike [`Self::collect_garbage_inner`]'s copying collector, this
    /// **never relocates an object**. Survivors keep their exact
    /// addresses, so every raw pointer held in a JIT spill slot or
    /// register stays valid regardless of whether the GC could describe
    /// it precisely. Dead young objects are reclaimed into the
    /// from-space arena's free list (see [`Arena::add_free_block`]).
    ///
    /// ## Correctness
    ///
    /// Marking must be **complete** — a non-moving sweep frees every
    /// young object that is not marked, so a missed root would free a
    /// live object. The root set is:
    ///
    ///   * `roots` — every precise VM root the caller gathered
    ///     (interpreter frame locals/stacks, statics, JNI handles, class
    ///     locks, autobox caches, …) **plus** the conservative
    ///     JIT-frame roots that `collect_roots` already appended via
    ///     `conservative_roots::scan_active_jit_frames`.
    ///   * old→young references discovered by scanning dirty cards.
    ///   * `finalizer_addrs` — finalizable objects kept alive so their
    ///     `finalize()` can run.
    ///
    /// A conservative false-positive root only over-retains an object;
    /// it can never cause a live object to be freed. Because nothing
    /// moves, a stack word that merely *looks* like a pointer is never
    /// rewritten, so the mutator's non-pointer data is never corrupted.
    ///
    /// Returns an empty pointer map (no object moved) and the list of
    /// resurrected dead-finalizer addresses (unchanged, since they too
    /// stay in place).
    fn sweep_young_non_moving(
        &self,
        roots: &[ObjectRef],
        finalizer_addrs: &[usize],
    ) -> (GcResult, Vec<usize>) {
        let mut young_from = self.young_from.lock();
        let mut old_gen = self.old_gen.lock();

        // Fold every mutator's thread-local card buffer into the bitmap
        // before scanning dirty cards (same protocol as the moving path).
        self.card_table.flush_all();
        self.card_table.drain_pending();

        let bytes_before = young_from.used();
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.used();

        // Helper: is `addr` the start of a young from-space object?
        let in_young = |addr: usize| -> bool {
            addr >= from_base && addr < from_end && (addr & 0x7) == 0
        };

        // ----- Mark phase -------------------------------------------------
        //
        // BFS over young-gen objects. The worklist holds young object
        // pointers that have been marked but not yet scanned. Marking an
        // old-gen object is unnecessary for a young collection, but we
        // still traverse *through* an old-gen object if a dirty card says
        // it may reference young gen (handled below via the dirty-card
        // seed). Marking uses `GC_FLAG_MARKED` in the object header.
        let mut worklist: Vec<*mut u8> = Vec::new();

        // mark_if_young: mark a candidate young pointer and enqueue it.
        // SAFETY contract: `ptr` is only dereferenced after `in_young`
        // confirms it lands inside the live from-space region.
        let mut mark_young = |ptr: *mut u8, worklist: &mut Vec<*mut u8>| {
            let addr = ptr as usize;
            if !in_young(addr) {
                return;
            }
            // SAFETY: `in_young` confirmed `addr` is an 8-byte-aligned
            // address inside the live from-space region, so reading an
            // ObjectHeader there is valid.
            let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
            // Reject implausible headers — a conservative root may point
            // at a non-object word. `is_object_address`-style sanity.
            // Multi-array reloc fix (2026-05-22): `num_slots` mirrors
            // `array_length` for arrays, so a legitimate 256 MB int[] has
            // num_slots = 2^26 > 1<<24 and would be skipped (then swept as
            // garbage, despite being a live root). Gate num_slots on
            // non-arrays; bound array_length at the JVM ceiling.
            let kind_byte = header.kind as u8;
            let is_array = header.kind == ObjectKind::Array;
            if kind_byte > 1
                || (!is_array && header.num_slots > (1 << 24))
                || (is_array && header.array_length > i32::MAX as u32)
            {
                return;
            }
            if header.gc_flags & GC_FLAG_MARKED == 0 {
                header.gc_flags |= GC_FLAG_MARKED;
                worklist.push(ptr);
            }
        };

        // Seed: precise + conservative roots gathered by the caller.
        for root in roots.iter() {
            mark_young(root.as_ptr(), &mut worklist);
        }

        // Seed: finalizable objects — keep them alive so finalize() runs.
        for &addr in finalizer_addrs {
            mark_young(addr as *mut u8, &mut worklist);
        }

        // Seed: old→young references from dirty cards. Reuse the existing
        // dirty-card scanner, which yields (old_obj, slot_idx, _) tuples;
        // we read the referenced young object out of each slot.
        let mut extra_roots: Vec<(ObjectRef, usize, usize)> = Vec::new();
        Self::scan_dirty_cards(&self.card_table, &old_gen, &young_from, &mut extra_roots);
        // `scan_dirty_cards` *consumes* the dirty-card tracking list. Since
        // this collection does not move young objects, every old→young
        // reference it found is still valid and the card covering it must
        // stay dirty for the NEXT collection. Re-dirty each old object's
        // card after we have read its slots below.
        let mut redirty_cards: Vec<usize> = Vec::with_capacity(extra_roots.len());
        for &(old_obj, slot_idx, _) in &extra_roots {
            // Card covering this old object must remain dirty for the
            // next GC (the old→young edge survives an in-place sweep).
            redirty_cards.push(old_obj.as_ptr() as usize);
            // SAFETY: `old_obj` is a live old-gen object from dirty-card
            // scanning; its header is valid.
            let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
            if header.kind == ObjectKind::Array
                && header.element_type == ArrayElementType::Reference
            {
                // SAFETY: `slot_idx` is within array bounds (from card scan).
                let slot_ptr = unsafe {
                    old_obj
                        .as_ptr()
                        .add(HEADER_SIZE + slot_idx * REF_ELEMENT_SIZE)
                };
                // SAFETY: `slot_ptr` is a valid 8-byte ref element.
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    mark_young(raw as usize as *mut u8, &mut worklist);
                }
            } else {
                // SAFETY: `slot_idx` is within `num_slots` (from card scan).
                let slot_ptr =
                    unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: `slot_ptr` is a valid Value-sized slot.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    mark_young(ref_obj.as_ptr(), &mut worklist);
                }
            }
        }

        // Restore the dirty cards consumed by `scan_dirty_cards` so the
        // next collection still sees these old→young references.
        self.card_table.mark_dirty_bulk(&redirty_cards);

        // BFS: transitively mark every young object reachable from a root.
        while let Some(obj_ptr) = worklist.pop() {
            // SAFETY: `obj_ptr` was validated by `mark_young` before being
            // pushed — it is a sane young-gen object header.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data.
                        let s_ptr =
                            unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        // SAFETY: `s_ptr` is a valid 8-byte ref element.
                        let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                        if raw != 0 {
                            mark_young(raw as usize as *mut u8, &mut worklist);
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` < `num_slots`; offset within field region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    // SAFETY: `s_ptr` is a valid Value-sized slot.
                    let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        mark_young(ref_obj.as_ptr(), &mut worklist);
                    }
                }
            }
        }

        // ----- Selective promotion -----------------------------------------
        //
        // ✅ CORRECT (Fix A, 2026-06-05). Enabled under `CRATONVM_SELECTIVE_PROMOTE`
        // OR `CRATONVM_SHADOW_STACK` (see `selective_on` below); opt out with
        // `CRATONVM_NO_SELECTIVE_PROMOTE`. It hits the HotSpot-verified checksums on
        // bintrees10/14/16/18 — including **bt18 = 68332206**, which is the TRUE
        // value (`java -cp bench BenchSuite bintrees18`). The old "golden 67674804"
        // was a CratonVM UNDER-COUNT bug, so the earlier "EXPERIMENTAL/KNOWN-BUGGY —
        // 68332206 is WRONG" verdict here was INVERTED: this path is the *correct*
        // one and the moving Cheney (67674804) is the buggy one. `SP_VERIFY` already
        // showed MISSED=0 + ALIASING=0 — the fixup was always correct; only the
        // reference value was wrong. It also eliminates young-gen exhaustion (the
        // non-moving sweep alone walls on bt18's long-lived set).
        //
        // The non-moving sweep keeps every survivor in young in place, so a
        // workload whose live young set approaches young capacity (bintrees18's
        // long-lived tree) cannot drain young — the sweep reclaims ~nothing and
        // the VM thrashes or exhausts young. The moving collector avoids this by
        // tenuring survivors to old gen, but it cannot run while conservative
        // JIT roots are live (it would relocate a JIT-held pointer it cannot
        // safely rewrite).
        //
        // Selective promotion threads the needle: evacuate to OLD GEN exactly
        // those marked survivors NOT pinned by any root value. PIN = every young
        // address that appears as a root or finalizer value — which INCLUDES
        // conservative JIT false-positives, because we pin by the raw slot
        // value. An evacuated object's address can therefore never equal any
        // conservative slot value, so neither this fixup nor the VM-level
        // pointer_map remap (which conservatively rescans JIT slots) can rewrite
        // a non-pointer slot. Objects reachable only via precise heap edges (the
        // tree's interior nodes) are movable and get tenured, draining young
        // while the few pinned objects stay put.
        let mut evac_map: HashMap<usize, usize> = HashMap::new();
        // Fix A: selective promotion is the proven-correct drain for the
        // JIT-active non-moving sweep (bt18 = 68332206 = HotSpot), so it is now
        // DEFAULT-ON. Opt out with `CRATONVM_NO_SELECTIVE_PROMOTE` to get the pure
        // (walling, under-counting) non-moving sweep for debugging. The legacy
        // `CRATONVM_SELECTIVE_PROMOTE` gate is now redundant (always-on) but kept
        // accepted for compatibility. This path runs only here, in the JIT-active
        // non-moving sweep, so it never affects the no-JIT moving Cheney.
        let selective_on = std::env::var_os("CRATONVM_NO_SELECTIVE_PROMOTE").is_none();
        if selective_on {
            let is_y = |a: usize| -> bool { a >= from_base && a < from_end && (a & 0x7) == 0 };

            // (1) Pin set: every root / finalizer value that lands in young.
            let mut pinned: FxHashSet<usize> = FxHashSet::default();
            for r in roots.iter() {
                let a = r.as_ptr() as usize;
                if is_y(a) {
                    pinned.insert(a);
                }
            }
            for &a in finalizer_addrs.iter() {
                if is_y(a) {
                    pinned.insert(a);
                }
            }

            // (2) Evacuate non-pinned marked survivors to old gen. Install a
            // forwarding pointer in each young source; record young→old in
            // `evac_map` (returned for the VM-level remap). Stop if old gen
            // fills (leave the remainder in young — correctness over completeness).
            let mut evacuated: Vec<*mut u8> = Vec::new();
            {
                let free_blocks = young_from.free_blocks_sorted();
                let mut free_iter = free_blocks.iter().peekable();
                let used = young_from.used();
                let mut cursor = 0usize;
                let mut old_full = false;
                while cursor < used && !old_full {
                    if let Some(&&(off, sz)) = free_iter.peek() {
                        if cursor == off {
                            cursor += sz;
                            free_iter.next();
                            continue;
                        }
                    }
                    let src = (from_base + cursor) as *mut u8;
                    // SAFETY: `cursor` within `used`; from-space is mapped.
                    let header = unsafe { &*(src as *const ObjectHeader) };
                    // Bug-D fix (2026-06-12): skip a GAP-filler sentinel (a
                    // sub-`HEADER_SIZE` TLAB tail) before `gen_object_total_size`.
                    // This selective-promotion walk runs before the main sweep
                    // reclaims gaps, so sentinels are still in place here.
                    if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                        // SAFETY: offset 4 lies within the >=8-byte gap.
                        let gap = unsafe {
                            std::ptr::read((src as *const u8).add(4) as *const u32)
                        } as usize;
                        if gap >= 8 && gap < HEADER_SIZE && cursor + gap <= used {
                            cursor += gap;
                            continue;
                        }
                    }
                    let total_size = gen_object_total_size(header);
                    if total_size < HEADER_SIZE || cursor + total_size > used {
                        break;
                    }
                    let addr = src as usize;
                    // Only tenure objects that have survived enough GCs
                    // (gc_age+1 >= PROMOTION_AGE), exactly like the moving
                    // collector. Promoting on the first survival would flood old
                    // gen with objects that die almost immediately (bintrees'
                    // short-lived trees), defeating the drain and starving old
                    // gen. Short-lived survivors stay in young and are reclaimed
                    // normally; only the genuinely long-lived set (the
                    // persistent tree) ages out and promotes.
                    let aged = header.gc_age + 1 >= PROMOTION_AGE;
                    if header.gc_flags & GC_FLAG_MARKED != 0 && aged && !pinned.contains(&addr) {
                        match old_gen.alloc(total_size, 8) {
                            Some(dst) => {
                                // SAFETY: src/dst are valid, non-overlapping, total_size bytes.
                                unsafe { std::ptr::copy_nonoverlapping(src, dst, total_size) };
                                // Replicate the atomic mark_word through atomic ops
                                // (the bulk copy of an AtomicU64 is otherwise UB).
                                // SAFETY: both headers are fully written.
                                unsafe {
                                    let m = (*(src as *const ObjectHeader))
                                        .mark_word
                                        .load(Ordering::Relaxed);
                                    (*(dst as *mut ObjectHeader))
                                        .mark_word
                                        .store(m, Ordering::Relaxed);
                                }
                                // SAFETY: dst is a freshly written object header.
                                let dhdr = unsafe { &mut *(dst as *mut ObjectHeader) };
                                dhdr.forwarding_ptr = std::ptr::null_mut();
                                dhdr.gc_flags |= GC_FLAG_OLD_GEN;
                                dhdr.gc_flags &= !GC_FLAG_MARKED;
                                // Install forwarding pointer in the young source.
                                // SAFETY: src is a live young object header.
                                unsafe {
                                    std::ptr::addr_of_mut!(
                                        (*(src as *mut ObjectHeader)).forwarding_ptr
                                    )
                                    .write(dst);
                                }
                                evac_map.insert(addr, dst as usize);
                                evacuated.push(dst);
                            }
                            None => old_full = true,
                        }
                    }
                    cursor += total_size;
                }
            }

            // DBG (CRATONVM_SP_STATS): per-GC pin/evac counts, printed
            // unconditionally (even when nothing evacuated) so we can confirm
            // whether selective promotion is actually evacuating at a given
            // heap size, or whether the only active effect is the free-block
            // coalescing below.
            if std::env::var_os("CRATONVM_SP_STATS").is_some() {
                eprintln!(
                    "[sp-stats] gc: pinned={} evac={}",
                    pinned.len(),
                    evac_map.len(),
                );
            }

            // DBG (CRATONVM_SP_TRACE): directly test the wrong-address/aliasing
            // hypothesis for the bintrees18 bug. Two young objects copied to
            // OVERLAPPING old-gen destinations (an old_gen.alloc collision) would
            // corrupt one copy's field data → a child reference reads the wrong
            // subtree → inflated check() count. Detect overlapping dst ranges and
            // duplicate dst values among this GC's evacuations.
            if std::env::var_os("CRATONVM_SP_TRACE").is_some() && !evacuated.is_empty() {
                let mut ranges: Vec<(usize, usize)> = evacuated
                    .iter()
                    .map(|&d| {
                        // SAFETY: d is a live old-gen object just written.
                        let sz = gen_object_total_size(unsafe { &*(d as *const ObjectHeader) });
                        (d as usize, sz)
                    })
                    .collect();
                ranges.sort_by_key(|&(d, _)| d);
                let mut overlaps = 0usize;
                for w in ranges.windows(2) {
                    let (d0, s0) = w[0];
                    let (d1, _) = w[1];
                    if d0 + s0 > d1 {
                        overlaps += 1;
                        if overlaps <= 8 {
                            eprintln!(
                                "[sp-trace] DST OVERLAP #{}: {:#x}+{} > {:#x}",
                                overlaps, d0, s0, d1
                            );
                        }
                    }
                }
                let uniq: FxHashSet<usize> = evacuated.iter().map(|&d| d as usize).collect();
                let dups = evacuated.len() - uniq.len();
                eprintln!(
                    "[sp-trace] evac={} dst_overlaps={} dst_dups={} dst_min={:#x} dst_max={:#x} oldused={}M",
                    evacuated.len(),
                    overlaps,
                    dups,
                    ranges.first().map(|r| r.0).unwrap_or(0),
                    ranges.last().map(|r| r.0).unwrap_or(0),
                    old_gen.used() / 1_048_576,
                );
            }

            // (3) Fix up every reference to an evacuated object (follow the
            // forwarding pointers installed above), then dirty cards for the new
            // old→young edges the evacuated copies introduce.
            if !evac_map.is_empty() {
                let fwd_of = |target: usize| -> Option<usize> {
                    if !is_y(target) {
                        return None;
                    }
                    // SAFETY: `is_y` confirmed an 8-aligned young address.
                    let h = unsafe { &*(target as *const ObjectHeader) };
                    if h.is_forwarded() {
                        Some(h.forwarding_address() as usize)
                    } else {
                        None
                    }
                };

                // (3a) References inside surviving (pinned / non-evacuated) young
                // objects → rewrite to the evacuated copies in old gen.
                {
                    let free_blocks = young_from.free_blocks_sorted();
                    let mut free_iter = free_blocks.iter().peekable();
                    let used = young_from.used();
                    let mut cursor = 0usize;
                    while cursor < used {
                        if let Some(&&(off, sz)) = free_iter.peek() {
                            if cursor == off {
                                cursor += sz;
                                free_iter.next();
                                continue;
                            }
                        }
                        let obj = (from_base + cursor) as *mut u8;
                        // SAFETY: cursor within used; from-space mapped.
                        let header = unsafe { &*(obj as *const ObjectHeader) };
                        // Bug-D fix (2026-06-12): skip a GAP-filler sentinel (a
                        // sub-`HEADER_SIZE` TLAB tail) before `gen_object_total_size`.
                        if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                            // SAFETY: offset 4 lies within the >=8-byte gap.
                            let gap = unsafe {
                                std::ptr::read((obj as *const u8).add(4) as *const u32)
                            } as usize;
                            if gap >= 8 && gap < HEADER_SIZE && cursor + gap <= used {
                                cursor += gap;
                                continue;
                            }
                        }
                        let total_size = gen_object_total_size(header);
                        if total_size < HEADER_SIZE || cursor + total_size > used {
                            break;
                        }
                        if header.gc_flags & GC_FLAG_MARKED != 0 && !header.is_forwarded() {
                            fixup_object_fields(obj, header, &fwd_of, &is_y, None);
                        }
                        cursor += total_size;
                    }
                }

                // (3b) References inside the evacuated (now old-gen) copies. Any
                // field still pointing at a (pinned) young object is a new
                // old→young edge whose card must be dirtied.
                let mut new_old_young: Vec<usize> = Vec::new();
                for &dst in &evacuated {
                    // SAFETY: dst is a live old-gen object just written.
                    let dhdr = unsafe { &*(dst as *const ObjectHeader) };
                    let mut pts = false;
                    fixup_object_fields(dst, dhdr, &fwd_of, &is_y, Some(&mut pts));
                    if pts {
                        new_old_young.push(dst as usize);
                    }
                }

                // (3c) Existing old→young edges (dirty-card seeds): rewrite any
                // whose young target was evacuated (now old→old). NOTE: a full
                // old-gen walk here did NOT fix the bintrees18 wrong-checksum, so
                // the missed reference is not a clean-card old→young edge — the
                // residual corruption is a register-invisible reference to an
                // evacuated object (see memory reference_osr_main_corruptor).
                {
                    let mut seen: FxHashSet<usize> = FxHashSet::default();
                    for &(old_obj, _, _) in &extra_roots {
                        let oaddr = old_obj.as_ptr() as usize;
                        if !seen.insert(oaddr) {
                            continue;
                        }
                        // SAFETY: dirty-card scan yielded a live old-gen object.
                        let ohdr = unsafe { &*(oaddr as *const ObjectHeader) };
                        fixup_object_fields(oaddr as *mut u8, ohdr, &fwd_of, &is_y, None);
                    }
                }

                if !new_old_young.is_empty() {
                    self.card_table.mark_dirty_bulk(&new_old_young);
                }

                // DBG (CRATONVM_SP_VERIFY): after ALL fixup, does any surviving
                // object still reference a forwarded (evacuated) young object? A
                // nonzero count is a MISSED fixup (dangling ref into a reclaimed
                // slot). Splits old-gen vs surviving-young so we know which path
                // (3a/3b/3c) has the gap. Zero on both ⇒ the corruption is a
                // WRONG-address rewrite, not a missed one.
                if std::env::var_os("CRATONVM_SP_VERIFY").is_some() {
                    // Incoming-reference count to each evacuated destination. In a
                    // forest of trees every node has exactly ONE parent, so any
                    // evacuated object with >=2 incoming heap refs is ALIASING — a
                    // child reference rewritten to a valid-but-wrong old object
                    // (passes the missed-fixup check, but inflates check()).
                    let dsts: FxHashSet<usize> = evac_map.values().copied().collect();
                    let mut incoming: FxHashMap<usize, u32> = FxHashMap::default();
                    let mut missed_old = 0usize;
                    let mut bump = |t: usize| {
                        if dsts.contains(&t) {
                            *incoming.entry(t).or_insert(0) += 1;
                        }
                    };
                    for (oaddr, _sz) in old_gen.walk_objects() {
                        let h = unsafe { &*(oaddr as *const ObjectHeader) };
                        missed_old += forwarded_ref_count(oaddr, h, &is_y);
                        for_each_ref(oaddr, h, &mut bump);
                    }
                    let mut missed_young = 0usize;
                    let fb = young_from.free_blocks_sorted();
                    let mut fi = fb.iter().peekable();
                    let used = young_from.used();
                    let mut c = 0usize;
                    while c < used {
                        if let Some(&&(off, sz)) = fi.peek() {
                            if c == off {
                                c += sz;
                                fi.next();
                                continue;
                            }
                        }
                        let o = (from_base + c) as *mut u8;
                        let h = unsafe { &*(o as *const ObjectHeader) };
                        let ts = gen_object_total_size(h);
                        if ts < HEADER_SIZE || c + ts > used {
                            break;
                        }
                        if h.gc_flags & GC_FLAG_MARKED != 0 && !h.is_forwarded() {
                            missed_young += forwarded_ref_count(o, h, &is_y);
                            for_each_ref(o, h, &mut bump);
                        }
                        c += ts;
                    }
                    let aliased = incoming.values().filter(|&&n| n >= 2).count();
                    let max_in = incoming.values().copied().max().unwrap_or(0);
                    eprintln!(
                        "[sp-verify] evac={} MISSED fwd refs old={} young={} | evac-objs with >=2 incoming (ALIASING)={} max_incoming={}",
                        evac_map.len(),
                        missed_old,
                        missed_young,
                        aliased,
                        max_in,
                    );
                }
            }
        }

        // ----- Diagnostic: inbound-edge search (CRATONVM_DBG_SWEEP_EDGES) --
        //
        // Marking is complete; nothing has been zeroed yet. This answers the
        // decisive question for the bintrees18 / AllocLoop non-moving-sweep
        // corruption: every object the sweep is about to zero is UNMARKED —
        // but is any of them actually still REACHABLE? We look for an inbound
        // edge from something that survives, classified by source:
        //
        //   (1) a passed-in ROOT/finalizer points straight at an unmarked
        //       young object  => `mark_young`'s plausibility filter rejected a
        //       live root (header looked corrupt at mark time).
        //   (2) a young SURVIVOR's reference field points at an unmarked young
        //       object  => intra-young BFS desync (survivor marked, but its
        //       field-scan didn't propagate: wrong num_slots at mark time, a
        //       post-mark SATB write, or a filter false-reject of the target).
        //   (3) an OLD-GEN object's reference field points at an unmarked young
        //       object  => card-table / write-barrier miss (the old->young edge
        //       was never seeded because its card wasn't dirty).
        //
        // ANY of (1)/(2)/(3) firing proves the swept node is reachable => the
        // bug is in marking/seeding (case "b"), not a register/native root gap.
        // If NONE fire across the whole run yet corruption still occurs, the
        // only live reference is outside roots+cards+finalizers+heap — i.e. a
        // register/native-stack root the sweep cannot see (case "a"), or a
        // sweep-walk/sizing defect. Routine dead garbage has no inbound edge,
        // so a clean (no-edge) sweep is NORMAL — only edge hits are bugs.
        if std::env::var_os("CRATONVM_DBG_SWEEP_EDGES").is_some() {
            use std::collections::HashSet;
            // Local young-membership test (the `in_young` closure above is
            // borrowed by `mark_young` for the rest of the fn; use a fresh one).
            let is_young = |a: usize| -> bool {
                a >= from_base && a < from_end && (a & 0x7) == 0
            };
            let is_unmarked_young = |a: usize| -> bool {
                if !is_young(a) {
                    return false;
                }
                // SAFETY: `is_young` confirmed an 8-aligned addr inside live
                // from-space; reading its header is valid.
                let h = unsafe { &*(a as *const ObjectHeader) };
                h.gc_flags & GC_FLAG_MARKED == 0
            };

            let root_set: HashSet<usize> =
                roots.iter().map(|r| r.as_ptr() as usize).collect();

            // (1) roots / finalizers that landed on an unmarked young object.
            let mut root_to_unmarked = 0usize;
            for &addr in root_set.iter() {
                if is_unmarked_young(addr) {
                    root_to_unmarked += 1;
                    if root_to_unmarked <= 8 {
                        let h = unsafe { &*(addr as *const ObjectHeader) };
                        tracing::warn!(
                            "[sweep-edges] (1) ROOT @{:#x} -> UNMARKED young obj \
                             (class_id={} kind=0x{:02x} num_slots={} array_len={}) — \
                             mark filter rejected a live root?",
                            addr, h.class_id.as_u32(), h.kind as u8,
                            h.num_slots, h.array_length,
                        );
                    }
                }
            }
            for &addr in finalizer_addrs.iter() {
                if is_unmarked_young(addr) {
                    root_to_unmarked += 1;
                }
            }

            // Helper: scan one object's reference fields for unmarked-young
            // targets, invoking `report(slot_idx, target_addr)` for each.
            let scan_refs = |obj_ptr: *mut u8, report: &mut dyn FnMut(usize, usize)| {
                // SAFETY: caller guarantees obj_ptr is a valid object header.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if h.kind == ObjectKind::Array {
                    if h.element_type == ArrayElementType::Reference {
                        for i in 0..h.array_length as usize {
                            let sp = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            let raw: u64 = unsafe { std::ptr::read(sp as *const u64) };
                            if raw != 0 && is_unmarked_young(raw as usize) {
                                report(i, raw as usize);
                            }
                        }
                    }
                } else {
                    for si in 0..h.num_slots as usize {
                        let sp = unsafe { obj_ptr.add(HEADER_SIZE + si * SLOT_SIZE) };
                        let v = unsafe { std::ptr::read(sp as *const Value) };
                        if let Value::Object(Some(rf)) = v {
                            let ta = rf.as_ptr() as usize;
                            if is_unmarked_young(ta) {
                                report(si, ta);
                            }
                        }
                    }
                }
            };

            // (2) young survivors referencing an unmarked young object.
            let mut survivor_to_unmarked = 0usize;
            let mut unmarked_total = 0usize;
            let mut marked_total = 0usize;
            {
                let existing_free_dbg = young_from.free_blocks_sorted();
                let mut free_it = existing_free_dbg.iter().peekable();
                let used_dbg = young_from.used();
                let mut c = 0usize;
                while c < used_dbg {
                    if let Some(&&(off, sz)) = free_it.peek() {
                        if c == off {
                            c += sz;
                            free_it.next();
                            continue;
                        }
                    }
                    let optr = (from_base + c) as *mut u8;
                    let h = unsafe { &*(optr as *const ObjectHeader) };
                    let tot = gen_object_total_size(h);
                    if tot < HEADER_SIZE || c + tot > used_dbg {
                        tracing::warn!(
                            "[sweep-edges] diagnostic walk desynced at off={} \
                             (size={}, used={}) — arena already corrupt before this sweep",
                            c, tot, used_dbg,
                        );
                        break;
                    }
                    if h.gc_flags & GC_FLAG_MARKED != 0 {
                        marked_total += 1;
                        let cid = h.class_id.as_u32();
                        let oaddr = optr as usize;
                        scan_refs(optr, &mut |si, ta| {
                            survivor_to_unmarked += 1;
                            if survivor_to_unmarked <= 16 {
                                let th = unsafe { &*(ta as *const ObjectHeader) };
                                tracing::warn!(
                                    "[sweep-edges] (2) SURVIVOR @{:#x} (class_id={}) field[{}] \
                                     -> UNMARKED @{:#x} (class_id={} num_slots={} kind=0x{:02x}) \
                                     in_roots={}",
                                    oaddr, cid, si, ta, th.class_id.as_u32(),
                                    th.num_slots, th.kind as u8, root_set.contains(&ta),
                                );
                            }
                        });
                    } else {
                        unmarked_total += 1;
                    }
                    c += tot;
                }
            }

            // (3) old-gen objects referencing an unmarked young object
            // (card-table / write-barrier miss). Walks the whole old gen — the
            // dirty-card seed above only covers cards that were marked dirty,
            // so this is exactly the set the seeding could have missed.
            let mut old_to_unmarked = 0usize;
            for (optr, _sz) in old_gen.walk_objects() {
                let oaddr = optr as usize;
                let cid = unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() };
                scan_refs(optr, &mut |si, ta| {
                    old_to_unmarked += 1;
                    if old_to_unmarked <= 16 {
                        let th = unsafe { &*(ta as *const ObjectHeader) };
                        tracing::warn!(
                            "[sweep-edges] (3) OLD-GEN @{:#x} (class_id={}) field[{}] \
                             -> UNMARKED young @{:#x} (class_id={} num_slots={}) — \
                             card/write-barrier MISS",
                            oaddr, cid, si, ta, th.class_id.as_u32(), th.num_slots,
                        );
                    }
                });
            }

            let verdict = if root_to_unmarked > 0 || survivor_to_unmarked > 0 || old_to_unmarked > 0 {
                "REACHABLE NODE WILL BE SWEPT — case (b) marking/seeding bug"
            } else {
                "no inbound heap edge to any swept node — case (a) register/native root gap or sweep-walk defect"
            };
            tracing::warn!(
                "[sweep-edges] SUMMARY marked={} unmarked={} | edges: root={} young-survivor={} old-gen={} => {}",
                marked_total, unmarked_total,
                root_to_unmarked, survivor_to_unmarked, old_to_unmarked, verdict,
            );
        }

        // ----- Sweep phase ------------------------------------------------
        //
        // Walk the from-space linearly, skipping holes already on the free
        // list (exactly as `OldGen::walk_objects` does — a hole's stale
        // bytes must never be parsed as an object). Every *unmarked*
        // object is dead: zero it and return its span to the free list.
        // Every marked object is a survivor: clear the mark and leave it
        // exactly where it is.
        let existing_free = young_from.free_blocks_sorted();
        let mut dead_regions: Vec<(usize, usize)> = Vec::new();
        let mut bytes_swept: usize = 0;
        let mut objects_swept: usize = 0;
        let mut objects_live: usize = 0;

        let mut cursor: usize = 0;
        let used = young_from.used();
        let mut free_iter = existing_free.iter().peekable();
        // Debug diag: keep a short ring buffer of (offset, size, class_id, kind,
        // num_slots, array_length) for the last 6 objects walked. When the
        // implausible-header break fires we dump it so we can pin down which
        // PRIOR object had an undersized/oversized header that mis-aligned the
        // walker into a payload region. Cheap (a Vec push per object) and only
        // logged once per sweep on the abort path.
        let mut walked: Vec<(usize, usize, u32, ObjectKind, u32, u32)> = Vec::new();
        while cursor < used {
            // If `cursor` is the start of a known free block, skip it.
            if let Some(&&(off, sz)) = free_iter.peek() {
                if cursor == off {
                    cursor += sz;
                    free_iter.next();
                    continue;
                }
            }
            // `cursor` is within `used`; the from-space region
            // `[base, base+used)` is backed by mapped, allocated memory. The
            // integer-to-pointer cast itself is safe; only the header deref
            // on the next line requires `unsafe`.
            let obj_ptr = (from_base + cursor) as *mut u8;
            let header = unsafe { &mut *(obj_ptr as *const ObjectHeader as *mut ObjectHeader) };
            // Bug-D fix (2026-06-12): a GAP-filler sentinel marks a
            // sub-`HEADER_SIZE` TLAB tail (`install_tail_filler`) that is too
            // small to hold a walkable `int[]`. It carries its exact byte
            // length at offset 4. Reclaim the span and continue — this MUST
            // run before `gen_object_total_size`, whose `num_slots` read
            // (offset 16) would fall outside an 8-byte gap. We SKIP rather
            // than free it: a sub-`HEADER_SIZE` span can never satisfy an
            // allocation (the smallest object/array is `HEADER_SIZE` bytes),
            // so adding it to the free list only bloats the linear free-list
            // scan with permanently-unusable tiny blocks. Skipping keeps the
            // walk exactly on the object grid; the span is reclaimed wholesale
            // when the next moving (Cheney) cycle resets from-space.
            if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                // SAFETY: offset 4 lies within the >=8-byte gap.
                let gap = unsafe {
                    std::ptr::read((obj_ptr as *const u8).add(4) as *const u32)
                } as usize;
                if gap >= 8 && gap < HEADER_SIZE && cursor + gap <= used {
                    cursor += gap;
                    continue;
                }
                // Malformed sentinel (should be impossible) — fall through to
                // the corrupt-header re-sync path below.
            }
            let total_size = gen_object_total_size(header);
            // Defensive: a corrupt / zero-size header would desynchronise
            // the linear walk. Stop rather than risk freeing live data.
            if total_size < HEADER_SIZE || cursor + total_size > used {
                // NOTE: format the RAW kind byte, not `header.kind` via Debug.
                // A corrupt header can hold an out-of-range discriminant; the
                // derived `Debug` for `ObjectKind` indexes a static name table
                // by discriminant, so `{:?}` on an invalid value reads past the
                // table into rodata and SIGSEGVs — turning a recoverable
                // corrupt-header detection into a hard crash (observed: JIT
                // miscompile of `JUnitCore.main` under real-JCA). Same for the
                // re-sync probe below.
                let raw_kind = header.kind as u8;
                if SWEEP_CORRUPTION_HITS.fetch_add(1, Ordering::Relaxed) == 0 {
                    eprintln!(
                        "[quiesce] FIRST corruption: quiescence depth={} enter_count={} leave_count={}",
                        crate::gc_quiescence::depth(),
                        crate::gc_quiescence::ENTER_COUNT.load(Ordering::Relaxed),
                        crate::gc_quiescence::LEAVE_COUNT.load(Ordering::Relaxed),
                    );
                }
                tracing::warn!(
                    "non-moving sweep: stopping walk at offset {} — implausible \
                     object size {} (kind=0x{:02x}, num_slots={}, array_len={}, class_id={})",
                    cursor,
                    total_size,
                    raw_kind,
                    header.num_slots,
                    header.array_length,
                    header.class_id.as_u32(),
                );
                tracing::warn!(
                    "  total walked={} objects, used={} from_base={:#x}",
                    walked.len(), used, from_base,
                );
                // Find the FIRST cursor where the all-zero-header pattern
                // started (num_slots == 0 && class_id == 0 && kind == Object).
                let first_zero = walked.iter().position(|(_, _, cid, k, ns, _)| {
                    *ns == 0 && *cid == 0 && matches!(k, ObjectKind::Object)
                });
                if let Some(idx) = first_zero {
                    let start = idx.saturating_sub(15);
                    for i in start..=idx {
                        let (off, sz, cid, kind, ns, al) = walked[i];
                        tracing::warn!(
                            "  PRE-corruption idx {} @off={} size={} class_id={} kind=0x{:02x} num_slots={} array_length={}",
                            i, off, sz, cid, kind as u8, ns, al,
                        );
                    }
                }
                let recent: Vec<_> = walked.iter().rev().take(8).rev().cloned().collect();
                for (off, sz, cid, kind, ns, al) in &recent {
                    tracing::warn!(
                        "  prior obj @off={} size={} class_id={} kind=0x{:02x} num_slots={} array_length={}",
                        off, sz, cid, *kind as u8, ns, al,
                    );
                }
                // Dump 64 bytes of context starting 16 bytes before the bad
                // header so we can see the tail of the previous object's payload.
                let start = cursor.saturating_sub(16);
                let end = (cursor + 48).min(used);
                let mut hex = String::new();
                for i in start..end {
                    // SAFETY: i < used, region mapped.
                    let b = unsafe { *((from_base + i) as *const u8) };
                    hex.push_str(&format!("{:02x} ", b));
                    if (i - start + 1) % 16 == 0 { hex.push('\n'); }
                }
                tracing::warn!("  bytes around bad header (start_off={}):\n{}", start, hex);

                // Defensive recovery: instead of `break` (which abandons
                // the rest of the arena and leaves dead objects unreclaimed
                // → young exhaust → OOM/SIGSEGV downstream), scan forward
                // in 8-byte (slot) increments looking for the next
                // plausible-looking header. This re-syncs the walker past
                // the corrupted region so the remainder of the arena can
                // still contribute free spans. The skipped region is left
                // out of `existing_free` — conservatively treated as live —
                // and will be recovered by the next major-GC compaction.
                const MAX_RESYNC_SKIP: usize = 1 << 20; // 1 MiB scan budget
                const MAX_PLAUSIBLE_OBJ_BYTES: usize = 1 << 28; // 256 MiB sanity ceiling
                let mut probe = cursor + 8;
                let mut found = false;
                while probe + HEADER_SIZE <= used && probe - cursor <= MAX_RESYNC_SKIP {
                    // SAFETY: probe + HEADER_SIZE <= used, region mapped.
                    let probe_hdr = unsafe { &*((from_base + probe) as *const ObjectHeader) };
                    let probe_size = gen_object_total_size(probe_hdr);
                    let kind_byte = probe_hdr.kind as u8;
                    if kind_byte <= 1
                        && probe_size >= HEADER_SIZE
                        && probe_size <= MAX_PLAUSIBLE_OBJ_BYTES
                        && probe + probe_size <= used
                        && probe_hdr.num_slots <= (1 << 24)
                        && probe_hdr.array_length <= i32::MAX as u32
                    {
                        tracing::warn!(
                            "non-moving sweep: RE-SYNCED at offset {} (skipped {} bytes) — \
                             class_id={} kind=0x{:02x} size={}; abandoned region treated as live, \
                             will be recovered by next major GC",
                            probe, probe - cursor,
                            probe_hdr.class_id.as_u32(), probe_hdr.kind as u8, probe_size,
                        );
                        cursor = probe;
                        found = true;
                        break;
                    }
                    probe += 8;
                }
                if !found {
                    tracing::warn!(
                        "non-moving sweep: no re-sync within {} bytes from offset {} — \
                         abandoning rest of arena ({} bytes opaque)",
                        MAX_RESYNC_SKIP, cursor, used - cursor,
                    );
                    break;
                }
                continue;
            }

            walked.push((
                cursor,
                total_size,
                header.class_id.as_u32(),
                header.kind,
                header.num_slots,
                header.array_length,
            ));

            if header.is_forwarded() {
                // Evacuated to old gen by selective promotion: the live copy is
                // in old gen and references were redirected in the fixup pass;
                // reclaim (and zero) the young slot. (A "don't zero" variant was
                // tested to rule out a register-only dangling read — it did NOT
                // fix bintrees18's wrong checksum, so the residual corruption is
                // a structural wrong-address fixup, not a dangling read.)
                // SAFETY: span within from-space (checked above).
                unsafe { std::ptr::write_bytes(obj_ptr, 0, total_size) };
                dead_regions.push((cursor, total_size));
                bytes_swept += total_size;
                objects_swept += 1;
            } else if header.gc_flags & GC_FLAG_MARKED != 0 {
                // Survivor: clear the mark, keep in place, and age it so the
                // next sweep can tenure it once it reaches PROMOTION_AGE
                // (selective promotion). Saturating so a long-lived pinned
                // object never wraps its age.
                header.gc_flags &= !GC_FLAG_MARKED;
                header.gc_age = header.gc_age.saturating_add(1);
                objects_live += 1;
            } else {
                // Dead: zero the whole object span so a later conservative
                // root scan cannot resurrect a stale header inside the
                // reclaimed hole, then record it for the free list.
                // SAFETY: `[obj_ptr, obj_ptr+total_size)` lies within the
                // live from-space region (checked above).
                unsafe { std::ptr::write_bytes(obj_ptr, 0, total_size) };
                dead_regions.push((cursor, total_size));
                bytes_swept += total_size;
                objects_swept += 1;
            }
            cursor += total_size;
        }

        // Publish reclaimed regions to the arena's free list FIRST. Subsequent
        // `try_alloc_young` calls will satisfy allocations from these holes
        // before bumping the cursor — reclaiming memory without moving a
        // survivor.
        //
        // ORDER MATTERS (bintrees18 Bug B fix): this must run BEFORE
        // `clear_all_mark_bits_in_arena` below. The main sweep loop zeroed each
        // dead object in place but only *collected* the spans in `dead_regions`
        // — it had not yet added them to the free list. `clear_all_mark_bits_in_arena`
        // re-walks the whole from-space and skips only the regions on the free
        // list; if the just-zeroed dead spans are not yet published, it strides
        // INTO a zeroed (num_slots=0) span, decodes it as a 40-byte object, and
        // — when the span isn't a multiple of HEADER_SIZE — overshoots off the
        // object grid into a live Node's interior `Value::Object` field cell.
        // That produced the non-deterministic "inconsistent header
        // (class_id=4, array_length=1)" warnings on bintrees18 (a false
        // positive: the Nodes are valid; the *walk* desynced). The main sweep
        // loop never desynced because it knew each object's size before zeroing
        // it; publishing the holes first makes the re-walk hole-aware too.
        for (off, sz) in dead_regions {
            young_from.add_free_block(off, sz);
        }

        // Coalesce adjacent free blocks into maximal spans. Selective promotion
        // evacuates the long-lived set and the sweep reclaims the short-lived
        // churn, leaving a large CONTIGUOUS free region carved into hundreds of
        // thousands of Node-sized holes. `Arena::alloc` scans the free list
        // LINEARLY, so an un-coalesced 500k-entry free list makes every
        // subsequent allocation O(n) — the bintrees18 allocation cliff (young
        // drains correctly but throughput collapses). Merging adjacent holes
        // collapses that region to a handful of spans, restoring near-O(1) bump
        // allocation out of a free block. Tied to `selective_on` (Fix A): it is
        // the necessary partner of the evacuation above — without it the default-on
        // selective sweep drains young but leaves a 500k-entry free list, so
        // allocation goes O(n) and bt18 throughput collapses (the rc=127 cliff).
        if selective_on && std::env::var_os("CRATONVM_SP_NO_COALESCE").is_none() {
            let sorted = young_from.free_blocks_sorted();
            if sorted.len() > 1 {
                young_from.clear_free_list();
                let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
                for (off, sz) in sorted {
                    if let Some(last) = merged.last_mut() {
                        if last.0 + last.1 == off {
                            last.1 += sz;
                            continue;
                        }
                    }
                    merged.push((off, sz));
                }
                for (off, sz) in merged {
                    young_from.add_free_block(off, sz);
                }
            }
        }

        // Defence-in-depth: unconditionally clear `GC_FLAG_MARKED` on every
        // object header in the from-space. The survivor branch above already
        // clears the bit for objects it visited, but if the walk broke out
        // early (corrupt / zero-size header) every later survivor would keep
        // a stale mark. The next non-moving sweep treats any object with the
        // mark bit set as live regardless of root reachability, which would
        // retain garbage indefinitely (bug C5). Re-walk and clear all marks
        // — cheap (one byte per header) and idempotent on this path. Runs on
        // both the normal-completion and the `break` arm because it sits
        // after the `while` loop. Now hole-aware: the dead spans are on the
        // free list (published above), so the re-walk skips them.
        clear_all_mark_bits_in_arena(&mut young_from);

        let live_bytes = bytes_before.saturating_sub(bytes_swept);
        tracing::debug!(
            "non-moving young sweep: {} live objects ({} bytes), {} dead \
             objects reclaimed ({} bytes) into free list",
            objects_live,
            live_bytes,
            objects_swept,
            bytes_swept,
        );

        self.stats.minor_gc_count.fetch_add(1, Ordering::Relaxed);

        if std::env::var_os("CRATONVM_DBG_PRECISE").is_some() && !evac_map.is_empty() {
            eprintln!("[PRECISE] sweep_young_non_moving returning evac_map.len()={}", evac_map.len());
        }

        (
            GcResult {
                stats: crate::gc::GcStats {
                    objects_copied: 0,
                    bytes_copied: live_bytes,
                    bytes_freed: bytes_swept,
                },
                // Selective promotion may have evacuated some survivors to old
                // gen; `evac_map` (young→old) lets the VM-level remap update any
                // reference the conservative sweep could not. Empty when the
                // CRATONVM_SELECTIVE_PROMOTE gate is off (true non-moving).
                pointer_map: evac_map,
            },
            // Resurrected finalizers keep their addresses (non-moving).
            finalizer_addrs.to_vec(),
        )
    }

    /// Run a major garbage collection on the old generation using mark-compact.
    ///
    /// 1. **Mark phase:** starting from `roots` + all young-gen objects, traverse
    ///    the heap marking live old-gen objects (set `GC_FLAG_MARKED`).
    /// 2. **Compact phase:** slide all live objects toward the start of the heap,
    ///    eliminating fragmentation. Update all internal references.
    /// 3. **Cross-gen fixup:** update young-gen references that pointed into
    ///    old gen to use the new compacted addresses.
    /// 4. **Root fixup:** update external root references pointing into old gen.
    ///
    /// Returns a pointer map (old_addr → new_addr) for relocated old-gen objects.
    /// The caller merges this into the overall `GcResult` for VM-level root updates.
    fn major_gc(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
    ) -> HashMap<usize, usize> {
        // ---- Mark phase ---- BFS from roots + young-gen cross-references ----

        let mut worklist: Vec<*mut u8> = Vec::new();

        // Seed: root ObjectRefs that point into old gen
        for root in roots.iter() {
            let ptr = root.as_ptr();
            if old_gen.contains(ptr) {
                // SAFETY: `ptr` is a root ObjectRef in old gen (verified by `contains` above); its header is valid.
                let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
                if header.gc_flags & GC_FLAG_MARKED == 0 {
                    header.gc_flags |= GC_FLAG_MARKED;
                    worklist.push(ptr);
                }
            }
        }

        // Seed: young from-space references into old gen
        Self::mark_young_to_old_refs(young_from, old_gen, &mut worklist);

        // BFS: transitively mark all reachable old-gen objects
        while let Some(obj_ptr) = worklist.pop() {
            Self::scan_object_for_old_refs(obj_ptr, old_gen, &mut worklist);
        }

        // ---- Compact phase ---- sliding compaction of old gen ----

        let compact_map = old_gen.compact();

        // ---- Cross-gen fixup ---- update young-gen refs into old gen ----

        if !compact_map.is_empty() {
            Self::fixup_young_old_refs(young_from, &compact_map);

            // ---- Root fixup ---- update roots pointing into old gen ----
            for root in roots.iter_mut() {
                let old_addr = root.as_ptr() as usize;
                if let Some(&new_addr) = compact_map.get(&old_addr) {
                    // SAFETY: `new_addr` comes from the compaction map, pointing to a valid relocated object.
                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }

        compact_map
    }

    /// Scan young from-space for references into old gen and mark them.
    fn mark_young_to_old_refs(
        young_from: &Arena,
        old_gen: &OldGen,
        worklist: &mut Vec<*mut u8>,
    ) {
        // Skip the zeroed holes the non-moving sweep leaves in from-space.
        // Without this, this linear walk strides into a reclaimed hole, decodes
        // its zeroed bytes as a `num_slots=0` (40-byte) object, and desyncs off
        // the true object grid — landing on a live object's interior field cell
        // (the bintrees18 "inconsistent header class_id=4" false positive) and
        // `break`ing early, which abandons the rest of the young→old mark scan
        // (a real correctness bug: old objects referenced past the hole go
        // unmarked). The non-moving sweep itself skips holes the same way; every
        // linear from-space walker must too. (Cheney is immune: it resets
        // from-space each cycle, so holes never accumulate there.)
        let free_blocks = young_from.free_blocks_sorted();
        let mut free_iter = free_blocks.iter().peekable();
        let mut cursor: usize = 0;
        while cursor < young_from.used() {
            if let Some(&&(off, sz)) = free_iter.peek() {
                if cursor == off {
                    cursor += sz;
                    free_iter.next();
                    continue;
                }
            }
            // SAFETY: `cursor` is within `young_from.used()`; pointer arithmetic stays in the arena.
            let obj_ptr = unsafe { young_from.base_ptr().add(cursor) as *mut u8 };
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Bug-D fix (2026-06-12): stride over a GAP-filler sentinel (a
            // sub-`HEADER_SIZE` TLAB tail) — dead filler with no refs to scan.
            // Must precede `gen_object_total_size` (its offset-16 `num_slots`
            // read falls outside an 8-byte gap). Like the holes skipped above,
            // an un-reclaimed sentinel here would desync this from-space walk.
            if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                // SAFETY: offset 4 lies within the >=8-byte gap.
                let gap = unsafe {
                    std::ptr::read((obj_ptr as *const u8).add(4) as *const u32)
                } as usize;
                if gap >= 8 && gap < HEADER_SIZE && cursor + gap <= young_from.used() {
                    cursor += gap;
                    continue;
                }
            }
            let total_size = gen_object_total_size(header);
            if total_size < HEADER_SIZE {
                break;
            }

            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                        if raw != 0 {
                            let ref_ptr = raw as usize as *mut u8;
                            if old_gen.contains(ref_ptr) {
                                // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                                let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                                if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                                    ref_header.gc_flags |= GC_FLAG_MARKED;
                                    worklist.push(ref_ptr);
                                }
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        let ref_ptr = ref_obj.as_ptr();
                        if old_gen.contains(ref_ptr) {
                            // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                            let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                            if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                                ref_header.gc_flags |= GC_FLAG_MARKED;
                                worklist.push(ref_ptr);
                            }
                        }
                    }
                }
            }

            cursor += total_size;
        }
    }

    /// Scan a single object's reference slots for old-gen pointers and mark them.
    fn scan_object_for_old_refs(
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        worklist: &mut Vec<*mut u8>,
    ) {
        // SAFETY: `obj_ptr` is a live old-gen object from the mark worklist; its header is valid.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    // SAFETY: `i` < `array_length`; offset within array data region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if old_gen.contains(ref_ptr) {
                            // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                            let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                            if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                                ref_header.gc_flags |= GC_FLAG_MARKED;
                                worklist.push(ref_ptr);
                            }
                        }
                    }
                }
            }
        } else {
            for slot_idx in 0..header.num_slots as usize {
                // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if old_gen.contains(ref_ptr) {
                        // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                        let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                        if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                            ref_header.gc_flags |= GC_FLAG_MARKED;
                            worklist.push(ref_ptr);
                        }
                    }
                }
            }
        }
    }

    /// After old-gen compaction, update references in young from-space that
    /// pointed to old-gen objects which have been relocated.
    fn fixup_young_old_refs(young_from: &Arena, compact_map: &HashMap<usize, usize>) {
        // Skip non-moving-sweep holes — same rationale as `mark_young_to_old_refs`:
        // a linear from-space walk must not stride into a reclaimed zeroed hole
        // (it would desync off the object grid and misread a live object's
        // interior cell, then `break` and leave the rest of from-space's
        // old-gen refs un-fixed-up after a compaction → dangling pointers).
        let free_blocks = young_from.free_blocks_sorted();
        let mut free_iter = free_blocks.iter().peekable();
        let mut cursor: usize = 0;
        while cursor < young_from.used() {
            if let Some(&&(off, sz)) = free_iter.peek() {
                if cursor == off {
                    cursor += sz;
                    free_iter.next();
                    continue;
                }
            }
            // SAFETY: `cursor` is within `young_from.used()`; pointer arithmetic stays in the arena.
            let obj_ptr = unsafe { young_from.base_ptr().add(cursor) as *mut u8 };
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Bug-D fix (2026-06-12): stride over a GAP-filler sentinel (a
            // sub-`HEADER_SIZE` TLAB tail) — dead filler with no refs to scan.
            // Must precede `gen_object_total_size` (its offset-16 `num_slots`
            // read falls outside an 8-byte gap). Like the holes skipped above,
            // an un-reclaimed sentinel here would desync this from-space walk.
            if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                // SAFETY: offset 4 lies within the >=8-byte gap.
                let gap = unsafe {
                    std::ptr::read((obj_ptr as *const u8).add(4) as *const u32)
                } as usize;
                if gap >= 8 && gap < HEADER_SIZE && cursor + gap <= young_from.used() {
                    cursor += gap;
                    continue;
                }
            }
            let total_size = gen_object_total_size(header);
            if total_size < HEADER_SIZE {
                break;
            }

            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data region.
                        let slot = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                        if raw != 0 {
                            if let Some(&new_addr) = compact_map.get(&(raw as usize)) {
                                // SAFETY: Writing the compacted address back to the same valid ref-array slot.
                                unsafe { std::ptr::write(slot as *mut u64, new_addr as u64) };
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(slot as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        if let Some(&new_addr) = compact_map.get(&(ref_obj.as_ptr() as usize)) {
                            // SAFETY: `new_addr` comes from the compaction map, pointing to a valid relocated object.
                            let new_value = Value::Object(Some(unsafe {
                                ObjectRef::from_raw(new_addr as *mut u8)
                            }));
                            // SAFETY: Writing updated Value back to the same valid slot.
                            unsafe { std::ptr::write(slot as *mut Value, new_value) };
                        }
                    }
                }
            }

            cursor += total_size;
        }
    }

    // ----- Internal ----------------------------------------------------------

    /// Allocate bytes in the young from-space.
    ///
    /// Returns `None` if the young generation is exhausted and cannot satisfy
    /// the allocation. The caller should trigger a GC cycle and retry, or
    /// throw `OutOfMemoryError`.
    fn try_alloc_young(&self, size: usize) -> Option<*mut u8> {
        // NUMA stub: query the preferred node for the calling thread so
        // the topology probe gets exercised in production. On multi-node
        // hosts this records when the heap's primary node disagrees with
        // the caller's node — that delta is the win the multi-arena
        // upgrade will eventually capture. On single-node hosts (every
        // current test target) this is two integer compares and a return.
        let node = self.numa_slow_path_hint();
        if self.numa_num_nodes > 1 && node != self.numa_node_hint {
            tracing::trace!(
                target: "cratonvm::gc::numa",
                node, primary = self.numa_node_hint, size,
                "try_alloc_young: cross-node slow path (single-arena fallback)",
            );
        }

        let ptr = {
            let mut from = self.young_from.lock();
            from.alloc(size, 8)
            // Lock released here — zeroing happens outside the lock
        };
        if let Some(ptr) = ptr {
            // Zero the block after releasing the lock (O(size) memset)
            // SAFETY: `ptr` was just allocated from the arena with `size` bytes; zeroing is within bounds.
            unsafe { std::ptr::write_bytes(ptr, 0, size) };
            self.stats.young_allocations.fetch_add(1, Ordering::Relaxed);
            return Some(ptr);
        }
        None
    }

    /// Check if an allocation of `size` bytes would succeed in the young from-space.
    /// Does NOT allocate — just probes available space.
    ///
    /// Mirrors what [`Arena::alloc`] would actually do: it can satisfy a request
    /// from the bump tail OR from a single free-list block. Checking ONLY the
    /// bump cursor (`used()` vs `capacity()`) is wrong after a non-moving sweep:
    /// that sweep reclaims dead objects into the free list but cannot retreat the
    /// cursor (it can't relocate survivors while conservative JIT roots are
    /// live), so once the cursor reaches capacity a cursor-only probe reports OOM
    /// even with gigabytes free. The JIT slow path (`jit_new_object`) treats that
    /// false OOM as "retire TLAB + force GC", so EVERY slow-path allocation
    /// forces a young GC — the bintrees18 allocation-failure thrash (~840 GCs/s,
    /// each reclaiming nothing). Falling back to the free list here lets the
    /// allocation proceed from reclaimed space without a spurious GC.
    pub fn try_alloc_young_probe(&self, size: usize) -> Option<()> {
        let from = self.young_from.lock();
        // Bump tail.
        if let Some(aligned) = from.used().checked_add(7).map(|v| v & !7) {
            if let Some(end) = aligned.checked_add(size) {
                if end <= from.capacity() {
                    return Some(());
                }
            }
        }
        // Reclaimed free-list space (only reached when the bump tail can't
        // satisfy the request — keeps the common path a single cursor compare).
        if from.largest_free_block() >= size {
            Some(())
        } else {
            None
        }
    }

    /// Carve out a TLAB-sized chunk from the young from-space.
    ///
    /// Returns `Some((ptr, size))` on success, where `ptr` is the start of
    /// the zeroed region and `size` is the actual TLAB size (may be smaller
    /// than requested if the arena is nearly full).
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        // NUMA stub: same shape as try_alloc_young. The TLAB itself is
        // per-thread so its fast path is already NUMA-local; this records
        // the *refill* node so that once we land per-node arenas we can
        // size each node's young-from to match its TLAB refill pressure.
        let node = self.numa_slow_path_hint();
        if self.numa_num_nodes > 1 && node != self.numa_node_hint {
            tracing::trace!(
                target: "cratonvm::gc::numa",
                node, primary = self.numa_node_hint, requested_size,
                "refill_tlab: cross-node refill (single-arena fallback)",
            );
        }

        let mut from = self.young_from.lock();
        let available = from.remaining();
        if available < 256 {
            return None; // Not enough for a useful TLAB
        }
        let actual_size = requested_size.min(available);
        let ptr = from.alloc(actual_size, 8)?;
        // Zero the TLAB region
        // SAFETY: `ptr` was just allocated from the arena with `actual_size` bytes; zeroing is within bounds.
        unsafe { std::ptr::write_bytes(ptr, 0, actual_size) };
        Some((ptr, actual_size))
    }

    /// Allocate bytes in the young from-space.
    ///
    /// If the from-space is full, logs a fatal error and aborts. The
    /// interpreter's `gc_alloc_*` functions use `try_alloc_young` with
    /// GC-and-retry; this method is only called by the panicking
    /// `alloc_object`/`alloc_array` convenience wrappers.
    fn alloc_young(&self, size: usize) -> *mut u8 {
        self.try_alloc_young(size).unwrap_or_else(|| {
            let from = self.young_from.lock();
            eprintln!(
                "FATAL: OutOfMemoryError: young gen exhausted — tried to allocate {} bytes, \
                 from-space has {}/{} used",
                size,
                from.used(),
                from.capacity(),
            );
            std::process::abort();
        })
    }

    /// Generate the next identity hash code.
    ///
    /// H1: exposed publicly so `interpreter::init_object_header` (TLAB
    /// fast path) can mint a unique hash at allocation time, matching the
    /// non-TLAB allocators. Without this, freshly TLAB-allocated objects
    /// of `ClassId(0)` (java/lang/Object) with no fields produce an
    /// all-zero header that the stale-pointer detector mis-flags as
    /// stale.
    pub fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    /// Forward (copy or promote) a single object from young from-space.
    ///
    /// If the object has been forwarded already, returns the existing address.
    /// If the object has survived enough GCs (age >= PROMOTION_AGE), promotes
    /// it to old gen. Otherwise copies to young to-space with incremented age.
    ///
    /// When `force_promote_all` is true (set by the previous minor GC after
    /// observing a high survival rate, see [`GC_PROMOTE_PRESSURE_PERCENT`]),
    /// every survivor is promoted regardless of age. This breaks the
    /// long-lived-tree semispace death spiral.
    #[allow(clippy::too_many_arguments)]
    fn forward_object(
        young_from: &Arena,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        old_ptr: *mut u8,
        objects_copied: &mut usize,
        pointer_map: &mut FxHashMap<usize, usize>,
        promoted_worklist: &mut Vec<*mut u8>,
        force_promote_all: bool,
    ) -> *mut u8 {
        // DBG (bc math-ec, CRATONVM_DBG_GCWRITE): wrap the forwarder so we can
        // see if it EVER returns a small (<0x1000) address — that would mean a
        // GC ref-update writes `Object(Some(0x4))` from forward_object's result
        // (the value-source for every minor-GC reference write).
        let r = Self::forward_object_impl(
            young_from,
            young_to,
            old_gen,
            old_ptr,
            objects_copied,
            pointer_map,
            promoted_worklist,
            force_promote_all,
        );
        if (r as usize) != 0 && (r as usize) < 0x1000 && gcw_enabled() {
            eprintln!(
                "[gcwrite] forward_object RETURNED 0x{:x} for old_ptr=0x{:x}",
                r as usize, old_ptr as usize,
            );
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn forward_object_impl(
        young_from: &Arena,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        old_ptr: *mut u8,
        objects_copied: &mut usize,
        pointer_map: &mut FxHashMap<usize, usize>,
        promoted_worklist: &mut Vec<*mut u8>,
        force_promote_all: bool,
    ) -> *mut u8 {
        // SAFETY: `old_ptr` points to a live young-gen object; its header is
        // valid. Build an owned *copy* of the header rather than holding a
        // shared `&ObjectHeader`: later in this function we install the
        // forwarding pointer through a `&mut`/raw write to the same address,
        // and a live `&` aliasing that write would be undefined behavior.
        //
        // ATOMIC-UB fix: do NOT `std::ptr::read` the whole `ObjectHeader` by
        // value. `ObjectHeader` embeds `mark_word: AtomicU64`; a `ptr::read`
        // of a struct containing an atomic performs a *non-atomic* read of an
        // atomic location, which is undefined behavior. Instead read each
        // scalar field individually through field-projected raw pointers, and
        // read `mark_word` via an explicit `AtomicU64::load`, then reconstruct
        // an owned header from those values.
        let header: ObjectHeader = unsafe {
            let h = old_ptr as *const ObjectHeader;
            let mut owned = ObjectHeader::new(
                std::ptr::addr_of!((*h).class_id).read(),
                std::ptr::addr_of!((*h).kind).read(),
                std::ptr::addr_of!((*h).element_type).read(),
                std::ptr::addr_of!((*h).identity_hash_code).read(),
                std::ptr::addr_of!((*h).array_length).read(),
                std::ptr::addr_of!((*h).num_slots).read(),
            );
            owned.gc_age = std::ptr::addr_of!((*h).gc_age).read();
            owned.gc_flags = std::ptr::addr_of!((*h).gc_flags).read();
            owned.forwarding_ptr = std::ptr::addr_of!((*h).forwarding_ptr).read();
            // `mark_word` is an `AtomicU64`: read it through an atomic load so
            // the access is well-defined under the memory model.
            owned.mark_word.store(
                (*h).mark_word.load(std::sync::atomic::Ordering::Relaxed),
                std::sync::atomic::Ordering::Relaxed,
            );
            owned
        };
        let header = &header;

        // KC16 SIGSEGV audit: sanity-check header before using it. A corrupted
        // header (e.g., slot tag mis-identified a non-pointer bit-pattern as an
        // ObjectRef) would cause forward_object to walk into arbitrary memory.
        // Emitting forensic output here converts a silent SIGSEGV into a visible
        // diagnostic.
        //
        // Multi-array reloc fix (2026-05-22): the `num_slots > 1<<24` clause
        // used to trip for ANY array whose length exceeds 2^24 elements,
        // because `alloc_array` stores the array length in BOTH
        // `array_length` and `num_slots`. With three back-to-back 2^26-int
        // arrays the third allocation triggers a minor GC; `forward_object`
        // would then refuse to relocate the first two large arrays, return
        // their old pointers without inserting into the pointer map, and
        // young_from.reset() would zero the original headers. After the
        // semispace swap, the still-unremapped roots dereferenced the now-
        // zeroed memory, giving the all-zero `kind/class_id/element_type/
        // array_length` symptom and a `.length` of 0. Guard num_slots only
        // for non-arrays (where it really is a field count). For arrays,
        // gate on `array_length` against the JVM's i32::MAX ceiling — the
        // same cap as `MAX_REASONABLE_ARRAY_LEN` in `array_length()`.
        let kind_byte = header.kind as u8;
        let is_array = header.kind == ObjectKind::Array;
        let array_length_too_large = is_array && header.array_length > i32::MAX as u32;
        let num_slots_too_large = !is_array && header.num_slots > (1 << 24);
        if kind_byte > 1 || num_slots_too_large || array_length_too_large {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_byte,
                num_slots = header.num_slots,
                array_length = header.array_length,
                class_id = ?header.class_id,
                gc_flags = format!("{:x}", header.gc_flags),
                backtrace = ?std::backtrace::Backtrace::capture(),
                "gen_heap::forward_object: suspect header",
            );
            // Leave the object unmoved; this is a suspected false root or
            // corrupted slot. Returning old_ptr preserves progress while the
            // eprintln above gives us the evidence needed to diagnose.
            return old_ptr;
        }

        // Already forwarded?
        if header.is_forwarded() {
            let fwd = header.forwarding_address();
            // KC16 SIGSEGV audit: verify the forwarding address is sane before
            // returning it. A stale forwarding pointer left over from a prior
            // GC cycle that wasn't cleared by session 73's fix would send
            // callers to freed memory.
            //
            // BUG-Z fix: the null/alignment check below is NOT enough — under
            // heavy multi-threaded churn (TestFileStoreConcurrency) the
            // `forwarding_ptr` field of a young_from source has been observed
            // holding 8-aligned non-null GARBAGE (small integers like 0x10/0x1110,
            // or `old - small_offset`) that passes those two checks and then gets
            // recorded into `pointer_map`, sending the GC's post-copy walk (and
            // `update_all_roots`) into a wild pointer → SIGSEGV. A real
            // forwarding address must land inside the to-space (`young_to`) or
            // the old gen; reject anything else as a stale/corrupt forward
            // (return `old_ptr` unmoved, exactly as the null/unaligned arm does)
            // rather than trusting the bogus pointer and recording it.
            let fwd_in_heap =
                young_to.contains(fwd as *const u8) || old_gen.contains(fwd as *const u8);
            if fwd.is_null() || (fwd as usize) % 8 != 0 || !fwd_in_heap {
                tracing::debug!(
                    target: "cratonvm::gc::guard",
                    fwd = ?fwd,
                    old_ptr = ?old_ptr,
                    class_id = ?header.class_id,
                    gc_flags = format!("{:x}", header.gc_flags),
                    gc_age = header.gc_age,
                    backtrace = ?std::backtrace::Backtrace::capture(),
                    "gen_heap::forward_object: bad forwarding_ptr",
                );
                return old_ptr;
            }
            // Ensure pointer_map has this entry so update_all_roots can update
            // all references to this old address, even if this is a second
            // encounter of the same object (e.g., root + dirty card + Cheney scan).
            pointer_map.entry(old_ptr as usize).or_insert(fwd as usize);
            return fwd;
        }

        let total_size = gen_object_total_size(header);

        // Sanity: skip objects whose computed extent does not fit entirely
        // within the young from-space. This guards against false conservative
        // roots from JIT spill slots that bit-match a heap address but don't
        // point at a real object — a bogus header yields a size that runs off
        // the end of the arena. A genuine object, however large, fits within
        // the arena it was allocated from by construction, so this never
        // rejects a real live array (a fixed byte cap did: a 64 MB int[] is
        // perfectly valid at a multi-GB heap).
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.capacity();
        let obj_addr = old_ptr as usize;
        let fits_in_arena = obj_addr >= from_base
            && obj_addr
                .checked_add(total_size)
                .is_some_and(|obj_end| obj_end <= from_end);
        if total_size < HEADER_SIZE || !fits_in_arena {
            tracing::warn!(
                "GC: skipping suspected false root at {:p} (computed size {} bytes, \
                 kind={:?}, num_slots={}, array_len={})",
                old_ptr,
                total_size,
                header.kind,
                header.num_slots,
                header.array_length,
            );
            return old_ptr; // Leave unmoved — likely not a real object
        }

        // bc math-ec 0x4 (2026-06-09): a STALE/INTERIOR pointer whose garbage
        // bytes PARSE plausibly (kind 0/1, small num_slots — common when the
        // bytes are long[] math data) slips past every guard above; the copy
        // is wasteful but the killer is the forwarding-pointer install below:
        // an 8-byte raw write at old_ptr+24 INTO THE MIDDLE OF A LIVE OBJECT,
        // corrupting whatever field/element lives there. One seed then
        // self-propagates: the corrupted cell feeds the next GC another false
        // root. Discriminator: every REAL object's class_id resolves in the
        // registry; garbage class_ids essentially never do. (class_id==0 ==
        // java/lang/Object resolves and stays allowed — zeroed-region refs are
        // handled by the existing guards.)
        // `CRATONVM_DBG_FWDGUARD` logs offenders; `CRATONVM_FWD_RESOLVE_STRICT`
        // rejects them (leave unmoved, no forwarding install) — candidate FIX.
        if fwdguard_enabled() || fwd_resolve_strict() {
            let cid = header.class_id.as_u32();
            if crate::gc::resolve_class_info(cid).is_none() {
                if fwdguard_enabled() {
                    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
                    static N: AtomicUsize = AtomicUsize::new(0);
                    let k = N.fetch_add(1, AOrd::Relaxed);
                    if k < 24 {
                        eprintln!(
                            "[fwdguard] #{k} UNRESOLVABLE class_id={} at old_ptr=0x{:x} \
                             (kind_byte={} num_slots={} array_len={} total_size={}) — {}",
                            cid,
                            old_ptr as usize,
                            header.kind as u8,
                            header.num_slots,
                            header.array_length,
                            total_size,
                            if fwd_resolve_strict() { "REJECTED" } else { "copied anyway" },
                        );
                    }
                }
                if fwd_resolve_strict() {
                    return old_ptr; // false root — never install forwarding at +24
                }
            }
        }
        // Promote if this GC survival would reach or exceed the promotion age,
        // OR if the previous minor GC observed high survival pressure and
        // armed the force-promote-all flag. The latter breaks the death
        // spiral where a long-lived object is repeatedly copied semi→semi
        // because its age hasn't yet reached PROMOTION_AGE.
        //
        // E.g., with PROMOTION_AGE=3: an object at age 2, surviving this GC,
        // would become age 3 → promote instead.
        let should_promote = force_promote_all || header.gc_age + 1 >= PROMOTION_AGE;

        let new_ptr = if should_promote {
            // Promote to old gen
            match old_gen.alloc(total_size, 8) {
                Some(ptr) => ptr,
                None => {
                    // Old gen full — fall back to young to-space
                    // (Major GC will be needed later)
                    match young_to.alloc(total_size, 8) {
                        Some(ptr) => ptr,
                        None => {
                            // UAF fix: both old gen and to-space are full.
                            // Previously this returned `old_ptr`, leaving the
                            // object in from-space. But the caller resets
                            // young_from immediately after this collection,
                            // wiping that memory — every still-live reference
                            // to `old_ptr` would then dangle (use-after-free).
                            // A copying collector cannot safely "skip" an
                            // object: there is no valid address to hand back.
                            // This is an unrecoverable OOM; abort hard, the
                            // same way `alloc_young` handles young-gen
                            // exhaustion.
                            eprintln!(
                                "FATAL: OutOfMemoryError: GC could not relocate a live object — \
                                 both old gen and young to-space are full during promotion \
                                 (tried {} bytes, to-space {}/{} used). Cannot leave the object \
                                 unmoved without dangling references after from-space reset.",
                                total_size,
                                young_to.used(),
                                young_to.capacity(),
                            );
                            std::process::abort();
                        }
                    }
                }
            }
        } else {
            // Copy to young to-space
            match young_to.alloc(total_size, 8) {
                Some(ptr) => ptr,
                None => {
                    // UAF fix: young to-space is full. Previously this
                    // returned `old_ptr`, leaving the object in from-space —
                    // but the caller resets young_from right after this
                    // collection, so every live reference to `old_ptr` would
                    // dangle (use-after-free). A copying collector has no
                    // valid address to return for an un-relocated object.
                    // This is an unrecoverable OOM; abort hard, consistent
                    // with `alloc_young`'s handling of young-gen exhaustion.
                    eprintln!(
                        "FATAL: OutOfMemoryError: GC could not relocate a live object — \
                         young to-space is full (tried {} bytes, to-space has {}/{} used). \
                         Cannot leave the object unmoved without dangling references after \
                         from-space reset.",
                        total_size,
                        young_to.used(),
                        young_to.capacity(),
                    );
                    std::process::abort();
                }
            }
        };

        // Copy the entire object
        // SAFETY: `old_ptr` and `new_ptr` are valid, non-overlapping regions of `total_size` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(old_ptr, new_ptr, total_size);
        }
        // DBG (bc math-ec, CRATONVM_DBG_GCWRITE): does the GC object copy
        // produce a small (<0x1000) object-field payload that the SOURCE did
        // not have? That would mean the copy truncated / used a wrong size and
        // the destination field holds stale to-space bytes (the 0x4). If the
        // source ALSO has the small payload, the 0x4 pre-existed in from-space.
        if !is_array && gcw_enabled() {
            let ns = header.num_slots as usize;
            for si in 0..ns {
                let dv = unsafe {
                    std::ptr::read(new_ptr.add(HEADER_SIZE + si * SLOT_SIZE) as *const Value)
                };
                if let Value::Object(Some(r)) = dv {
                    let p = r.as_ptr() as usize;
                    if p != 0 && p < 0x1000 {
                        let sv = unsafe {
                            std::ptr::read(old_ptr.add(HEADER_SIZE + si * SLOT_SIZE) as *const Value)
                        };
                        let src_small = matches!(sv, Value::Object(Some(rr))
                            if { let q = rr.as_ptr() as usize; q != 0 && q < 0x1000 });
                        eprintln!(
                            "[gcwrite] COPY fld[{}]=0x{:x} src_small={} cid={} num_slots={} total_size={} new=0x{:x}",
                            si, p, src_small, header.class_id.as_u32(), ns, total_size, new_ptr as usize,
                        );
                    }
                }
            }
        }

        // Round-2 fix (T2-4): explicit atomic load+store for the mark_word
        // field. The bulk memcpy above is technically UB for `AtomicU64`:
        // even under STW (no mutator is racing), the memory model still
        // requires that reads/writes of atomic locations go through atomic
        // ops. Replicating the mark word atomically materializes the
        // correct happens-before edge for any observer that may later
        // perform a CAS on the new copy (monitor inflation, etc.).
        //
        // NOTE for future concurrent-GC support: this STW-only protocol is
        // not sufficient for a concurrent collector. A concurrent
        // forwarding protocol must instead CAS-install the forwarding
        // pointer and re-read the mark word if a mutator raced.
        // SAFETY: both `old_ptr` and `new_ptr` point at a fully written
        // ObjectHeader whose `mark_word` field lives at MARK_WORD_OFFSET.
        unsafe {
            let old_header_ptr = old_ptr as *const ObjectHeader;
            let new_header_ptr = new_ptr as *mut ObjectHeader;
            let mark = (*old_header_ptr)
                .mark_word
                .load(std::sync::atomic::Ordering::Relaxed);
            (*new_header_ptr)
                .mark_word
                .store(mark, std::sync::atomic::Ordering::Relaxed);
        }

        // Update the new header
        // SAFETY: `new_ptr` was just allocated and the object was copied there; its header is valid and mutable.
        let new_header = unsafe { &mut *(new_ptr as *mut ObjectHeader) };
        new_header.forwarding_ptr = std::ptr::null_mut();

        let landed_in_old_gen = should_promote && old_gen.contains(new_ptr);
        if landed_in_old_gen {
            // Mark as old gen
            new_header.gc_flags |= GC_FLAG_OLD_GEN;
        } else {
            // Increment age for young gen survivors
            new_header.gc_age = new_header.gc_age.saturating_add(1);
        }

        // Install forwarding pointer in the old header.
        // SAFETY: `old_ptr` is a valid young-gen object; writing the forwarding
        // pointer into its header is safe. Written through a raw pointer so no
        // `&mut ObjectHeader` is ever live alongside another reference to this
        // header (we earlier read an owned copy rather than borrowing it).
        unsafe {
            std::ptr::addr_of_mut!((*(old_ptr as *mut ObjectHeader)).forwarding_ptr).write(new_ptr);
        }

        pointer_map.insert(old_ptr as usize, new_ptr as usize);
        *objects_copied += 1;
        // CRIT-P2 fix: enqueue promoted objects so the alternating Cheney
        // loop can scan them in O(1) per object instead of re-filtering
        // `pointer_map.values()` per iteration. Young to-space copies are
        // already handled by the bump-cursor Cheney scan in the caller, so
        // we only enqueue when the object actually landed in old gen.
        if landed_in_old_gen {
            promoted_worklist.push(new_ptr);
        }

        debug_assert!(young_from.contains(old_ptr));

        new_ptr
    }

    /// Scan dirty cards in the card table for old→young references.
    ///
    /// For each dirty card, walks the objects in that card region and collects
    /// slots containing references into young from-space.
    fn scan_dirty_cards(
        card_table: &CardTable,
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
    ) {
        // Drain the O(dirty) tracking list rather than scanning the whole
        // bitmap with `dirty_card_indices()` — this is O(dirty) instead of
        // O(total cards). `take_dirty_cards` also clears the tracking list,
        // which is fine because `card_table.clear_all()` is called by the
        // caller right after the dirty-card scan completes.
        let mut dirty_indices = card_table.take_dirty_cards();
        if dirty_indices.is_empty() {
            return;
        }

        let card_base = card_table.base_addr();
        let card_size = crate::card_table::CARD_SIZE;
        let old_base = old_gen.base_ptr() as usize;

        // Round-11 perf: instead of `walk_objects()` (which allocates a Vec
        // of *every* old-gen object and then filters per-object), translate
        // the dirty card indices into byte-offset ranges relative to the
        // old-gen data buffer and let `walk_objects_in_card_ranges` collect
        // only the objects that actually start inside a dirty card.
        //
        // The card table covers a region starting at `card_base`; old-gen
        // object pointers may sit at `old_base >= card_base`. Convert each
        // dirty card's `[card_start, card_end)` address window into an
        // offset window relative to `old_base`, clamped to `[0, capacity)`.
        // Cards entirely below the old gen contribute nothing.
        dirty_indices.sort_unstable();
        let old_cap = old_gen.capacity();
        let mut dirty_ranges: Vec<(usize, usize)> = Vec::with_capacity(dirty_indices.len());
        for &card_idx in &dirty_indices {
            let card_start = card_base.saturating_add(card_idx * card_size);
            let card_end = card_start.saturating_add(card_size);
            // Skip cards that end at or before the old-gen base.
            if card_end <= old_base {
                continue;
            }
            let lo = card_start.saturating_sub(old_base).min(old_cap);
            let hi = card_end.saturating_sub(old_base).min(old_cap);
            if lo < hi {
                // Coalesce with the previous range if the cards are
                // contiguous (adjacent dirty cards are common).
                if let Some(last) = dirty_ranges.last_mut() {
                    if last.1 >= lo {
                        last.1 = last.1.max(hi);
                        continue;
                    }
                }
                dirty_ranges.push((lo, hi));
            }
        }
        if dirty_ranges.is_empty() {
            return;
        }

        // Walk old gen, collecting only objects whose start offset lands in
        // a dirty card range. Preserves the original semantics (an object
        // is a root iff the card containing its *header* is dirty).
        let objects = old_gen.walk_objects_in_card_ranges(&dirty_ranges);
        for (obj_ptr, _total_size) in objects {
            // SAFETY: `obj_ptr` is from `old_gen.walk_objects_in_card_ranges()`, pointing to a valid old-gen object header.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

            // Scan ref slots: ref arrays use compact 8-byte pointers,
            // object fields use 16-byte Value.
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                        if raw != 0 {
                            let ref_ptr = raw as usize as *mut u8;
                            if young_from.contains(ref_ptr) {
                                // SAFETY: `obj_ptr` is a valid old-gen object pointer; wrapping in ObjectRef is sound.
                                let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                                extra_roots.push((obj_ref, i, 0));
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        if young_from.contains(ref_obj.as_ptr()) {
                            // SAFETY: `obj_ptr` is a valid old-gen object pointer; wrapping in ObjectRef is sound.
                            let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                            extra_roots.push((obj_ref, slot_idx, 0));
                        }
                    }
                }
            }
        }
    }

    /// The capacity of each young semi-space.
    pub fn young_semi_capacity(&self) -> usize {
        self.young_from.lock().capacity()
    }

    /// The capacity of the old generation.
    pub fn old_gen_capacity(&self) -> usize {
        self.old_gen.lock().capacity()
    }

    /// The number of bytes currently used in the young from-space.
    pub fn young_from_used(&self) -> usize {
        self.young_from.lock().used()
    }

    /// The number of bytes currently used in the old generation.
    pub fn old_gen_used(&self) -> usize {
        self.old_gen.lock().used()
    }

    /// Check if a pointer is in the young from-space.
    pub fn is_in_young(&self, ptr: *const u8) -> bool {
        self.young_from.lock().contains(ptr)
    }

    /// Check if a pointer is in EITHER young semispace. A PRE-GC young
    /// address sits in the buffer that became the (empty) to-space after the
    /// Cheney swap, which `is_in_young` (from-space only) misses — reference
    /// processing uses this to detect stale/dead pre-GC Reference addresses
    /// (in young + not in the pointer map ⇒ the object did not survive).
    pub fn is_in_young_either(&self, ptr: *const u8) -> bool {
        self.young_from.lock().contains(ptr) || self.young_to.lock().contains(ptr)
    }

    /// Check if a pointer is in the old generation.
    pub fn is_in_old(&self, ptr: *const u8) -> bool {
        self.old_gen.lock().contains(ptr)
    }

    /// Walk all live objects in both young and old generations.
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = Vec::new();

        // Walk young generation (from-space only — to-space is GC scratch)
        {
            let young = self.young_from.lock();
            let base = young.base_ptr() as usize;
            let used = young.used();
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                // SAFETY: `ptr` is within `young_from.used()` region; reading the header is valid.
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: HumongousFiller is a walker sentinel
                // installed by the regional GC. The generational heap
                // never allocates humongous, but defensively stop the
                // scan if the kind byte is the sentinel value (treating
                // it as a real object would mis-parse the rest of the
                // stripe).
                if header.kind == ObjectKind::HumongousFiller {
                    break;
                }
                let total_size = if header.kind == ObjectKind::Array {
                    // A malformed `array_length` makes `array_data_size`
                    // overflow/fail. Do NOT silently treat it as a 0-byte
                    // payload — that would advance the cursor by only
                    // HEADER_SIZE and mis-parse the rest of the arena as
                    // bogus objects. Treat it as heap corruption and stop
                    // the walk cleanly, matching the `size < HEADER_SIZE`
                    // handling below.
                    match array_data_size(header.array_length as usize, header.element_type) {
                        Ok(data) => HEADER_SIZE + data,
                        Err(_) => {
                            tracing::warn!(
                                "GC: stopping young-gen heap walk at {:p} — implausible \
                                 array_length {} (element_type={:?}); suspected corrupt header",
                                ptr,
                                header.array_length,
                                header.element_type,
                            );
                            break;
                        }
                    }
                } else {
                    HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
                };
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                result.push((ptr, total_size));
                offset += total_size;
            }
        }

        // Walk old generation
        {
            let old = self.old_gen.lock();
            result.extend(old.walk_objects());
        }

        result
    }
}

impl Default for GenerationalHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GenerationalHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let yf = self.young_from.lock();
        let yt = self.young_to.lock();
        let og = self.old_gen.lock();
        f.debug_struct("GenerationalHeap")
            .field("young_from_used", &yf.used())
            .field("young_from_capacity", &yf.capacity())
            .field("young_to_used", &yt.used())
            .field("young_to_capacity", &yt.capacity())
            .field("old_gen_used", &og.used())
            .field("old_gen_capacity", &og.capacity())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Size / slot access helpers
// ---------------------------------------------------------------------------

/// Selective-promotion fixup: rewrite every reference field of `obj` that
/// points at an evacuated (forwarded) young object to its new old-gen address,
/// via `fwd_of` (returns `Some(new_addr)` for an evacuated young target, else
/// `None`). When `points_young` is `Some`, it is set `true` if any field still
/// points at a *non-evacuated* young object after fixup — the caller uses that
/// to maintain the old→young card invariant when `obj` itself now lives in old
/// gen. `in_young` classifies an address as young-from-space.
///
/// SAFETY: `obj` is a valid object header; its reference slots lie within the
/// object's body. No mutator runs (STW).
fn fixup_object_fields(
    obj: *mut u8,
    header: &ObjectHeader,
    fwd_of: &dyn Fn(usize) -> Option<usize>,
    in_young: &dyn Fn(usize) -> bool,
    mut points_young: Option<&mut bool>,
) {
    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length as usize {
                let slot = unsafe { obj.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                if raw != 0 {
                    if let Some(nw) = fwd_of(raw as usize) {
                        unsafe { std::ptr::write(slot as *mut u64, nw as u64) };
                    } else if let Some(p) = points_young.as_deref_mut() {
                        if in_young(raw as usize) {
                            *p = true;
                        }
                    }
                }
            }
        }
    } else {
        for si in 0..header.num_slots as usize {
            let slot = unsafe { obj.add(HEADER_SIZE + si * SLOT_SIZE) };
            let value = unsafe { std::ptr::read(slot as *const Value) };
            if let Value::Object(Some(ref_obj)) = value {
                let target = ref_obj.as_ptr() as usize;
                if let Some(nw) = fwd_of(target) {
                    let new_value =
                        Value::Object(Some(unsafe { ObjectRef::from_raw(nw as *mut u8) }));
                    unsafe { std::ptr::write(slot as *mut Value, new_value) };
                } else if let Some(p) = points_young.as_deref_mut() {
                    if in_young(target) {
                        *p = true;
                    }
                }
            }
        }
    }
}

/// Selective-promotion verify (CRATONVM_SP_VERIFY): count reference fields of
/// `obj` that still point at a forwarded (evacuated) young object after the
/// fixup pass. A nonzero count is a MISSED fixup — a dangling reference into a
/// reclaimed young slot, the bintrees18 wrong-checksum smoking gun.
fn forwarded_ref_count(
    obj: *mut u8,
    header: &ObjectHeader,
    is_y: &dyn Fn(usize) -> bool,
) -> usize {
    let mut n = 0usize;
    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length as usize {
                let slot = unsafe { obj.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                if raw != 0 && is_y(raw as usize) {
                    let h = unsafe { &*(raw as usize as *const ObjectHeader) };
                    if h.is_forwarded() {
                        n += 1;
                    }
                }
            }
        }
    } else {
        for si in 0..header.num_slots as usize {
            let slot = unsafe { obj.add(HEADER_SIZE + si * SLOT_SIZE) };
            let v = unsafe { std::ptr::read(slot as *const Value) };
            if let Value::Object(Some(rf)) = v {
                let t = rf.as_ptr() as usize;
                if is_y(t) {
                    let h = unsafe { &*(t as *const ObjectHeader) };
                    if h.is_forwarded() {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

/// Invoke `f` with each non-null reference target address held by `obj`'s
/// reference fields (object Value slots or reference-array elements). Used by
/// the CRATONVM_SP_VERIFY aliasing detector.
fn for_each_ref(obj: *mut u8, header: &ObjectHeader, mut f: impl FnMut(usize)) {
    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length as usize {
                let slot = unsafe { obj.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                if raw != 0 {
                    f(raw as usize);
                }
            }
        }
    } else {
        for si in 0..header.num_slots as usize {
            let slot = unsafe { obj.add(HEADER_SIZE + si * SLOT_SIZE) };
            let v = unsafe { std::ptr::read(slot as *const Value) };
            if let Value::Object(Some(rf)) = v {
                f(rf.as_ptr() as usize);
            }
        }
    }
}

/// Compute total size of a heap object (for GC cursor advancement).
/// Objects use SLOT_SIZE per field. Arrays use compact element sizes.
///
/// A malformed `array_length` makes `array_data_size` overflow/fail. Rather
/// than silently returning a 0-byte payload (`HEADER_SIZE`, which advances a
/// scan cursor by the wrong amount and mis-parses subsequent memory), this
/// returns `0` — a value below `HEADER_SIZE`. Every caller already either
/// breaks the walk on `total_size < HEADER_SIZE` or rejects it via the
/// `MAX_SANE_OBJECT_SIZE` corrupt-header check, so a bad array length is
/// surfaced as corruption instead of corrupting the cursor.
#[inline]
/// Cached `CRATONVM_DBG_GCWRITE` gate (bc math-ec diagnostic). Cached in a
/// `OnceLock` so the per-object-copy check in `forward_object` does NOT pay an
/// `env::var_os` lookup on the hot GC path when the gate is off.
#[inline]
fn gcw_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_GCWRITE").is_some())
}

/// Cached `CRATONVM_DBG_FWDGUARD` gate (bc math-ec 0x4): log forward_object
/// candidates whose header class_id does NOT resolve (false interior/stale
/// roots that would get a forwarding_ptr smashed into live-object interiors).
#[inline]
fn fwdguard_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_FWDGUARD").is_some())
}

/// Cached `CRATONVM_FWD_RESOLVE_STRICT` gate: REJECT (leave unmoved, no
/// forwarding install) forward_object candidates with unresolvable class_id.
#[inline]
fn fwd_resolve_strict() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_FWD_RESOLVE_STRICT").is_some())
}

/// Cached `CRATONVM_DBG_SEEDHUNT` gate (bc math-ec `0x4` seed-phase bisect).
/// When on, `collect_garbage_inner` scans the young + old arenas for object/
/// array reference slots holding `Object(Some(p))` with `0 < p < 0x1000` (the
/// `0x4` corruption signature) at three points — GC entry, after the Cheney
/// loop, and after a possible major GC — printing per-phase counts. A count
/// that JUMPS at a specific phase localizes the SEEDING collector path
/// (minor Cheney/promotion vs. major mark-compact) — see
/// docs/bc-math-ec-gc-0x4-handoff.md §6.4.
#[inline]
fn seedhunt_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_SEEDHUNT").is_some())
}

/// Scan a single object's reference slots for the `0x4` seed signature
/// (`Object(Some(0 < p < 0x1000))` in a field, or a raw `0 < x < 0x1000` in a
/// ref-array element). Returns the number of bad slots found; prints up to
/// `cap` victims (shared budget via `printed`).
fn seedhunt_scan_obj(
    optr: *mut u8,
    h: &ObjectHeader,
    label: &str,
    arena: &str,
    printed: &mut usize,
    cap: usize,
) -> usize {
    let mut count = 0usize;
    if h.kind == ObjectKind::Object {
        for si in 0..h.num_slots as usize {
            // SAFETY: `si < num_slots`; offset stays within the object.
            let sp = unsafe { optr.add(HEADER_SIZE + si * SLOT_SIZE) };
            let v = unsafe { std::ptr::read(sp as *const Value) };
            if let Value::Object(Some(r)) = v {
                let p = r.as_ptr() as usize;
                if p != 0 && p < 0x1000 {
                    count += 1;
                    if *printed < cap {
                        *printed += 1;
                        eprintln!(
                            "[seedhunt] {} {} @0x{:x} cid={} fld[{}] -> 0x{:x}",
                            label, arena, optr as usize, h.class_id.as_u32(), si, p,
                        );
                    }
                }
            }
        }
    } else if h.kind == ObjectKind::Array && h.element_type == ArrayElementType::Reference {
        for i in 0..h.array_length as usize {
            // SAFETY: `i < array_length`; offset stays within the array data.
            let sp = unsafe { optr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
            let raw = unsafe { std::ptr::read(sp as *const u64) } as usize;
            if raw != 0 && raw < 0x1000 {
                count += 1;
                if *printed < cap {
                    *printed += 1;
                    eprintln!(
                        "[seedhunt] {} {}ARR @0x{:x} cid={} arr[{}] -> 0x{:x}",
                        label, arena, optr as usize, h.class_id.as_u32(), i, raw,
                    );
                }
            }
        }
    }
    count
}

/// Scan a contiguous, bump-allocated young arena (Cheney to-space, or the
/// post-swap from-space) for the `0x4` seed signature. Linear walk by
/// `gen_object_total_size`; bails on a malformed header (these spaces are
/// freshly compacted/contiguous so the walk normally reaches every object).
fn seedhunt_scan_young(
    base: *const u8,
    used: usize,
    label: &str,
    printed: &mut usize,
    cap: usize,
) -> usize {
    let mut count = 0usize;
    let mut cur = 0usize;
    while cur < used {
        // SAFETY: `cur < used`; pointer stays within the arena.
        let optr = unsafe { base.add(cur) as *mut u8 };
        let h = unsafe { &*(optr as *const ObjectHeader) };
        let size = gen_object_total_size(h);
        if size < HEADER_SIZE || cur + size > used {
            break;
        }
        count += seedhunt_scan_obj(optr, h, label, "Y", printed, cap);
        cur += size;
    }
    count
}

fn gen_object_total_size(header: &ObjectHeader) -> usize {
    if header.kind == ObjectKind::Array {
        match array_data_size(header.array_length as usize, header.element_type) {
            Ok(data) => HEADER_SIZE + data,
            Err(_) => {
                tracing::warn!(
                    "GC: implausible array_length {} (element_type={:?}) in heap object \
                     header — treating as corrupt; caller will stop/skip the walk",
                    header.array_length,
                    header.element_type,
                );
                0
            }
        }
    } else {
        // Header-coherence sanity check: a correctly-allocated `kind = Object`
        // header always has `array_length = 0` (see `try_alloc_object` / the
        // ObjectHeader::new contract — only `alloc_array` writes a non-zero
        // array_length, and it sets `kind = Array` together with it).
        //
        // The binary-trees workload exposed a JIT inline-allocation path that
        // writes `array_length` into the header but leaves `kind` at its
        // TLAB-zeroed default of `Object`. The walker, trusting `kind`, would
        // then compute size = HEADER_SIZE + num_slots * SLOT_SIZE (using
        // whatever garbage `num_slots` happened to be — e.g. 55, yielding 920
        // bytes) and overshoot into the next object's payload, eventually
        // reading String char-array bytes as a header (the `"Data"` /
        // `0x61746144` signature observed in the diag dumps).
        //
        // Return 0 here to flag the inconsistency. The non-moving sweep
        // walker (line ~2579) interprets `total_size < HEADER_SIZE` as
        // corruption and falls through to its re-sync path, which finds the
        // next plausible header and resumes walking — losing only the
        // skipped region (recovered by the next major-GC compaction)
        // instead of aborting the whole arena sweep.
        if header.array_length != 0 {
            tracing::warn!(
                "GC: inconsistent header — kind=Object but array_length={} (num_slots={}, \
                 class_id={}); inline-alloc forgot to set kind=Array. Treating as corrupt \
                 so the walker can re-sync.",
                header.array_length,
                header.num_slots,
                header.class_id.as_u32(),
            );
            return 0;
        }
        // Defensive cap on num_slots: no real class has 1<<24 fields, and a
        // value above this is almost certainly garbage from an uninitialised
        // region.  Same fallthrough — walker re-syncs.
        if header.num_slots > (1 << 24) {
            tracing::warn!(
                "GC: implausible num_slots {} on kind=Object header (class_id={}); \
                 treating as corrupt so the walker can re-sync.",
                header.num_slots,
                header.class_id.as_u32(),
            );
            return 0;
        }
        HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
    }
}

/// Compute a pointer to the slot at `index` within an object/array.
#[inline]
fn slot_ptr(obj_ref: ObjectRef, index: usize) -> *mut u8 {
    // SAFETY: Caller guarantees `index` is within the object's slot count; pointer arithmetic stays within the allocation.
    unsafe { obj_ref.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE) }
}

/// Walk every object header in `arena` and clear `GC_FLAG_MARKED`.
///
/// Used after `sweep_young_non_moving` to guarantee no stale mark bits
/// survive into the next collection (bug C5). The non-moving sweep's main
/// loop only clears marks on objects it visits as survivors; if the loop
/// breaks early on a corrupt / zero-size header, every still-marked object
/// past the breakout point would be treated as live by the *next* sweep
/// (`header.gc_flags & GC_FLAG_MARKED != 0`), retaining unreachable
/// garbage indefinitely.
///
/// Walks the same layout as the sweep — skipping known free blocks, parsing
/// headers in place — and is equally defensive about corruption: a
/// zero-size or out-of-range header stops the walk (we cannot safely
/// continue past unknown structure), but every header we *did* reach gets
/// its mark cleared. Cheap (one byte per header) and idempotent.
fn clear_all_mark_bits_in_arena(arena: &mut Arena) {
    let base = arena.base_ptr() as usize;
    let used = arena.used();
    let free_blocks = arena.free_blocks_sorted();
    let mut free_iter = free_blocks.iter().peekable();
    let mut cursor: usize = 0;
    while cursor < used {
        // Skip known free blocks — their bytes are stale and must not be
        // parsed as object headers.
        if let Some(&&(off, sz)) = free_iter.peek() {
            if cursor == off {
                cursor += sz;
                free_iter.next();
                continue;
            }
        }
        // `cursor` is within `used`; the arena's `[base, base+used)` region
        // is backed by mapped, allocated memory. The integer-to-pointer cast
        // itself is safe; only the header deref on the next line requires
        // `unsafe`.
        let obj_ptr = (base + cursor) as *mut u8;
        // SAFETY: `obj_ptr` is 8-byte-aligned (bump arena) and points at the
        // start of an object header within the live region.
        let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
        // Bug-D fix (2026-06-12): stride over a GAP-filler sentinel (a
        // sub-`HEADER_SIZE` TLAB tail) the same way the sweep does. Normally
        // these are already on the free list (skipped above), but if the
        // sweep broke out early one may remain in place; recognise it here so
        // this mark-clearing walk does not desync on it. No mark bit to clear
        // (it is dead filler).
        if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
            // SAFETY: offset 4 lies within the >=8-byte gap.
            let gap = unsafe {
                std::ptr::read((obj_ptr as *const u8).add(4) as *const u32)
            } as usize;
            if gap >= 8 && gap < HEADER_SIZE && cursor + gap <= used {
                cursor += gap;
                continue;
            }
        }
        let total_size = gen_object_total_size(header);
        if total_size < HEADER_SIZE || cursor + total_size > used {
            // Corruption — same defence as the sweep loop. Stop rather than
            // risk parsing arbitrary bytes as a header. Any marks past this
            // point remain, but on the normal-completion path of the sweep
            // this branch is unreachable; on the break path the sweep
            // already abandoned freeing past this offset for the same
            // reason, so any retained mark is no worse than the sweep's own
            // pre-existing conservativism.
            break;
        }
        header.gc_flags &= !GC_FLAG_MARKED;
        cursor += total_size;
    }
}

/// Read a `Value` from a slot pointer.
///
/// DECODE RULE (R-niche): the 16-byte `Value` is read raw, returning the
/// variant whose discriminant was last *written* into the slot. After
/// `Value::Object` gained a `NonNull` niche, the all-zero bit pattern decodes
/// as `Value::Int(0)`, NOT `Value::Object(None)` — there is no "zeroed slot
/// reads as null" shortcut. Reference/uninitialized slots are therefore
/// written an explicit `Value::Object(None)` (see
/// `GenerationalHeap::alloc_object_with_descriptors`); this read does not
/// synthesize `null` from zero bytes.
///
/// # Safety
///
/// `ptr` must point to a valid, initialized `Value`-sized region within a
/// heap-allocated object. The caller must ensure no concurrent writes to
/// the same slot.
// SAFETY: caller guarantees `ptr` points to a valid Value within a heap object.
#[inline]
unsafe fn read_slot(ptr: *mut u8) -> Value {
    std::ptr::read(ptr as *const Value)
}

/// Write a `Value` to a slot pointer.
///
/// # Safety
///
/// `ptr` must point to a `Value`-sized region within a heap-allocated object.
/// The caller must ensure exclusive access to the slot.
// SAFETY: caller guarantees `ptr` points to a valid Value slot within a heap object.
#[inline]
unsafe fn write_slot(ptr: *mut u8, value: Value) {
    std::ptr::write(ptr as *mut Value, value);
}

// ---------------------------------------------------------------------------
// GarbageCollector trait impl
// ---------------------------------------------------------------------------

impl GarbageCollector for GenerationalHeap {
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_object(class_id, num_fields)
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        self.alloc_array(class_id, element_type, length)
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        self.get_header(obj)
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.class_id_of(obj)
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.kind_of(obj)
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.element_type_of(obj)
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        self.identity_hash_code(obj)
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field(obj, index)
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value)
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field_volatile(obj, index)
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field_volatile(obj, index, value)
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.array_length(obj)
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        self.get_array_element(obj, index)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.set_array_element(obj, index, value)
    }

    fn needs_gc(&self) -> bool {
        self.needs_gc()
    }

    fn collect_garbage(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        self.collect_garbage(stw, roots, monitors)
    }

    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        self.write_barrier(obj, stored_value)
    }

    /// Task #25: route through the inherent `satb_barrier` so the
    /// concurrent old-gen marker sees the about-to-be-overwritten ref.
    /// `satb_barrier` early-outs on the `is_marking_active()` check when
    /// concurrent mark is idle.
    #[inline]
    fn write_barrier_pre(&self, _slot: *mut ObjectRef, old: ObjectRef) {
        self.satb_barrier(Value::Object(Some(old)));
    }

    fn allocated_bytes(&self) -> usize {
        self.allocated_bytes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op monitor cleanup for tests in the gc crate.
    struct NoOpMonitors;
    impl crate::collector::MonitorCleanup for NoOpMonitors {
        fn remap_after_gc(&self, _pointer_map: &std::collections::HashMap<usize, usize>) {}
    }

    /// Test-only `StopTheWorldToken`. The single-threaded test harness
    /// trivially satisfies the STW invariant — no other mutator exists.
    #[inline]
    fn stw() -> crate::collector::StopTheWorldToken {
        crate::collector::StopTheWorldToken::new()
    }

    /// Create a small generational heap for testing.
    fn small_gen_heap() -> GenerationalHeap {
        // 4KB young semi-space, 8KB old gen
        GenerationalHeap::with_sizes(4 * 1024, 8 * 1024)
    }

    /// Regression: `with_capacity` MUST scale the young semi-space linearly
    /// with the total heap requested — no hidden internal cap.
    ///
    /// Commit a98a375 moved the split from 25/75 (young_semi = 12.5% of
    /// total) to 50/50 (young_semi = 25% of total). This test pins the
    /// post-commit ratios so a future edit cannot silently regress to an
    /// arbitrary internal ceiling (the original bug had a 32 MiB cap
    /// regardless of -Xmx; the fix has *no* ceiling other than the user-
    /// supplied total).
    ///
    /// We test the same -Xmx values the orchestrator uses to validate
    /// QuickBenchLong's binary-trees-d18 kernel:
    ///   - 256 MiB heap → young_semi = 64 MiB
    ///   - 1 GiB heap   → young_semi = 256 MiB
    ///   - 4 GiB heap   → young_semi = 1 GiB
    ///
    /// Each semi must be exactly `total / 4`; the lower bound (>= the
    /// orchestrator's "≥256 MiB at -Xmx 1g" criterion) is also asserted
    /// explicitly so the test fails loudly if someone reinstates a clamp.
    #[test]
    fn with_capacity_scales_young_semi_with_xmx() {
        // -Xmx 256m → 64 MiB young semi (4× larger than the buggy 32 MiB cap)
        let h_256m = GenerationalHeap::with_capacity(256 * 1024 * 1024);
        assert_eq!(
            h_256m.young_semi_capacity(),
            64 * 1024 * 1024,
            "with_capacity(256m) must give a 64 MiB young semi (25% of total)",
        );
        assert_eq!(
            h_256m.old_gen_capacity(),
            128 * 1024 * 1024,
            "with_capacity(256m) old gen must be 128 MiB (the remaining 50%)",
        );

        // -Xmx 1g → 256 MiB young semi. This is the orchestrator's
        // explicit lower-bound check: "≥256 MiB at -Xmx 1g".
        let h_1g = GenerationalHeap::with_capacity(1024 * 1024 * 1024);
        assert_eq!(
            h_1g.young_semi_capacity(),
            256 * 1024 * 1024,
            "with_capacity(1g) must give a 256 MiB young semi",
        );
        assert!(
            h_1g.young_semi_capacity() >= 256 * 1024 * 1024,
            "with_capacity(1g) young semi must be >= 256 MiB (orchestrator floor)",
        );

        // -Xmx 4g → 1 GiB young semi.
        let h_4g = GenerationalHeap::with_capacity(4_usize * 1024 * 1024 * 1024);
        assert_eq!(
            h_4g.young_semi_capacity(),
            1024 * 1024 * 1024,
            "with_capacity(4g) must give a 1 GiB young semi",
        );

        // Tiny test heap (64 KiB): the floor (512 B) does not engage
        // because 64 KiB / 4 = 16 KiB is well above it.
        let h_64k = GenerationalHeap::with_capacity(64 * 1024);
        assert_eq!(
            h_64k.young_semi_capacity(),
            16 * 1024,
            "with_capacity(64k) must give a 16 KiB young semi",
        );

        // Floor: a 1 KiB heap (well below the 4 KiB total floor) goes
        // through the `total.max(4096)` path, then `4096 / 4 = 1024`
        // which is above the 512-byte minimum.
        let h_1k = GenerationalHeap::with_capacity(1024);
        assert!(
            h_1k.young_semi_capacity() >= 1024,
            "tiny heap must still produce a non-trivial arena (>= 1 KiB)",
        );
    }

    #[test]
    fn alloc_object_in_young() {
        let heap = small_gen_heap();
        let obj = heap.alloc_object(ClassId::new(1), 2);
        assert!(heap.is_in_young(obj.as_ptr()));
        assert!(!heap.is_in_old(obj.as_ptr()));
        assert_eq!(heap.class_id_of(obj), ClassId::new(1));
        assert_eq!(heap.get_header(obj).num_slots, 2);
        assert_eq!(heap.get_header(obj).gc_age, 0);
        assert_eq!(heap.get_header(obj).gc_flags, 0);
    }

    #[test]
    fn alloc_array_in_young() {
        let heap = small_gen_heap();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
        assert!(heap.is_in_young(arr.as_ptr()));
        assert_eq!(heap.array_length(arr), 5);
        assert_eq!(heap.kind_of(arr), ObjectKind::Array);
    }

    /// Humongous-routing regression: an array whose footprint exceeds
    /// `HUMONGOUS_YOUNG_FRACTION_PERCENT` of one young semi-space must
    /// be allocated directly in the old generation, NOT in young.
    ///
    /// Before the humongous path landed, a single allocation larger
    /// than one young semi-space (`Xmx / 4`) would fail with
    /// `OutOfMemoryError: Java heap space (alloc_array length N)` even
    /// when old gen had tens of GiB free.  The acceptance case from
    /// the bug report: a 2 GiB int[] at `--Xmx 16g` (4 GiB young semi,
    /// 8 GiB old).
    ///
    /// We stage the same shape at test scale (1 MiB young semi / 4 MiB
    /// old gen) so the test does not commit a real multi-GiB heap.
    #[test]
    fn humongous_array_routes_to_old_gen() {
        // 1 MiB young semi → humongous threshold = 512 KiB.
        // Old gen has 4 MiB so a 1 MiB array fits comfortably.
        let heap = GenerationalHeap::with_sizes(1024 * 1024, 4 * 1024 * 1024);
        // 256 K ints * 4 bytes/int = 1 MiB array payload; > 512 KiB threshold.
        let big = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 256 * 1024);
        assert!(
            heap.is_in_old(big.as_ptr()),
            "humongous array must land in old gen, not young from-space",
        );
        assert!(
            !heap.is_in_young(big.as_ptr()),
            "humongous array must NOT be in young from-space",
        );
        assert_eq!(heap.array_length(big), 256 * 1024);
        assert_eq!(heap.kind_of(big), ObjectKind::Array);
        // The humongous header MUST have GC_FLAG_OLD_GEN set so the next
        // minor GC's `forward_object` does not try to relocate it from
        // (non-existent) young from-space.
        assert_ne!(
            heap.get_header(big).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "humongous array header must carry GC_FLAG_OLD_GEN",
        );
    }

    /// Small arrays must still allocate in the young generation; the
    /// humongous routing must not steal the fast path for normal-sized
    /// allocations.
    #[test]
    fn small_array_still_in_young() {
        let heap = GenerationalHeap::with_sizes(1024 * 1024, 4 * 1024 * 1024);
        // 16 ints * 4 bytes = 64 bytes payload, well below the 512 KiB
        // humongous threshold.
        let small = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 16);
        assert!(
            heap.is_in_young(small.as_ptr()),
            "small array must still allocate in young from-space",
        );
        assert_eq!(
            heap.get_header(small).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "small young-gen array must not be flagged as old-gen resident",
        );
    }

    /// Fallible `try_alloc_array` must take the humongous path as well —
    /// the interpreter's `gc_alloc_array` calls `try_alloc_array` first
    /// and only falls back to the panicking `alloc_array` via the OOM
    /// formatter, so the routing must work on the fallible path too.
    #[test]
    fn humongous_routing_applies_to_try_alloc_array() {
        let heap = GenerationalHeap::with_sizes(1024 * 1024, 4 * 1024 * 1024);
        let big = heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Int, 256 * 1024)
            .expect("humongous try_alloc_array must succeed when old gen has room");
        assert!(
            heap.is_in_old(big.as_ptr()),
            "try_alloc_array humongous must land in old gen",
        );
        assert_ne!(
            heap.get_header(big).gc_flags & GC_FLAG_OLD_GEN,
            0,
            "humongous try_alloc_array header must carry GC_FLAG_OLD_GEN",
        );
    }

    #[test]
    fn field_set_and_get() {
        let heap = small_gen_heap();
        let obj = heap.alloc_object(ClassId::new(0), 3);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));
        heap.set_field(obj, 2, Value::Float(2.71));

        assert_eq!(heap.get_field(obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(obj, 1).as_long(), Some(100));
        assert_eq!(heap.get_field(obj, 2).as_float(), Some(2.71));
    }

    #[test]
    fn array_set_and_get() {
        let heap = small_gen_heap();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        heap.set_array_element(arr, 0, Value::Int(10)).unwrap();
        heap.set_array_element(arr, 1, Value::Int(20)).unwrap();
        heap.set_array_element(arr, 2, Value::Int(30)).unwrap();

        assert_eq!(heap.get_array_element(arr, 0), Ok(Value::Int(10)));
        assert_eq!(heap.get_array_element(arr, 1), Ok(Value::Int(20)));
        assert_eq!(heap.get_array_element(arr, 2), Ok(Value::Int(30)));
        assert!(heap.get_array_element(arr, 3).is_err());
    }

    #[test]
    fn minor_gc_basic() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));

        let mut roots = vec![obj];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 1);

        // Root should be updated
        let new_obj = roots[0];
        assert_ne!(new_obj.as_ptr(), obj.as_ptr());

        // Fields intact
        assert_eq!(heap.get_field(new_obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(new_obj, 1).as_long(), Some(100));

        // Age should be incremented
        assert_eq!(heap.get_header(new_obj).gc_age, 1);
    }

    #[test]
    fn large_array_survives_gc() {
        // Regression: `forward_object`'s false-root guard rejected any
        // object whose computed size exceeded a hardcoded 64 MiB cap.
        // A genuine `int[16777216]` is 67_108_904 bytes (16M*4 + 40-byte
        // header) — 40 bytes over the cap — so the collector skipped it
        // as a "suspected false root" and freed a still-live array.
        // The guard now bounds-checks against the from-space arena, so a
        // real object of any size that fits the arena survives.
        let heap = GenerationalHeap::with_sizes(80 * 1024 * 1024, 8 * 1024 * 1024);
        let monitors = NoOpMonitors;

        let n: usize = 16_777_216; // 67_108_904-byte int[] — over the old 64 MiB cap
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(arr, 0, Value::Int(7)).unwrap();
        heap.set_array_element(arr, n - 1, Value::Int(12345)).unwrap();

        let mut roots = vec![arr];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        // The array must be forwarded, not skipped as a false root.
        assert_eq!(result.stats.objects_copied, 1, "large array must survive GC");

        let new_arr = roots[0];
        assert_ne!(new_arr.as_ptr(), arr.as_ptr(), "array should have been moved");
        assert_eq!(heap.array_length(new_arr), n);
        assert_eq!(heap.get_array_element(new_arr, 0), Ok(Value::Int(7)));
        assert_eq!(heap.get_array_element(new_arr, n - 1), Ok(Value::Int(12345)));
    }

    #[test]
    fn multiple_large_arrays_survive_gc() {
        // Multi-array reloc regression (2026-05-22): when three back-to-back
        // 2^26-element int[] (256 MiB each) survive a minor GC, every array's
        // length must remain intact after the swap. The previous
        // `forward_object` guard rejected any array whose `num_slots > 2^24`
        // (because `alloc_array` mirrors the length into `num_slots`),
        // leaving the roots pointing at original young-from memory that
        // `reset()` then zeroed — every header looked all-zero
        // (kind=Object, class_id=0, element_type=Reference, array_length=0)
        // and `.length` reported 0. Use a smaller scale (2^20-elt) here to
        // keep the unit test cheap; the failing condition is `num_slots`
        // exceeding the old 2^24 ceiling.
        //
        // We use three arrays so the third allocation forces the same
        // copying-collection codepath the real workload triggers.
        let n: usize = 17 * 1024 * 1024; // 17M elements; num_slots = 17M > 1<<24
        let payload = (n * 4 + 7) & !7usize;
        let arr_bytes = HEADER_SIZE + payload;
        let young_semi = (arr_bytes * 4).max(80 * 1024 * 1024);
        let heap = GenerationalHeap::with_sizes(young_semi, 64 * 1024 * 1024);
        let monitors = NoOpMonitors;

        let a = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(a, n - 1, Value::Int(0xAAAA_AAAAu32 as i32)).unwrap();
        let b = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(b, n - 1, Value::Int(0xBBBB_BBBBu32 as i32)).unwrap();
        let c = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(c, n - 1, Value::Int(0xCCCC_CCCCu32 as i32)).unwrap();

        let mut roots = vec![a, b, c];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);
        assert_eq!(result.stats.objects_copied, 3, "all three large arrays must be forwarded");

        let (na, nb, nc) = (roots[0], roots[1], roots[2]);
        assert_eq!(heap.array_length(na), n, "a.length corrupted after GC");
        assert_eq!(heap.array_length(nb), n, "b.length corrupted after GC");
        assert_eq!(heap.array_length(nc), n, "c.length corrupted after GC");
        assert_eq!(heap.get_array_element(na, n - 1), Ok(Value::Int(0xAAAA_AAAAu32 as i32)));
        assert_eq!(heap.get_array_element(nb, n - 1), Ok(Value::Int(0xBBBB_BBBBu32 as i32)));
        assert_eq!(heap.get_array_element(nc, n - 1), Ok(Value::Int(0xCCCC_CCCCu32 as i32)));
    }

    #[test]
    fn minor_gc_unreachable_freed() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let _dead = heap.alloc_object(ClassId::new(0), 2);
        let live = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(live, 0, Value::Int(999));

        let mut roots = vec![live];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 1);
        assert!(result.stats.bytes_freed > 0);

        let new_live = roots[0];
        assert_eq!(heap.get_field(new_live, 0).as_int(), Some(999));
    }

    #[test]
    fn minor_gc_preserves_references() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Int(77));

        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 2);

        let new_a = roots[0];
        match heap.get_field(new_a, 0) {
            Value::Object(Some(new_b)) => {
                assert_ne!(new_b.as_ptr(), obj_b.as_ptr());
                assert_eq!(heap.get_field(new_b, 0).as_int(), Some(77));
            }
            _ => panic!("Expected A's field to reference B"),
        }
    }

    /// Non-moving young-gen sweep (the JIT-frames-active path).
    ///
    /// With `gc_quiescence` active the collector must run a non-moving
    /// mark-sweep: survivors keep their exact addresses, dead objects
    /// are reclaimed into the free list, and a subsequent allocation
    /// reuses a reclaimed hole. This is the fix for the
    /// "GC skipped: JIT frames are active → OOM" blocker.
    #[test]
    fn non_moving_sweep_when_jit_active() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        // Build a live chain a→b plus a dead object in between.
        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        let dead = heap.alloc_object(ClassId::new(9), 1);
        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Int(4242));
        heap.set_field(dead, 0, Value::Int(-1));

        let a_ptr = obj_a.as_ptr();
        let b_ptr = obj_b.as_ptr();
        let dead_ptr = dead.as_ptr();

        // Simulate "a JIT frame is active" so the collector takes the
        // non-moving path. The guard is paired with a `leave()` below.
        crate::gc_quiescence::enter();
        assert!(crate::gc_quiescence::is_active());

        // `roots` carries only `obj_a`; `obj_b` is reached transitively,
        // `dead` is unreachable.
        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        crate::gc_quiescence::leave();

        // Non-moving: nothing was copied, the pointer map is empty, and
        // the root's address is UNCHANGED.
        assert_eq!(result.stats.objects_copied, 0);
        assert!(result.pointer_map.is_empty());
        assert_eq!(roots[0].as_ptr(), a_ptr, "survivor must not move");

        // Live chain intact at its original addresses.
        assert_eq!(obj_a.as_ptr(), a_ptr);
        match heap.get_field(obj_a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(b.as_ptr(), b_ptr, "B must not move");
                assert_eq!(heap.get_field(b, 0).as_int(), Some(4242));
            }
            other => panic!("A's field should still reference B, got {other:?}"),
        }

        // Dead object's memory was reclaimed (zeroed + on the free list).
        assert!(result.stats.bytes_freed > 0, "dead object must be reclaimed");
        // The reclaimed region was zeroed by the sweep.
        // SAFETY: `dead_ptr` is inside the young arena; reading its
        // (now-freed, zeroed) header is a valid in-bounds read.
        let dead_header = unsafe { &*(dead_ptr as *const ObjectHeader) };
        assert_eq!(dead_header.class_id, ClassId::new(0), "freed hole must be zeroed");

        // A fresh allocation must succeed and reuse the reclaimed hole
        // (it lands at the dead object's old address since that's the
        // first free block).
        let reused = heap.alloc_object(ClassId::new(7), 1);
        assert_eq!(
            reused.as_ptr(),
            dead_ptr,
            "new allocation should reuse the swept hole",
        );
        heap.set_field(reused, 0, Value::Int(555));
        assert_eq!(heap.get_field(reused, 0).as_int(), Some(555));

        // And the live chain is STILL intact after reusing the hole.
        match heap.get_field(obj_a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(heap.get_field(b, 0).as_int(), Some(4242));
            }
            other => panic!("live chain corrupted after hole reuse: {other:?}"),
        }
    }

    #[test]
    fn promotion_after_enough_gcs() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj, 0, Value::Int(42));

        let mut roots = vec![obj];

        // Run PROMOTION_AGE minor GCs — object should be promoted on the last one
        for i in 0..PROMOTION_AGE {
            let result = heap.collect_garbage(&stw(), &mut roots, &monitors);
            assert_eq!(result.stats.objects_copied, 1);
            let cur = roots[0];

            if i < PROMOTION_AGE - 1 {
                // Still in young gen
                assert!(
                    heap.is_in_young(cur.as_ptr()),
                    "Expected in young gen at iteration {i}"
                );
                assert_eq!(heap.get_header(cur).gc_age, i + 1);
            } else {
                // Should be promoted to old gen
                assert!(
                    heap.is_in_old(cur.as_ptr()),
                    "Expected in old gen after {PROMOTION_AGE} GCs"
                );
                assert!(heap.get_header(cur).is_old_gen());
            }
        }

        // Field value should still be intact
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(42));
    }

    #[test]
    fn write_barrier_marks_card_dirty() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        // First, promote an object to old gen
        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![old_obj];

        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        // Allocate a young object
        let young_obj = heap.alloc_object(ClassId::new(0), 0);
        assert!(heap.is_in_young(young_obj.as_ptr()));

        // Store young ref into old object
        heap.set_field(promoted, 0, Value::Object(Some(young_obj)));
        heap.write_barrier(promoted, Value::Object(Some(young_obj)));

        // Card should be dirty.
        //
        // T5.5.2 (HIGH-1 fix): the write barrier now batches into a
        // per-thread buffer instead of touching the global card-table
        // mutex on every store. Drain the buffer manually here so the
        // bitmap reflects the dirtying — the collector performs the
        // same flush + drain at GC entry.
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        let card_idx = (promoted.as_ptr() as usize - heap.card_table.base_addr())
            / crate::card_table::CARD_SIZE;
        assert!(heap.card_table.is_dirty(card_idx));
    }

    #[test]
    fn card_table_preserves_old_to_young_ref() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        // Promote an object to old gen
        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![old_obj];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        // Allocate a young object and store ref from old → young
        let young_obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(young_obj, 0, Value::Int(999));
        heap.set_field(promoted, 0, Value::Object(Some(young_obj)));
        heap.write_barrier(promoted, Value::Object(Some(young_obj)));

        // Minor GC: only the promoted object is a root (young_obj is NOT a root)
        // But the write barrier should have marked the card dirty, so the GC
        // should discover young_obj via the dirty card scan.
        let mut gc_roots = vec![promoted];
        let result = heap.collect_garbage(&stw(), &mut gc_roots, &monitors);

        // young_obj should have been copied (reachable via dirty card)
        assert!(result.stats.objects_copied >= 1);

        // Verify the old→young reference was updated
        let promoted_after = gc_roots[0];
        match heap.get_field(promoted_after, 0) {
            Value::Object(Some(new_young)) => {
                assert_eq!(heap.get_field(new_young, 0).as_int(), Some(999));
            }
            _ => panic!("Expected promoted object to still reference young object"),
        }
    }

    #[test]
    fn needs_gc_threshold() {
        // Create heap with very small young gen
        let heap = GenerationalHeap::with_sizes(1024, 4096);
        assert!(!heap.needs_gc());

        // Allocate objects until threshold is exceeded
        let mut count = 0;
        while !heap.needs_gc() {
            heap.alloc_object(ClassId::new(0), 1);
            count += 1;
            if count > 100 {
                panic!("needs_gc never triggered");
            }
        }
        assert!(heap.needs_gc());
    }

    #[test]
    fn multiple_gc_cycles() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj1 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj1, 0, Value::Int(1));
        let mut roots = vec![obj1];

        // Cycle 1
        let r1 = heap.collect_garbage(&stw(), &mut roots, &monitors);
        assert_eq!(r1.stats.objects_copied, 1);
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(1));

        // Cycle 2: add another object
        let obj2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj2, 0, Value::Int(2));
        heap.set_field(roots[0], 0, Value::Object(Some(obj2)));
        roots = vec![roots[0]];

        let r2 = heap.collect_garbage(&stw(), &mut roots, &monitors);
        assert_eq!(r2.stats.objects_copied, 2);
    }

    #[test]
    fn identity_hash_codes_unique() {
        let heap = small_gen_heap();
        let obj1 = heap.alloc_object(ClassId::new(0), 0);
        let obj2 = heap.alloc_object(ClassId::new(0), 0);
        assert_ne!(heap.identity_hash_code(obj1), heap.identity_hash_code(obj2),);
    }

    #[test]
    fn major_gc_frees_old_gen_garbage() {
        // Create a heap with small old gen to force major GC
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        // Promote several objects to old gen, then drop references to some
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..5 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        // Run enough minor GCs to promote all objects
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        // All should be in old gen now
        for root in &roots {
            assert!(
                heap.is_in_old(root.as_ptr()),
                "Expected object to be in old gen after promotion"
            );
        }

        // Verify values intact
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32));
        }

        // Drop references to some objects (keep indices 0 and 2)
        let kept_root0 = roots[0];
        let kept_root2 = roots[2];
        roots = vec![kept_root0, kept_root2];

        // Manually trigger major GC (mark-compact)
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let old_used_before = old_gen.used();
            let _compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);
            let old_used_after = old_gen.used();
            assert!(
                old_used_after < old_used_before,
                "Major GC should have freed memory: before={}, after={}",
                old_used_before,
                old_used_after,
            );
        }

        // Roots may have been relocated by compaction — use updated refs
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(0));
        assert_eq!(heap.get_field(roots[1], 0).as_int(), Some(2));
    }

    #[test]
    fn major_gc_preserves_old_gen_references() {
        let heap = GenerationalHeap::with_sizes(2 * 1024, 8 * 1024);
        let monitors = NoOpMonitors;

        // Create a chain: A -> B -> C (all will be promoted to old gen)
        let obj_c = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj_c, 0, Value::Int(333));

        let obj_b = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj_b, 0, Value::Object(Some(obj_c)));

        let obj_a = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));

        let mut roots = vec![obj_a];

        // Promote to old gen
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        let promoted_a = roots[0];
        assert!(heap.is_in_old(promoted_a.as_ptr()));

        // Also create some garbage in old gen
        let garbage = heap.alloc_object(ClassId::new(0), 0);
        let mut garbage_roots = vec![garbage];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut garbage_roots, &monitors);
        }
        // Drop reference to garbage

        // Run major GC (mark-compact) with only A as a root
        let mut major_roots = vec![promoted_a];
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut major_roots, &young_from, &mut old_gen);
        }

        // Walk the chain from the (possibly relocated) root: A -> B -> C
        let compacted_a = major_roots[0];
        match heap.get_field(compacted_a, 0) {
            Value::Object(Some(new_b)) => match heap.get_field(new_b, 0) {
                Value::Object(Some(new_c)) => {
                    assert_eq!(heap.get_field(new_c, 0).as_int(), Some(333));
                }
                _ => panic!("Expected B -> C reference"),
            },
            _ => panic!("Expected A -> B reference"),
        }
    }

    #[test]
    fn stress_alloc_gc_cycles() {
        let heap = GenerationalHeap::with_sizes(2 * 1024, 8 * 1024);
        let monitors = NoOpMonitors;

        let mut live_refs: Vec<ObjectRef> = Vec::new();

        // Rapid alloc/dealloc cycles
        for cycle in 0..20 {
            // Allocate some objects
            for j in 0..5 {
                let obj = heap.alloc_object(ClassId::new(0), 1);
                heap.set_field(obj, 0, Value::Int(cycle * 100 + j));
                live_refs.push(obj);
            }

            // Drop half the references
            if live_refs.len() > 5 {
                live_refs = live_refs.split_off(live_refs.len() / 2);
            }

            // Trigger GC if needed
            if heap.needs_gc() {
                heap.collect_garbage(&stw(), &mut live_refs, &monitors);
            }
        }

        // Final GC
        heap.collect_garbage(&stw(), &mut live_refs, &monitors);

        // All surviving objects should be readable without panicking
        for obj_ref in &live_refs {
            let _ = heap.get_field(*obj_ref, 0);
        }
    }

    #[test]
    fn stress_fill_young_and_promote() {
        let heap = GenerationalHeap::with_sizes(2 * 1024, 16 * 1024);
        let monitors = NoOpMonitors;

        let mut roots = Vec::new();

        // Fill young gen repeatedly, causing GC and promotions
        let obj_size = HEADER_SIZE + SLOT_SIZE; // 1 field object
        let approx_objects_per_young = 2 * 1024 / obj_size;

        for wave in 0..6 {
            // Allocate until GC triggers or young gen is full
            let mut wave_objs = Vec::new();
            for i in 0..approx_objects_per_young {
                let obj = match heap.try_alloc_object(ClassId::new(0), 1) {
                    Some(obj) => obj,
                    None => {
                        // Young gen full — trigger GC and retry
                        roots.append(&mut wave_objs);
                        heap.collect_garbage(&stw(), &mut roots, &monitors);
                        match heap.try_alloc_object(ClassId::new(0), 1) {
                            Some(obj) => obj,
                            None => break, // Can't allocate even after GC
                        }
                    }
                };
                heap.set_field(obj, 0, Value::Int((wave * 1000 + i) as i32));
                wave_objs.push(obj);

                if heap.needs_gc() {
                    // Combine with existing roots
                    roots.append(&mut wave_objs);
                    heap.collect_garbage(&stw(), &mut roots, &monitors);
                    break;
                }
            }
            roots.extend(wave_objs);

            // Keep only last 10 objects each wave to manage memory
            if roots.len() > 10 {
                roots = roots.split_off(roots.len() - 10);
            }
        }

        // Everything should be accessible
        for root in &roots {
            let _ = heap.get_field(*root, 0);
        }
    }

    #[test]
    fn gc_handles_cycles_in_young_gen() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj_a = heap.alloc_object(ClassId::new(0), 1);
        let obj_b = heap.alloc_object(ClassId::new(0), 1);

        // A -> B -> A (cycle)
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Object(Some(obj_a)));

        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 2); // both survive

        // Verify the cycle is intact
        let new_a = roots[0];
        match heap.get_field(new_a, 0) {
            Value::Object(Some(new_b)) => match heap.get_field(new_b, 0) {
                Value::Object(Some(back_to_a)) => {
                    assert_eq!(back_to_a.as_ptr(), new_a.as_ptr());
                }
                _ => panic!("Expected B->A cycle"),
            },
            _ => panic!("Expected A->B reference"),
        }
    }

    #[test]
    fn heap_expansion_on_gc_pressure() {
        // Small heap under allocation pressure: GC runs, and if reclamation
        // is low, the to-space expands for the next cycle.
        let heap = GenerationalHeap::with_capacity(64 * 1024); // 64 KB
        let monitors = NoOpMonitors;
        let _initial_cap = heap.young_semi_capacity();

        // Allocate objects, keeping half alive to create GC pressure.
        let mut live: Vec<ObjectRef> = Vec::new();
        for i in 0..200 {
            match heap.try_alloc_object(ClassId::new(0), 2) {
                Some(obj) => {
                    if i % 2 == 0 {
                        live.push(obj);
                    }
                }
                None => {
                    heap.collect_garbage(&stw(), &mut live, &monitors);
                    let obj = heap.try_alloc_object(ClassId::new(0), 2)
                        .expect("alloc should succeed after GC");
                    if i % 2 == 0 {
                        live.push(obj);
                    }
                }
            }
        }
        // The heap should have detected low reclamation and expanded to-space
    }

    #[test]
    fn tlab_refill() {
        let heap = GenerationalHeap::with_capacity(1024 * 1024); // 1 MB
        let result = heap.refill_tlab(64 * 1024);
        assert!(result.is_some(), "refill_tlab should succeed");
        let (ptr, size) = result.unwrap();
        assert!(!ptr.is_null());
        assert!(size > 0);
    }

    #[test]
    fn gc_reclaims_dead_objects() {
        // Allocate objects, don't keep roots to them, GC should reclaim.
        let heap = GenerationalHeap::with_capacity(256 * 1024); // 256 KB
        // Fill up young gen — use try_alloc with manual GC on failure
        let mut dummy_roots: Vec<ObjectRef> = Vec::new();
        for _ in 0..100 {
            if heap.try_alloc_object(ClassId::new(0), 4).is_none() {
                heap.collect_garbage(&stw(), &mut dummy_roots, &NoOpMonitors);
                let _obj = heap.try_alloc_object(ClassId::new(0), 4);
            }
        }
        // GC with empty roots — all objects are dead
        let mut roots = vec![];
        let result = heap.collect_garbage(&stw(), &mut roots, &NoOpMonitors);
        assert!(result.stats.bytes_freed > 0, "GC should free dead objects");
    }

    #[test]
    fn gc_preserves_live_objects() {
        let heap = GenerationalHeap::with_capacity(256 * 1024);
        let live = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(live, 0, Value::Int(42));
        // Allocate dead objects
        for _ in 0..50 {
            if heap.try_alloc_object(ClassId::new(0), 2).is_none() {
                break; // Don't overflow
            }
        }
        let mut roots = vec![live];
        let _result = heap.collect_garbage(&stw(), &mut roots, &NoOpMonitors);
        // The root should still be valid after GC
        let survived = roots[0];
        assert_eq!(heap.get_field(survived, 0).as_int(), Some(42));
    }

    #[test]
    fn heavy_allocation_with_gc_cycles() {
        // Simulates the workload that caused M4 heap exhaustion.
        let heap = GenerationalHeap::with_capacity(256 * 1024); // 256 KB
        let monitors = NoOpMonitors;
        let mut live_objs: Vec<ObjectRef> = Vec::new();

        for i in 0..1000 {
            let obj = heap.alloc_object(ClassId::new(0), 2);
            heap.set_field(obj, 0, Value::Int(i as i32));
            // Keep every 20th object alive
            if i % 20 == 0 {
                live_objs.push(obj);
            }
            // Trigger GC periodically
            if heap.needs_gc() {
                let mut roots: Vec<ObjectRef> = live_objs.clone();
                let _result = heap.collect_garbage(&stw(), &mut roots, &monitors);
                // Update live_objs to new addresses
                live_objs = roots;
            }
        }

        // Verify all surviving objects
        for obj in &live_objs {
            let header = heap.get_header(*obj);
            assert_eq!(header.class_id, ClassId::new(0));
        }
    }

    // -----------------------------------------------------------------------
    // Session 26: GC Compaction tests
    // -----------------------------------------------------------------------

    #[test]
    fn s26_compact_eliminates_fragmentation() {
        // Create a heap with small old gen, promote objects, free some, compact
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        // Allocate 6 objects and promote them all to old gen
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..6 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i * 100));
            roots.push(obj);
        }
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        for root in &roots {
            assert!(heap.is_in_old(root.as_ptr()), "Object should be in old gen");
        }

        // Drop every other object (indices 1, 3, 5) — creates fragmentation
        let kept = vec![roots[0], roots[2], roots[4]];
        roots = kept;

        // Before compaction: multiple free blocks (fragmented)
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let free_blocks_before = old_gen.free_block_count();

            let compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // After compaction: exactly one free block (defragmented)
            assert_eq!(
                old_gen.free_block_count(),
                1,
                "After compaction, there should be exactly one contiguous free block"
            );

            // The largest free block should equal total free space
            let total_free = old_gen.capacity() - old_gen.used();
            assert_eq!(
                old_gen.largest_free_block(),
                total_free,
                "Largest free block should be all free space after compaction"
            );

            // Some objects should have moved
            assert!(
                !compact_map.is_empty() || free_blocks_before <= 1,
                "Objects should have been relocated (or heap was already compacted)"
            );
        }

        // Values should be preserved after compaction
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(0));
        assert_eq!(heap.get_field(roots[1], 0).as_int(), Some(200));
        assert_eq!(heap.get_field(roots[2], 0).as_int(), Some(400));
    }

    #[test]
    fn s26_compact_updates_internal_references() {
        // Object A -> B -> C, all in old gen. Compact should update A->B and B->C.
        let heap = GenerationalHeap::with_sizes(2 * 1024, 8 * 1024);
        let monitors = NoOpMonitors;

        let obj_c = heap.alloc_object(ClassId::new(3), 1);
        heap.set_field(obj_c, 0, Value::Int(777));

        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_b, 0, Value::Object(Some(obj_c)));

        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));

        // Also allocate garbage between chain objects
        let garbage1 = heap.alloc_object(ClassId::new(0), 2);
        let garbage2 = heap.alloc_object(ClassId::new(0), 2);

        let mut roots = vec![obj_a, obj_b, obj_c, garbage1, garbage2];

        // Promote all to old gen
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        // Drop garbage references, keep only A (B and C reachable via A)
        roots = vec![roots[0]];

        // Run major GC (mark-compact)
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // Only 3 live objects (A, B, C) — garbage should be freed
            assert_eq!(
                old_gen.free_block_count(),
                1,
                "After compaction, old gen should have one free block"
            );
        }

        // Walk the chain from compacted A -> B -> C
        let a = roots[0];
        match heap.get_field(a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(heap.class_id_of(b), ClassId::new(2), "B should have class 2");
                match heap.get_field(b, 0) {
                    Value::Object(Some(c)) => {
                        assert_eq!(
                            heap.class_id_of(c),
                            ClassId::new(3),
                            "C should have class 3"
                        );
                        assert_eq!(heap.get_field(c, 0).as_int(), Some(777));
                    }
                    _ => panic!("Expected B -> C reference after compaction"),
                }
            }
            _ => panic!("Expected A -> B reference after compaction"),
        }
    }

    #[test]
    fn s26_compact_recovers_fragmented_space() {
        // Allocate objects in a pattern that fragments the heap, then verify
        // compaction recovers enough space for a large allocation that would
        // have failed without compaction.
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        // Allocate many small objects and promote them
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..8 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        // Record old gen usage before freeing
        let used_before_free = heap.old_gen.lock().used();

        // Free every other object — creates interleaved free blocks
        roots = roots
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % 2 == 0)
            .map(|(_, o)| o)
            .collect();

        // Run mark-compact via major GC
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // Used space should have decreased (half the objects freed)
            assert!(
                old_gen.used() < used_before_free,
                "Old gen used should decrease after freeing half the objects"
            );

            // Should be exactly 1 free block (fully compacted)
            assert_eq!(old_gen.free_block_count(), 1);

            // The single free block should be large enough for a new large alloc
            assert!(
                old_gen.largest_free_block() >= HEADER_SIZE + 4 * SLOT_SIZE,
                "Compacted free block should be large enough for a 4-field object"
            );
        }

        // Verify surviving objects have correct values (0, 2, 4, 6)
        for (i, root) in roots.iter().enumerate() {
            let expected = (i * 2) as i32;
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(expected));
        }
    }

    #[test]
    fn s26_compact_pointer_map_in_gc_result() {
        // Verify that compaction pointer_map flows through collect_garbage()
        // so the VM can update external roots.
        let heap = GenerationalHeap::with_sizes(2 * 1024, 2 * 1024);
        let monitors = NoOpMonitors;

        // Fill old gen to >75% to trigger major GC during minor GC
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..10 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        // Promote to old gen
        for _ in 0..PROMOTION_AGE {
            let result = heap.collect_garbage(&stw(), &mut roots, &monitors);
            // Update roots from minor GC pointer_map
            for root in &mut roots {
                if let Some(&new_addr) = result.pointer_map.get(&(root.as_ptr() as usize)) {
                    // SAFETY: `new_addr` comes from the GC pointer map, pointing to a valid relocated object.
                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }

        // Drop most references — keep only 2 objects
        roots = vec![roots[0], roots[5]];

        // Allocate more to trigger minor GC which should cascade into major GC
        for _ in 0..20 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            // Don't keep reference — these are garbage
            let _ = obj;
        }

        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        // The pointer_map should contain entries (from minor GC and/or major GC compaction)
        // Surviving objects should still be accessible
        for root in &roots {
            let val = heap.get_field(*root, 0);
            assert!(
                val.as_int().is_some(),
                "Root should have valid Int field after compaction"
            );
        }
    }

    #[test]
    fn s26_compact_no_move_when_contiguous() {
        // If live objects are already contiguous, compaction should be a no-op
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..3 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        // Promote all to old gen
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        // All objects are contiguous (no gaps) — compaction should not move anything
        let ptrs_before: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);
            assert!(
                compact_map.is_empty(),
                "No objects should move when they are already contiguous"
            );
        }

        // Pointers should be unchanged
        let ptrs_after: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(ptrs_before, ptrs_after);
    }

    #[test]
    fn s26_compact_forwarding_ptrs_cleared() {
        // After compaction, no objects should have forwarding pointers set
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..4 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }

        // Free some, then compact
        roots = vec![roots[0], roots[2]];
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // Verify no forwarding pointers remain set
            for (obj_ptr, _) in old_gen.walk_objects() {
                // SAFETY: `obj_ptr` is from `old_gen.walk_objects()`, pointing to a valid old-gen object header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                assert!(
                    header.forwarding_ptr.is_null(),
                    "Forwarding pointer should be cleared after compaction"
                );
                assert_eq!(
                    header.gc_flags & GC_FLAG_MARKED,
                    0,
                    "Mark bit should be cleared after compaction"
                );
            }
        }
    }

    // =========================================================================
    // S29: GC Write Barrier Verification
    // =========================================================================

    /// Helper: promote an object to old gen by surviving PROMOTION_AGE minor GCs.
    fn promote_to_old(heap: &GenerationalHeap, obj: ObjectRef) -> ObjectRef {
        let monitors = NoOpMonitors;
        let mut roots = vec![obj];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        assert!(heap.is_in_old(roots[0].as_ptr()), "object should be promoted to old gen");
        roots[0]
    }

    #[test]
    fn s29_random_graph_gc_never_collects_reachable() {
        // Property-based: build random object graph, GC, verify all reachable objects survive
        // with correct field values.
        let heap = GenerationalHeap::with_sizes(32 * 1024, 64 * 1024);
        let monitors = NoOpMonitors;

        // Allocate 50 objects, each tagged with its index
        let n = 50;
        let mut objs: Vec<ObjectRef> = Vec::new();
        for i in 0..n {
            let obj = heap.alloc_object(ClassId::new(0), 2); // field 0 = tag, field 1 = link
            heap.set_field(obj, 0, Value::Int(i as i32));
            heap.set_field(obj, 1, Value::Object(None));
            objs.push(obj);
        }

        // Build random links: obj[i].field[1] = obj[(i*7+3) % n] (deterministic pseudo-random)
        for i in 0..n {
            let target_idx = (i * 7 + 3) % n;
            heap.set_field(objs[i], 1, Value::Object(Some(objs[target_idx])));
        }

        // Use first 10 as roots
        let mut roots: Vec<ObjectRef> = objs[..10].to_vec();

        // Compute reachable set from roots via BFS
        let mut reachable: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        for i in 0..10 {
            reachable.insert(i);
            queue.push_back(i);
        }
        while let Some(idx) = queue.pop_front() {
            let target_idx = (idx * 7 + 3) % n;
            if reachable.insert(target_idx) {
                queue.push_back(target_idx);
            }
        }

        // Run GC
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        // Verify all roots survived and their tags are correct
        for (i, root) in roots.iter().enumerate() {
            let tag = heap.get_field(*root, 0);
            assert_eq!(tag.as_int(), Some(i as i32),
                "S29: root {} tag should be {} after GC", i, i);
        }

        // Walk reachable graph from roots and verify all tags
        let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut walk_queue: std::collections::VecDeque<ObjectRef> = std::collections::VecDeque::new();
        for r in &roots {
            walk_queue.push_back(*r);
        }
        while let Some(obj) = walk_queue.pop_front() {
            let tag = heap.get_field(obj, 0).as_int().unwrap() as usize;
            if !visited.insert(tag) { continue; }
            // Verify tag is in our expected reachable set
            assert!(reachable.contains(&tag),
                "S29: object with tag {} should be reachable", tag);
            // Follow link
            if let Value::Object(Some(next)) = heap.get_field(obj, 1) {
                walk_queue.push_back(next);
            }
        }
        // All reachable objects should have been visited
        assert_eq!(visited.len(), reachable.len(),
            "S29: all {} reachable objects should survive GC, found {}", reachable.len(), visited.len());
    }

    #[test]
    fn s29_cross_gen_old_to_young_chain() {
        // Test old→young reference chain: old object points to young, GC must preserve young.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Create and promote root to old gen
        let root = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(root, 0, Value::Int(100));
        let promoted = promote_to_old(&heap, root);

        // Create chain of young objects: y1 → y2 → y3
        let y1 = heap.alloc_object(ClassId::new(0), 2);
        let y2 = heap.alloc_object(ClassId::new(0), 2);
        let y3 = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(y1, 0, Value::Int(1));
        heap.set_field(y2, 0, Value::Int(2));
        heap.set_field(y3, 0, Value::Int(3));
        heap.set_field(y1, 1, Value::Object(Some(y2)));
        heap.set_field(y2, 1, Value::Object(Some(y3)));

        // Link old → y1
        heap.set_field(promoted, 1, Value::Object(Some(y1)));
        heap.write_barrier(promoted, Value::Object(Some(y1)));

        // GC with only old object as root — young chain must survive via dirty card
        let mut roots = vec![promoted];
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        // Walk chain and verify all tags
        let old_after = roots[0];
        assert_eq!(heap.get_field(old_after, 0).as_int(), Some(100));
        let y1_after = match heap.get_field(old_after, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: old→young link broken after GC"),
        };
        assert_eq!(heap.get_field(y1_after, 0).as_int(), Some(1));
        let y2_after = match heap.get_field(y1_after, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: y1→y2 link broken after GC"),
        };
        assert_eq!(heap.get_field(y2_after, 0).as_int(), Some(2));
        let y3_after = match heap.get_field(y2_after, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: y2→y3 link broken after GC"),
        };
        assert_eq!(heap.get_field(y3_after, 0).as_int(), Some(3));
    }

    #[test]
    fn s29_cross_gen_young_to_old_survives() {
        // Young object references old object — young must be collected if unreachable,
        // old must survive independently.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Promote an object to old gen
        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(old_obj, 0, Value::Int(42));
        let promoted = promote_to_old(&heap, old_obj);

        // Create young object pointing to old
        let young = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(young, 0, Value::Int(7));
        heap.set_field(young, 1, Value::Object(Some(promoted)));

        // Both as roots — both should survive
        let mut roots = vec![promoted, young];
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        let old_after = roots[0];
        let young_after = roots[1];
        assert_eq!(heap.get_field(old_after, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(young_after, 0).as_int(), Some(7));
        // Young's reference to old should be updated
        match heap.get_field(young_after, 1) {
            Value::Object(Some(r)) => assert_eq!(
                heap.get_field(r, 0).as_int(), Some(42),
                "S29: young→old reference should point to correct old object"
            ),
            _ => panic!("S29: young→old link broken after GC"),
        }
    }

    #[test]
    fn s29_card_table_multiple_dirty_cards() {
        // Multiple old objects on different cards all pointing to young — all must be preserved.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 64 * 1024);
        let monitors = NoOpMonitors;

        // Promote 5 objects — spread across old gen (different cards if possible)
        let mut old_objs = Vec::new();
        for i in 0..5 {
            let obj = heap.alloc_object(ClassId::new(0), 2);
            heap.set_field(obj, 0, Value::Int(i * 100));
            old_objs.push(obj);
        }
        let mut roots: Vec<ObjectRef> = old_objs.clone();
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        // All should be in old gen now
        for r in &roots {
            assert!(heap.is_in_old(r.as_ptr()), "S29: object should be promoted");
        }

        // Create 5 young objects, each referenced by one old object
        let mut young_objs = Vec::new();
        for i in 0..5 {
            let y = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(y, 0, Value::Int(i * 10 + 1));
            young_objs.push(y);
            heap.set_field(roots[i as usize], 1, Value::Object(Some(y)));
            heap.write_barrier(roots[i as usize], Value::Object(Some(y)));
        }

        // GC with only old objects as roots
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        // Verify all old→young links preserved
        for (i, old) in roots.iter().enumerate() {
            match heap.get_field(*old, 1) {
                Value::Object(Some(y)) => {
                    assert_eq!(heap.get_field(y, 0).as_int(), Some(i as i32 * 10 + 1),
                        "S29: old[{}]→young tag should be {}", i, i * 10 + 1);
                }
                _ => panic!("S29: old[{}]→young link broken after GC", i),
            }
        }
    }

    #[test]
    fn s29_card_table_overwrite_still_marks_new_target() {
        // Overwrite an old→young ref with a different young ref, verify new target preserved.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(1));
        let promoted = promote_to_old(&heap, obj);

        // First young target
        let y1 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y1, 0, Value::Int(10));
        heap.set_field(promoted, 1, Value::Object(Some(y1)));
        heap.write_barrier(promoted, Value::Object(Some(y1)));

        // Overwrite with different young target
        let y2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y2, 0, Value::Int(20));
        heap.set_field(promoted, 1, Value::Object(Some(y2)));
        heap.write_barrier(promoted, Value::Object(Some(y2)));

        // GC: y1 is unreachable, y2 reachable via old
        let mut roots = vec![promoted];
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        match heap.get_field(roots[0], 1) {
            Value::Object(Some(y)) => {
                assert_eq!(heap.get_field(y, 0).as_int(), Some(20),
                    "S29: overwritten ref should point to y2 (tag=20)");
            }
            _ => panic!("S29: old→young link broken after overwrite + GC"),
        }
    }

    #[test]
    fn s29_stress_random_link_unlink_gc_cycles() {
        // Stress test: all objects are roots so GC updates all refs. Random link/unlink per cycle.
        let heap = GenerationalHeap::with_sizes(64 * 1024, 128 * 1024);
        let monitors = NoOpMonitors;

        let num_objects = 80;
        let num_cycles = 20;

        // All objects are roots — GC will update all of them
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..num_objects {
            let obj = heap.alloc_object(ClassId::new(0), 2); // field 0=tag, field 1=link
            heap.set_field(obj, 0, Value::Int(i as i32));
            roots.push(obj);
        }

        for cycle in 0..num_cycles {
            // Deterministic pseudo-random linking among roots
            let n = roots.len();
            for i in 0..n {
                let target_idx = (i * 13 + cycle * 7 + 5) % n;
                heap.set_field(roots[i], 1, Value::Object(Some(roots[target_idx])));
            }

            // Unlink every 3rd
            for i in (0..n).step_by(3) {
                heap.set_field(roots[i], 1, Value::Object(None));
            }

            // Run GC — all objects are roots, so all survive and get updated
            heap.collect_garbage(&stw(), &mut roots, &monitors);

            // Verify tags
            for (i, root) in roots.iter().enumerate() {
                let tag = heap.get_field(*root, 0).as_int().unwrap();
                assert_eq!(tag, i as i32,
                    "S29 cycle {}: root {} tag corrupted (got {})", cycle, i, tag);
            }

            // Verify linked objects are reachable with correct tags
            for i in 0..n {
                if i % 3 == 0 { continue; } // unlinked
                let target_idx = (i * 13 + cycle * 7 + 5) % n;
                match heap.get_field(roots[i], 1) {
                    Value::Object(Some(linked)) => {
                        let linked_tag = heap.get_field(linked, 0).as_int().unwrap();
                        assert_eq!(linked_tag, target_idx as i32,
                            "S29 cycle {}: obj[{}] link tag should be {}, got {}",
                            cycle, i, target_idx, linked_tag);
                    }
                    _ => panic!("S29 cycle {}: obj[{}] link broken", cycle, i),
                }
            }
        }
    }

    #[test]
    fn s29_cross_gen_old_to_young_ref_array() {
        // Old object has a reference array pointing to young objects — verify write barrier
        // preserves all young objects through GC.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Promote a reference array to old gen
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 4);
        let promoted_arr = promote_to_old(&heap, arr);

        // Create young objects and store in the old array
        let mut young_tags = Vec::new();
        for i in 0..4 {
            let y = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(y, 0, Value::Int(i * 11));
            young_tags.push(i * 11);
            heap.set_array_element(promoted_arr, i as usize, Value::Object(Some(y))).unwrap();
            heap.write_barrier(promoted_arr, Value::Object(Some(y)));
        }

        // GC with only old array as root
        let mut roots = vec![promoted_arr];
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        let arr_after = roots[0];
        for i in 0..4 {
            match heap.get_array_element(arr_after, i as usize).unwrap() {
                Value::Object(Some(y)) => {
                    assert_eq!(heap.get_field(y, 0).as_int(), Some(young_tags[i as usize]),
                        "S29: ref array[{}] young tag should be {}", i, young_tags[i as usize]);
                }
                _ => panic!("S29: ref array[{}] lost after GC", i),
            }
        }
    }

    #[test]
    fn s29_write_barrier_no_false_positives() {
        // Write barrier should NOT mark card for: young→young, old→old, non-reference stores.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Promote two objects to old gen
        let o1 = heap.alloc_object(ClassId::new(0), 2);
        let o2 = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![o1, o2];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        let old1 = roots[0];
        let old2 = roots[1];
        assert!(heap.is_in_old(old1.as_ptr()));
        assert!(heap.is_in_old(old2.as_ptr()));

        // Clear card table.
        //
        // T5.5.2 (HIGH-1 fix): the write barrier batches into a
        // per-thread buffer; observing the bitmap requires draining
        // the buffer first. Each scenario below flushes + drains
        // before checking `take_dirty_cards()` so the assertion sees
        // the post-barrier state, exactly as the collector would at
        // GC entry.
        heap.card_table.clear_all();

        // Old → Old: should NOT dirty card
        heap.set_field(old1, 1, Value::Object(Some(old2)));
        heap.write_barrier(old1, Value::Object(Some(old2)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(heap.card_table.take_dirty_cards().is_empty(),
            "S29: old→old should not dirty card");

        // Young → Young: should NOT dirty card
        let y1 = heap.alloc_object(ClassId::new(0), 2);
        let y2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y1, 1, Value::Object(Some(y2)));
        heap.write_barrier(y1, Value::Object(Some(y2)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(heap.card_table.take_dirty_cards().is_empty(),
            "S29: young→young should not dirty card");

        // Non-reference store: should NOT dirty card
        heap.set_field(old1, 0, Value::Int(999));
        heap.write_barrier(old1, Value::Int(999));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(heap.card_table.take_dirty_cards().is_empty(),
            "S29: non-ref store should not dirty card");

        // Old → Young: SHOULD dirty card
        heap.set_field(old1, 1, Value::Object(Some(y1)));
        heap.write_barrier(old1, Value::Object(Some(y1)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(!heap.card_table.take_dirty_cards().is_empty(),
            "S29: old→young SHOULD dirty card");
    }

    #[test]
    fn s29_gc_cycle_with_promotion_and_cross_gen_refs() {
        // Complex scenario: objects promoted over multiple GC cycles while maintaining
        // cross-generational references.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 64 * 1024);
        let monitors = NoOpMonitors;

        // Create chain: A → B → C → D
        let a = heap.alloc_object(ClassId::new(0), 2);
        let b = heap.alloc_object(ClassId::new(0), 2);
        let c = heap.alloc_object(ClassId::new(0), 2);
        let d = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(a, 0, Value::Int(1));
        heap.set_field(b, 0, Value::Int(2));
        heap.set_field(c, 0, Value::Int(3));
        heap.set_field(d, 0, Value::Int(4));
        heap.set_field(a, 1, Value::Object(Some(b)));
        heap.set_field(b, 1, Value::Object(Some(c)));
        heap.set_field(c, 1, Value::Object(Some(d)));

        let mut roots = vec![a];

        // Run PROMOTION_AGE GCs — all should be promoted
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        let a_old = roots[0];
        assert!(heap.is_in_old(a_old.as_ptr()), "S29: A should be in old gen");

        // Verify entire chain survives promotion
        let b_old = match heap.get_field(a_old, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: A→B link lost during promotion"),
        };
        assert_eq!(heap.get_field(b_old, 0).as_int(), Some(2));

        let c_old = match heap.get_field(b_old, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: B→C link lost during promotion"),
        };
        assert_eq!(heap.get_field(c_old, 0).as_int(), Some(3));

        let d_old = match heap.get_field(c_old, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: C→D link lost during promotion"),
        };
        assert_eq!(heap.get_field(d_old, 0).as_int(), Some(4));

        // Now add a new young object hanging off the old chain
        let e = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(e, 0, Value::Int(5));
        heap.set_field(d_old, 1, Value::Object(Some(e)));
        heap.write_barrier(d_old, Value::Object(Some(e)));

        // GC again — e should survive via dirty card
        heap.collect_garbage(&stw(), &mut roots, &monitors);
        let a_final = roots[0];
        // Walk full chain: A→B→C→D→E
        let mut current = a_final;
        for expected_tag in [1, 2, 3, 4] {
            assert_eq!(heap.get_field(current, 0).as_int(), Some(expected_tag));
            current = match heap.get_field(current, 1) {
                Value::Object(Some(r)) => r,
                _ => panic!("S29: chain broken at tag={}", expected_tag),
            };
        }
        assert_eq!(heap.get_field(current, 0).as_int(), Some(5),
            "S29: E (young, linked from old D) should survive");
    }

    #[test]
    fn s29_stress_1000_objects_random_graph_gc() {
        // Large stress test: 1000 objects, random links, multiple GC cycles.
        let heap = GenerationalHeap::with_sizes(256 * 1024, 512 * 1024);
        let monitors = NoOpMonitors;

        let n = 1000;
        let mut objs: Vec<ObjectRef> = Vec::new();
        for i in 0..n {
            let obj = heap.alloc_object(ClassId::new(0), 2);
            heap.set_field(obj, 0, Value::Int(i as i32));
            objs.push(obj);
        }

        // Build deterministic random graph
        for i in 0..n {
            let target = (i * 37 + 17) % n;
            heap.set_field(objs[i], 1, Value::Object(Some(objs[target])));
        }

        // All objects are roots initially
        let mut roots = objs.clone();

        // GC cycle 1: everything should survive
        heap.collect_garbage(&stw(), &mut roots, &monitors);
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32),
                "S29 stress: object {} tag corrupted after GC1", i);
        }

        // Drop half the roots — only first 500
        roots.truncate(500);

        // GC cycle 2
        heap.collect_garbage(&stw(), &mut roots, &monitors);
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32),
                "S29 stress: root {} tag corrupted after GC2", i);
        }

        // GC cycles 3-5: repeatedly compact
        for cycle in 3..=5 {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
            for (i, root) in roots.iter().enumerate() {
                assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32),
                    "S29 stress: root {} tag corrupted after GC{}", i, cycle);
            }
        }
    }

    #[test]
    fn s29_barrier_correctness_promoted_then_linked() {
        // Edge case: object promoted during GC, then immediately linked to a young object.
        // Next GC must see the cross-gen ref via write barrier.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(42));
        let mut roots = vec![obj];

        // Promote object
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        // Allocate young, link from old, and immediately GC
        let young = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(young, 0, Value::Int(99));
        heap.set_field(promoted, 1, Value::Object(Some(young)));
        heap.write_barrier(promoted, Value::Object(Some(young)));

        // GC: young object's only root is via old object + write barrier
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        let after = roots[0];
        match heap.get_field(after, 1) {
            Value::Object(Some(y)) => {
                assert_eq!(heap.get_field(y, 0).as_int(), Some(99),
                    "S29: young object linked from just-promoted old should survive");
            }
            _ => panic!("S29: old→young link lost after promotion+barrier+GC"),
        }
    }
}
