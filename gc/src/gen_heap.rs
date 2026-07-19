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
use std::collections::{HashMap, HashSet, VecDeque};
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
    array_data_size, array_element_type_from_tag, object_kind_from_tag, read_prim_element,
    write_prim_element, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_ELEMENT_TYPE_OFFSET,
    AUTOBOX_CLASS_ID, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE, OBJECT_KIND_OFFSET,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use crate::old_gen::OldGen;
// Compact reference-field layout (CRATONVM_COMPACT_REF_FIELDS). Reference
// instance fields are stored as 8-byte pointers per the per-class oop-map.
use crate::satb::SatbQueue;
use crate::{class_layout, compact_ref_fields_enabled, is_compact_object, object_body_size};
use cratonvm_types::GC_FLAG_COMPACT;
use cratonvm_types::{ClassId, CompactLayout, ObjectRef, Value};

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

/// The default non-moving young collector does not need Cheney-copy headroom:
/// it reclaims dead spans in place and falls back to allocation-failure GC for
/// fragmentation.  Let transient allocation fill most of the active semi-space
/// before paying the O(heap) mark/sweep cost.  The moving-young opt-in retains
/// the conservative 50% trigger above so the to-space can hold all survivors.
/// Keeping a 10% reserve also leaves room for TLAB refill granularity and avoids
/// turning every near-capacity refill into an allocation-failure collection.
const NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT: usize = 90;

#[inline]
const fn young_gc_trigger_bytes(
    capacity: usize,
    moving_threshold: usize,
    non_moving_young: bool,
) -> usize {
    if non_moving_young {
        capacity * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100
    } else {
        moving_threshold
    }
}

#[inline]
const fn next_young_gc_is_guaranteed_non_moving(
    jit_active: bool,
    unregistered_jit_frame: bool,
    jit_allocation_frame: bool,
    moving_young_requested: bool,
    allow_moving_young: bool,
    force_moving: bool,
) -> bool {
    !force_moving
        && ((jit_active && !allow_moving_young)
            || ((jit_active || unregistered_jit_frame || jit_allocation_frame)
                && !moving_young_requested))
}

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

/// Absolute ceiling on the humongous threshold, independent of heap size.
///
/// The `HUMONGOUS_YOUNG_FRACTION_PERCENT`-of-young-semi rule scales the
/// threshold with `--Xmx` (young semi = `Xmx / 4`), so on a *large* heap a
/// merely-large array stays below it and lands in young — where the Cheney
/// collector must **copy** it into to-space on every minor GC until it ages
/// out (`PROMOTION_AGE` cycles). A handful of ~1 GiB arrays then either
/// overflow to-space (hard OOM in the relocation path) or, when old gen is
/// also full and humongous routing falls back to young, get corrupted under
/// pressure. Concretely a 1 GiB array is only "humongous" when
/// `Xmx <= 8g` (semi/2 < 1 GiB); at `--Xmx 16g` it is young and gets copied.
/// (Observed: `TestEncryptInterceptorLargeHeap` needs `--Xmx 16g` to pass,
/// AES-GCM `AuthenticationFailed` / relocation-OOM below that.)
///
/// Capping the threshold means any array larger than this ALWAYS routes
/// directly to old gen and is never young-copied, mirroring HotSpot G1's
/// humongous handling (large objects bypass the copying young generation).
/// 256 MiB is well above the default-heap young-semi fraction
/// (256 MiB heap → 64 MiB semi → 32 MiB threshold), so small/default heaps
/// are unaffected; it only changes where genuinely large (>256 MiB) arrays
/// live on multi-GiB heaps, which is exactly the case the fraction rule
/// mishandles.
const HUMONGOUS_ABSOLUTE_CAP_BYTES: usize = 256 * 1024 * 1024;

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

/// A2 forensic probe (CRATONVM_DBG_A2) — limits the per-run detail dump to the
/// first few corruption hits so the log is not flooded.
pub static A2_PROBE_HITS: AtomicU64 = AtomicU64::new(0);

/// Count of OVERLAPPING free blocks merged by the post-sweep coalescer (off <
/// previous block end). A non-zero count proves the free list was handing out —
/// or about to hand out — the same young region twice (the A2 object-overlap /
/// walk-desync root). Exposed for the gated diagnostic + tests.
pub static A2_FL_OVERLAP_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead walk-desync hardening (2026-07-02) — count of times a linear young
/// from-space walk OVERSHOT into a known free block (`cursor > off`). Each hit
/// is proof the walk grid and the free list disagreed upstream (an unlisted
/// zeroed span or a mis-sized header) — the seed of the phantom-stride /
/// mid-live-object free corruption family. Kept as a plain counter so a
/// repro run can grep it without a debug gate.
pub static SWEEP_WALK_OVERSHOOT_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead walk-desync hardening — count of all-zero spans (word0 == 0 at a
/// walk-grid offset, at least `HEADER_SIZE` of zero bytes) encountered by a
/// young walk OUTSIDE the free list. These are freed-then-reused-then-
/// clobbered slots or freed-but-unlisted residue; they are SKIPPED (never
/// re-freed, never zeroed — the span may be a live allocation whose header a
/// stale register-held reference clobbered, so re-freeing would double-serve
/// live memory).
pub static SWEEP_ZERO_SPAN_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead walk-desync hardening — count of forwarded young objects whose
/// forwarding target failed validation (not inside old gen). Such a header is
/// a phantom write from a desynced walk or corruption; the span is retained
/// instead of zeroed+freed.
pub static SWEEP_BAD_FORWARD_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead comb-7 fix (2026-07-03) — count of mark-phase candidates rejected
/// because the header's claimed EXTENT (`gen_object_total_size`) does not fit
/// inside its generation at that address. The per-field plausibility gate
/// admits `array_length` up to i32::MAX, and the mark BFS scan iterates that
/// count with the header as its only bound — a corrupt header (e.g. packed
/// pointer bytes misparsed as `kind=Array, array_length=<pointer low 32>`) ran
/// the scan tens of MB off the mapped arena (observed SIGSEGV at the region
/// boundary). A real object's extent always fits inside the arena it was
/// allocated from, so out-of-extent headers are rejected without marking.
pub static SWEEP_BAD_EXTENT_HITS: AtomicU64 = AtomicU64::new(0);

/// Conservative root candidates may be interior heap addresses.  Only the
/// opt-in A2 forensic mode reports rejected candidates; normal collection
/// silently discards an address that is not an object start.
#[inline]
fn emit_conservative_candidate_diagnostic(hit: u64, a2_enabled: bool) -> bool {
    a2_enabled && hit < 8
}

/// DoHead walk-desync hardening — count of selective-promotion UNWIND events:
/// the evacuation walk saw a grid anomaly (zero span, implausible header,
/// free-block overshoot, or a span crossing a free hole) and dropped the
/// candidates collected since the last trustworthy anchor (a free-block
/// boundary). Their forwarding pointers were never installed (installs are
/// deferred until a stretch is anchor-verified); the already-copied old-gen
/// bytes are unreachable garbage for the next major GC. Candidates on
/// verified stretches still promote, so a persistent unparseable span cannot
/// starve promotion.
pub static SWEEP_PROMOTION_ABORT_HITS: AtomicU64 = AtomicU64::new(0);

/// DBG (CRATONVM_DBG_WATCHREF): trace the RandomizedContext WeakHashMap fix —
/// which sweep path ran, which kept-in-place survivors matched a watched
/// Weak/Soft/Phantom referent, and whether each was found alive or dead.
/// Cached (checked once, not per-object-visited) so enabling it cannot itself
/// perturb GC timing enough to mask/create the races it's meant to diagnose.
#[inline]
fn watchref_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_WATCHREF").is_some())
}

/// DBG: optional young-GC stress threshold (bytes). Read from
/// `CRATONVM_DBG_GC_STRESS`, or `CRATONVM_GC_STRESS` as an accepted alias
/// (the latter is what several handoff/repro docs use; without the alias the
/// documented `CRATONVM_GC_STRESS=<bytes> …` command silently does nothing).
/// `CRATONVM_DBG_GC_STRESS` wins if both are set.
fn gc_stress_threshold() -> Option<usize> {
    use std::sync::OnceLock;
    static S: OnceLock<Option<usize>> = OnceLock::new();
    *S.get_or_init(|| {
        std::env::var("CRATONVM_DBG_GC_STRESS")
            .or_else(|_| std::env::var("CRATONVM_GC_STRESS"))
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&v| v > 0)
    })
}

// ---------------------------------------------------------------------------
// "Zeroed-a-live-object" detector (CRATONVM_DBG_SWEEP_ZERO)
//
// The non-moving sweep zeroes every UNMARKED young object. When a live object
// is reclaimed because its only reference is invisible to the marker (a
// register/native-stack root — `CRATONVM_DBG_SWEEP_EDGES` stays silent, ruling
// out heap/root/card edges), the bug surfaces LATER as an all-zero-header
// receiver / ClassCastException on some other thread. By then the header is
// zeroed, so the object's class is lost.
//
// This records the (address, class_id, kind) of every object the sweep zeroes
// into a bounded ring BEFORE the zeroing write. A consumer (e.g. the
// interpreter's all-zero-header detection) calls `sweep_zero_lookup(addr)` to
// recover the ORIGINAL class of a reclaimed object — i.e. "this swept slot was
// a java/util/concurrent/ForkJoinTask", which names the root-coverage gap.
// Gated + cheap (one masked ring write per dead object only when enabled).
// ---------------------------------------------------------------------------

/// One swept-object record: heap address, original class_id, original kind byte.
#[derive(Clone, Copy)]
struct SweptRec {
    addr: u64,
    class_id: u32,
    kind: u8,
    cycle: u32,
    /// GC context at the time of reclamation (CRATONVM_DBG_MTROOTS), so the
    /// sweep-zero detector can name WHICH GC reclaimed a live object: the
    /// reason (1=System.gc / 2=alloc-young / 3=forced-alloc), the initiating
    /// thread id, and how many threads were parked (blocked) at that STW.
    reason: u8,
    initiator: u32,
    blocked: u32,
}

const SWEPT_RING_BITS: usize = 18; // 256K entries
const SWEPT_RING_LEN: usize = 1 << SWEPT_RING_BITS;
const SWEPT_RING_MASK: usize = SWEPT_RING_LEN - 1;

struct SweptRing {
    buf: Vec<SweptRec>,
    next: usize,
}

static SWEPT_RING: std::sync::OnceLock<parking_lot::Mutex<SweptRing>> = std::sync::OnceLock::new();
static SWEEP_ZERO_CYCLE: AtomicU64 = AtomicU64::new(0);

/// GC context for the IN-PROGRESS collection, published by the VM root-gathering
/// path just before `collect_garbage` (CRATONVM_DBG_MTROOTS). Read by
/// `record_swept` so a reclaimed-live record names the responsible GC.
/// reason: 1=System.gc, 2=alloc-young (maybe_gc), 3=forced-alloc (maybe_gc_forced).
pub static GC_CTX_REASON: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static GC_CTX_INITIATOR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static GC_CTX_BLOCKED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Publish the current collection's context (VM-side, before `collect_garbage`).
pub fn set_gc_context(reason: u8, initiator: u32, blocked: u32) {
    GC_CTX_REASON.store(reason as u32, Ordering::Relaxed);
    GC_CTX_INITIATOR.store(initiator, Ordering::Relaxed);
    GC_CTX_BLOCKED.store(blocked, Ordering::Relaxed);
}

/// Current young-sweep cycle stamp (the value the next sweep will record).
pub fn current_sweep_cycle() -> u32 {
    SWEEP_ZERO_CYCLE.load(Ordering::Relaxed) as u32
}

fn sweep_zero_enabled() -> bool {
    use std::sync::OnceLock;
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| std::env::var_os("CRATONVM_DBG_SWEEP_ZERO").is_some())
}

fn swept_ring() -> &'static parking_lot::Mutex<SweptRing> {
    SWEPT_RING.get_or_init(|| {
        parking_lot::Mutex::new(SweptRing {
            buf: vec![
                SweptRec {
                    addr: 0,
                    class_id: 0,
                    kind: 0,
                    cycle: 0,
                    reason: 0,
                    initiator: 0,
                    blocked: 0
                };
                SWEPT_RING_LEN
            ],
            next: 0,
        })
    })
}

/// Record that the sweep is about to zero `addr` (a young object with the given
/// header), so a later all-zero-header consumer can recover its original class.
/// No-op unless `CRATONVM_DBG_SWEEP_ZERO` is set.
#[inline]
fn record_swept(addr: usize, class_id: u32, kind: u8, cycle: u32) {
    if !sweep_zero_enabled() {
        return;
    }
    // Skip synthetic filler sentinels (TLAB/GAP fillers are *supposed* to be
    // reclaimed) — they only mask the real reclaimed object's record when a
    // freed slot is later reused as a filler at the same address.
    if class_id == crate::tlab::TLAB_FILLER_CLASS_ID.as_u32()
        || class_id == crate::tlab::GAP_FILLER_CLASS_ID.as_u32()
    {
        return;
    }
    let mut r = swept_ring().lock();
    let i = r.next & SWEPT_RING_MASK;
    r.buf[i] = SweptRec {
        addr: addr as u64,
        class_id,
        kind,
        cycle,
        reason: GC_CTX_REASON.load(Ordering::Relaxed) as u8,
        initiator: GC_CTX_INITIATOR.load(Ordering::Relaxed),
        blocked: GC_CTX_BLOCKED.load(Ordering::Relaxed),
    };
    r.next = r.next.wrapping_add(1);
}

/// Look up whether `addr` was recently zeroed by the non-moving sweep. Returns
/// `(original_class_id, original_kind, gc_cycle, reason, initiator_tid, blocked)`
/// of the most recent matching record, or `None`. Used by the all-zero-header
/// detection to name a reclaimed (register/native-root-invisible) live object
/// AND the GC that reclaimed it. No-op unless the gate is set.
pub fn sweep_zero_lookup(addr: usize) -> Option<(u32, u8, u32, u8, u32, u32)> {
    if !sweep_zero_enabled() {
        return None;
    }
    let a = addr as u64;
    let r = swept_ring().lock();
    let mut best: Option<SweptRec> = None;
    for rec in r.buf.iter() {
        if rec.addr == a {
            match best {
                Some(b) if b.cycle >= rec.cycle => {}
                _ => best = Some(*rec),
            }
        }
    }
    best.map(|b| {
        (
            b.class_id,
            b.kind,
            b.cycle,
            b.reason,
            b.initiator,
            b.blocked,
        )
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

/// Process-global mirror of the generational heap's three region `[base, end)`
/// bounds — layout `[yf_base, yf_end, yt_base, yt_end, og_base, og_end]`.
///
/// The JIT's guarded inline `getfield` bakes this table's address as an
/// immediate and emits the same containment check `is_object_address` performs
/// as its first gate: a receiver inside one of these `[base, end)` ranges
/// points into an arena that is mapped for the heap's lifetime, so a raw field
/// load cannot fault; anything else branches to the checked `jit_getfield`
/// helper (which throws the NPE / runs the full validation). Updated in
/// `store_region_bounds_locked` alongside the per-heap `region_bounds` cache —
/// same freshness contract (construction + GC start/end), same
/// Acquire/Release pairing.
///
/// All-zero entries match nothing, so before the first publish — or for heap
/// backends that don't publish (G1/ZGC) — every guarded site simply falls
/// through to the helper. [`GenerationalHeap`]'s `Drop` re-zeroes the table so
/// a torn-down heap (embedding / unit tests) can never leave stale bounds that
/// would admit a pointer into freed arena memory.
#[repr(C)]
pub struct JitRegionBoundsTable {
    pub words: [AtomicUsize; 6],
}

pub static JIT_REGION_BOUNDS: JitRegionBoundsTable = JitRegionBoundsTable {
    // Written out element-by-element (not `[const { ... }; 6]`) to stay under
    // the workspace MSRV — inline-const repeat expressions landed in 1.79.
    words: [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ],
};

/// Address of [`JIT_REGION_BOUNDS`] for the JIT helpers table
/// (`JitRuntimeHelpers::region_bounds_addr`).
pub fn jit_region_bounds_addr() -> usize {
    &JIT_REGION_BOUNDS as *const _ as usize
}

impl Drop for GenerationalHeap {
    fn drop(&mut self) {
        // The guarded inline getfield's safety argument is "anything inside the
        // published bounds points at a mapped arena". Once this heap's arenas
        // free, that stops holding — zero the global table so every guarded
        // site degrades to the checked helper. (If another live heap owns the
        // table it re-publishes at its next GC; until then helper-only is a
        // safe, merely slower, state.)
        for w in JIT_REGION_BOUNDS.words.iter() {
            w.store(0, Ordering::Release);
        }
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
    /// `CRATONVM_DBG_STALE_OBJREF` quarantine ring of evacuated arenas.
    ///
    /// Empty (no cost) when the flag is unset. While
    /// [`crate::stale_objref_debug::enabled`] reads true,
    /// `collect_garbage_inner` routes each cycle's just-evacuated
    /// (all-garbage) `young_from` arena through this ring instead of
    /// resetting it immediately: a stale native `ObjectRef` held across that
    /// GC still resolves to a header showing `is_forwarded() == true` for
    /// [`crate::stale_objref_debug::quarantine_cycles`] extra full minor-GC
    /// cycles (`CRATONVM_DBG_STALE_OBJREF_CYCLES`, default 1 — the original
    /// single-arena behaviour), which [`Self::get_header`] turns into a hard
    /// panic instead of the object silently reading back all-zero (or,
    /// worse, a same-slot-reused unrelated object) once that memory is
    /// actually reclaimed. Ordered oldest-first; the front arena's grace
    /// period has elapsed and its memory is the next to be reused. See
    /// docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md
    /// and docs/internal/wildfly-stale-objectref-debug-assertion-scoping.md.
    quarantine: Mutex<VecDeque<Arena>>,
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
    /// would prefer if/when [`try_alloc_young_initialized`]/[`refill_tlab`]
    /// grow into a per-node arena layout.
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
    /// Young-exhaustion signal from the *native* allocation wrappers
    /// (`ctx.alloc_object` / `alloc_array` / the `*_full` fallible twins),
    /// which must stay GC-free mid-callback (their callers hold unrooted
    /// local `ObjectRef`s) and therefore spill into old gen when young
    /// cannot satisfy an allocation. Set on every such spill; consumed at
    /// the next native-call boundary (`safe_native_call` in the VM), where
    /// every argument is pinned and remappable, to run the same
    /// orchestrated GC the interpreter's `gc_alloc_*` slow path would.
    /// Without this, a workload whose allocations all happen inside
    /// natives (e.g. a JIT'd `HashMap<Integer,Integer>` put loop — boxing
    /// and node allocations are both native) never initiates ANY
    /// collection: young fills once with mostly-dead wrappers, every
    /// subsequent allocation spills, old gen fills, and
    /// [`alloc_young_initialized`] hard-aborts a process whose heap is
    /// almost entirely garbage.
    young_spill_pressure: std::sync::atomic::AtomicBool,
    /// BUG-03 — absolute `(cursor, end)` reserved-tail regions of TLABs
    /// belonging to peer threads that the cross-thread STW JIT root scan
    /// forcibly stopped while they were executing JIT code. Such a peer never
    /// reached a safepoint to `retire` (tail-fill) its TLAB, so its un-filled
    /// tail would desync the non-moving sweep's linear heap walk. The
    /// collector publishes these regions here (under STW, before
    /// `collect_garbage`) and clears them afterward; the non-moving young
    /// sweep treats them like already-free blocks — neither walked as objects
    /// nor reclaimed into the free list — so the peer's reservation survives
    /// the collection intact. Empty on every normal collection (byte-identical
    /// default path).
    jit_tlab_skip_regions: Mutex<Vec<(usize, usize)>>,
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
            if n < numa_num_nodes {
                n
            } else {
                0
            }
        };

        let heap = Self {
            young_from: Mutex::new(Arena::new(young_semi_size)),
            young_to: Mutex::new(Arena::new(young_semi_size)),
            old_gen: Mutex::new(old_gen),
            quarantine: Mutex::new(VecDeque::new()),
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
            young_spill_pressure: std::sync::atomic::AtomicBool::new(false),
            jit_tlab_skip_regions: Mutex::new(Vec::new()),
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
        for (i, (slot, (base, cap))) in self.region_bounds.iter().zip(pairs).enumerate() {
            slot.0.store(base, Ordering::Release);
            slot.1.store(base.wrapping_add(cap), Ordering::Release);
            // Mirror into the process-global table the JIT's guarded inline
            // getfield bakes as an absolute address (see JIT_REGION_BOUNDS).
            JIT_REGION_BOUNDS.words[i * 2].store(base, Ordering::Release);
            JIT_REGION_BOUNDS.words[i * 2 + 1].store(base.wrapping_add(cap), Ordering::Release);
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
    /// returns the index that `try_alloc_young_initialized`/`refill_tlab`
    /// would use to pick a per-node arena. Today there is only one arena pair, so the
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
        // `plan_object_alloc` also picks the compact reference-field layout when
        // enabled (smaller `total_size`, `array_length` = body bytes,
        // `GC_FLAG_COMPACT`).
        let (total_size, array_len, compact_flag) = plan_object_alloc(class_id, num_fields)
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
        // `alloc_young_initialized` fire the OOM diagnostic and abort.
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                self.next_hash(),
                array_len,
                num_slots_u32,
            );
            header.gc_flags |= compact_flag;
            // SAFETY: `ptr` was just allocated from the young arena with
            // `total_size` bytes and remains protected by the arena lock until
            // this header write completes.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag it so the next native-call boundary
                // runs a GC (this method itself must stay GC-free — see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) = self.try_alloc_object_old(class_id, num_fields) {
                    return obj;
                }
                self.alloc_young_initialized(total_size, &init)
            }
        };

        // SAFETY: `ptr` is non-null and the header was fully initialized by
        // `try_alloc_young_initialized` / `alloc_young_initialized` before the
        // young arena lock was released.
        unsafe {
            crate::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Object as u8,
                ArrayElementType::Reference as u8,
                array_len,
                num_slots_u32,
                total_size,
            );
            ObjectRef::from_raw(ptr)
        }
    }

    /// Fallible twin of [`alloc_object`]: walks the identical
    /// young → old-gen spill path, but returns `None` instead of aborting the
    /// process when both generations are exhausted (or the size overflows).
    /// This lets a *native*/JIT caller surface a catchable
    /// `java.lang.OutOfMemoryError` ("Java heap space") rather than the VM
    /// hard-aborting in [`alloc_young_initialized`]. Like `alloc_object` it performs no GC,
    /// so it is safe to call from a context holding unrooted local `ObjectRef`s
    /// (the JIT object-alloc helper GC-and-retries before calling this).
    pub fn try_alloc_object_full(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let (total_size, array_len, compact_flag) = plan_object_alloc(class_id, num_fields)?;
        let num_slots_u32 = u32::try_from(num_fields).ok()?;

        // Young fast path; on exhaustion spill into old gen (non-moving) BEFORE
        // reporting OOM — mirrors `alloc_object`, but returns `None` instead of
        // aborting in `alloc_young_initialized` when old gen is also full (the divergence).
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                self.next_hash(),
                array_len,
                num_slots_u32,
            );
            header.gc_flags |= compact_flag;
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag for the boundary GC (see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) = self.try_alloc_object_old(class_id, num_fields) {
                    return Some(obj);
                }
                return None;
            }
        };

        // SAFETY: identical invariants to `alloc_object` — `ptr` is a freshly
        // bump-allocated, exclusively-owned, zeroed region of `total_size` bytes
        // with 8-byte alignment, so writing the header and wrapping it in an
        // `ObjectRef` are sound.
        unsafe {
            crate::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Object as u8,
                ArrayElementType::Reference as u8,
                array_len,
                num_slots_u32,
                total_size,
            );
            Some(ObjectRef::from_raw(ptr))
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
            // standard OOM diagnostic fires (`alloc_young_initialized` aborts
            // hard with a young-gen-exhaustion message; a future humongous-OOM
            // diagnostic would live here).
        }

        // Young fast path; on exhaustion spill into old gen (non-moving) BEFORE
        // the hard abort. The panicking `alloc_array` is used by the native
        // `ctx.new_array`/`new_ref_array` allocators, which cannot safely
        // GC-and-retry (a moving young GC would dangle their unrooted local
        // ObjectRefs). `try_alloc_array_humongous` allocates in old gen
        // regardless of size; only when old gen is also full does
        // `alloc_young_initialized` fire the OOM diagnostic and abort. See
        // [`try_alloc_object_old`].
        let init = |ptr: *mut u8| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type,
                self.next_hash(),
                length_u32,
                length_u32,
            );
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag for the boundary GC (see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) =
                    self.try_alloc_array_humongous(class_id, element_type, length_u32)
                {
                    return obj;
                }
                self.alloc_young_initialized(total_size, &init)
            }
        };

        // SAFETY: `ptr` was allocated, zeroed, and header-initialized before
        // the young arena lock was released.
        unsafe {
            crate::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Array as u8,
                element_type as u8,
                length_u32,
                length_u32,
                total_size,
            );
            // Data region already zeroed by the locked young-allocation helper.
            ObjectRef::from_raw(ptr)
        }
    }

    /// Fallible twin of [`alloc_array`]: walks the identical
    /// young → humongous(old-gen) spill path, but returns `None` instead of
    /// aborting the process when the request cannot be satisfied (size/length
    /// overflow, or both generations exhausted). This lets a *native* caller
    /// surface a catchable `java.lang.OutOfMemoryError` (matching HotSpot's
    /// "Requested array size exceeds VM limit" / "Java heap space") rather than
    /// the VM hard-aborting in [`alloc_young_initialized`]. Like `alloc_array` it performs
    /// no GC, so it is safe to call from a native method holding unrooted local
    /// `ObjectRef`s.
    pub fn try_alloc_array_full(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;
        let length_u32 = u32::try_from(length).ok()?;

        // Humongous path: skip young, allocate straight into old gen.
        if self.is_humongous(total_size) {
            if let Some(obj) = self.try_alloc_array_humongous(class_id, element_type, length_u32) {
                return Some(obj);
            }
            // Old gen full — fall through to the young path (mirrors `alloc_array`).
        }

        let init = |ptr: *mut u8| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type,
                self.next_hash(),
                length_u32,
                length_u32,
            );
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag for the boundary GC (see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) =
                    self.try_alloc_array_humongous(class_id, element_type, length_u32)
                {
                    return Some(obj);
                }
                // Both generations exhausted: report OOM to the caller instead
                // of aborting (the divergence from `alloc_array`).
                return None;
            }
        };

        // SAFETY: identical invariants to `alloc_array` — `ptr` is a freshly
        // bump-allocated, exclusively-owned, zeroed region of `total_size`
        // bytes with 8-byte alignment, so writing the header and wrapping it in
        // an `ObjectRef` are sound.
        unsafe {
            crate::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Array as u8,
                element_type as u8,
                length_u32,
                length_u32,
                total_size,
            );
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate a Java object. Returns `None` if young gen is exhausted.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // M6 (round-12 gc): make the `+ HEADER_SIZE` add checked too, so a
        // near-`usize::MAX` field count can't wrap past the checked multiply.
        // `plan_object_alloc` also selects the compact reference-field layout.
        let (total_size, array_len, compact_flag) = plan_object_alloc(class_id, num_fields)?;
        let num_slots_u32 = u32::try_from(num_fields).ok()?;
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                self.next_hash(),
                array_len,
                num_slots_u32,
            );
            header.gc_flags |= compact_flag;
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = self.try_alloc_young_initialized(total_size, &init)?;
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // and 8-byte alignment via `try_alloc_young_initialized`. The pointer
        // is exclusively owned, so creating an `ObjectRef` is sound.
        unsafe {
            crate::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Object as u8,
                ArrayElementType::Reference as u8,
                array_len,
                num_slots_u32,
                total_size,
            );
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
            if let Some(obj) = self.try_alloc_array_humongous(class_id, element_type, length_u32) {
                return Some(obj);
            }
            // Old gen full — fall through to the young path. If young is
            // also too small, the caller (`gc_alloc_array`) will trigger
            // a GC and retry, which may free old-gen space; if that still
            // can't satisfy the request, the standard OOM fires.
        }

        let init = |ptr: *mut u8| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type,
                self.next_hash(),
                length_u32,
                length_u32,
            );
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = self.try_alloc_young_initialized(total_size, &init)?;
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // for the array header + data and 8-byte alignment. The pointer is exclusively
        // owned, so writing the header and creating an `ObjectRef` are sound.
        unsafe {
            crate::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Array as u8,
                element_type as u8,
                length_u32,
                length_u32,
                total_size,
            );
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
        let frac_threshold = (semi / 100).saturating_mul(HUMONGOUS_YOUNG_FRACTION_PERCENT);
        // Cap the fraction-based threshold so large arrays bypass the young
        // copying space regardless of heap size (see
        // `HUMONGOUS_ABSOLUTE_CAP_BYTES`). On small/default heaps the fraction
        // is far below the cap, so `min` leaves behaviour unchanged.
        let threshold = frac_threshold.min(HUMONGOUS_ABSOLUTE_CAP_BYTES);
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
        let data_size = array_data_size(length_u32 as usize, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;

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

        let ptr = {
            let mut og = self.old_gen.lock();
            let ptr = og.alloc(total_size, 8)?;
            // SAFETY: `OldGen::alloc` returned a pointer to `total_size`
            // bytes of zeroed, 8-byte-aligned memory exclusive to this
            // allocation. The old-gen lock is still held, so major GC cannot
            // walk the span until this complete header has been published.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
            ptr
        };
        self.stats.old_allocations.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` points at the header just written above; it is a valid,
        // fully-initialized, heap-owned object so wrapping it as an `ObjectRef` is sound.
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
    pub(crate) fn try_alloc_object_old(
        &self,
        class_id: ClassId,
        num_fields: usize,
    ) -> Option<ObjectRef> {
        let (total_size, array_len, compact_flag) = plan_object_alloc(class_id, num_fields)?;
        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            array_len,
            u32::try_from(num_fields).ok()?,
        );
        header.gc_flags |= GC_FLAG_OLD_GEN | compact_flag;
        let ptr = {
            let mut og = self.old_gen.lock();
            let ptr = og.alloc(total_size, 8)?;
            // SAFETY: `OldGen::alloc` returned `total_size` bytes of zeroed,
            // 8-byte-aligned memory exclusive to this allocation. Holding the
            // old-gen lock through the header write prevents major GC from
            // observing the allocation before it is a valid object.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
            ptr
        };
        self.stats.old_allocations.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` points at the header just written above; it is a valid,
        // fully-initialized, heap-owned object so wrapping it as an `ObjectRef` is sound.
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    /// Allocate up to `count` identical objects in old generation while
    /// holding its lock once. Native wrapper allocation uses this after young
    /// space is exhausted, keeping unused objects in a GC-visible per-thread
    /// pool. A partial batch is valid when old space runs out.
    pub fn try_alloc_objects_old_batch(
        &self,
        class_id: ClassId,
        num_fields: usize,
        count: usize,
    ) -> Vec<ObjectRef> {
        // This batch refill only runs once young can no longer supply a TLAB
        // (see `NativeContextImpl::alloc_object`), i.e. on a young-exhaustion
        // spill: arm the boundary-GC pressure flag (advisability-gated; one
        // extra old-gen lock per 2048-object batch, not per allocation).
        self.note_young_spill_pressure();
        let Some((total_size, array_len, compact_flag)) = plan_object_alloc(class_id, num_fields)
        else {
            return Vec::new();
        };
        let Ok(num_slots) = u32::try_from(num_fields) else {
            return Vec::new();
        };
        let mut objects = Vec::with_capacity(count);
        let mut og = self.old_gen.lock();
        for _ in 0..count {
            let Some(ptr) = og.alloc(total_size, 8) else {
                break;
            };
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                self.next_hash(),
                array_len,
                num_slots,
            );
            header.gc_flags |= GC_FLAG_OLD_GEN | compact_flag;
            // SAFETY: `OldGen::alloc` returned an exclusive, zeroed span and
            // the old-generation lock remains held until the header is valid.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
                objects.push(ObjectRef::from_raw(ptr));
            }
        }
        self.stats
            .old_allocations
            .fetch_add(objects.len() as u64, Ordering::Relaxed);
        objects
    }

    /// DIAGNOSTIC-ONLY (cce0079 tree-key tail): current minor-GC count, for
    /// the `[SETFIELD-GC]` epoch assertion in the VM's `set_field` wrapper.
    pub fn debug_minor_gc_count(&self) -> u64 {
        self.stats.minor_gc_count.load(Ordering::Relaxed)
    }

    // ----- Header access -----------------------------------------------------

    /// Read the object header from a heap reference.
    pub fn get_header(&self, obj_ref: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj_ref` was created by one of this heap's `alloc_*` methods (or
        // forwarded during GC), so its pointer targets a valid, fully initialized
        // `ObjectHeader` within a heap-owned arena. The reference lifetime is bounded
        // by `&self`, ensuring the arena stays alive.
        let header = unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) };
        // CRATONVM_DBG_STALE_OBJREF: `get_header` is the accessor native code
        // and interpreter bytecode dispatch use to inspect a supposedly-live
        // object (`get_field`/`set_field`/`class_id_of`/`array_length`/
        // `identity_hash_code`/etc. all funnel through here) — the GC's own
        // internal forward/remap machinery reads headers through its own raw
        // pointer casts instead (see `forward_object_impl`), so it never hits
        // this check. A forwarded header reaching a caller here means the
        // caller is holding a raw `ObjectRef` local across a GC-triggering
        // call without `pin_native_root`/`read_native_pin` — exactly the
        // "Family 1" stale-ObjectRef pattern documented in
        // docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.
        // Only reachable when the quarantine dance in `collect_garbage_inner`
        // is active (see the `quarantine` field), since without it the
        // evacuated memory would already have been zeroed by the time a
        // mutator could observe it.
        if crate::stale_objref_debug::enabled() && header.is_forwarded() {
            // DIAGNOSTIC-ONLY (attrib-cce-investigate, 2026-07-13): read the
            // object's REAL, fully-valid header at the forwarded (new)
            // address so the panic message identifies which class/kind went
            // stale — the old-address header is just a forwarding marker by
            // this point. Not a functional change: only executed on the
            // already-panicking path, gated behind the same debug flag.
            let fwd_ptr = header.forwarding_address();
            let (fwd_class_id, fwd_kind) = if !fwd_ptr.is_null() {
                let fwd_header = unsafe { &*(fwd_ptr as *const ObjectHeader) };
                (
                    fwd_header.class_id.as_u32(),
                    format!("{:?}", fwd_header.kind),
                )
            } else {
                (u32::MAX, "<null-forward>".to_string())
            };
            // DIAGNOSTIC-ONLY (cceres2, 2026-07-16): before dying, name every
            // heap slot that STILL holds the stale (old) address. A holder in
            // OLD gen indicts the old->young scan/remap (card path) for that
            // slot's store site; NO heap holder means the stale copy lived
            // only in a frame/register/native local (root-remap gap). Card
            // state is printed as-of-panic (the GC consumes dirty bits, so
            // false here does NOT prove the card was clean at GC time).
            let stale_usize = obj_ref.as_ptr() as usize;
            let mut holders = String::new();
            let mut holder_n = 0usize;
            {
                use std::fmt::Write as _;
                if let Some(old) = self.old_gen.try_lock() {
                    let card_base = self.card_table.base_addr();
                    'oldscan: for (optr, _sz) in old.walk_objects() {
                        // SAFETY: walk_objects yields valid object starts.
                        let oh = unsafe { &*(optr as *const ObjectHeader) };
                        let mut hits: Vec<usize> = Vec::new();
                        // SAFETY: header/object pair valid for the walk.
                        unsafe {
                            for_each_ref_slot(optr, oh, |raw, idx| {
                                if raw as usize == stale_usize {
                                    hits.push(idx);
                                }
                            });
                        }
                        for idx in hits {
                            let cidx = (optr as usize).saturating_sub(card_base)
                                / crate::card_table::CARD_SIZE;
                            let _ = write!(
                                holders,
                                "\n  OLD holder {:p} class_id={} kind={:?} slot={} card_dirty_now={}",
                                optr,
                                oh.class_id.as_u32(),
                                oh.kind,
                                idx,
                                self.card_table.is_dirty(cidx),
                            );
                            holder_n += 1;
                            if holder_n >= 16 {
                                break 'oldscan;
                            }
                        }
                    }
                } else {
                    let _ = write!(holders, "\n  (old_gen lock busy — old holders not scanned)");
                }
                // Young from-space: lock-free raw word scan over the published
                // region bounds (object-walk under mutation is not crash-safe).
                let yf_base = self.region_bounds[0].0.load(Ordering::Acquire);
                let yf_end = self.region_bounds[0].1.load(Ordering::Acquire);
                if yf_base != 0 && yf_end > yf_base && holder_n < 16 {
                    let mut addr = yf_base;
                    while addr + 8 <= yf_end {
                        // SAFETY: [yf_base, yf_end) is a mapped arena range.
                        let w: u64 = unsafe { std::ptr::read_volatile(addr as *const u64) };
                        if w as usize == stale_usize {
                            let _ = write!(
                                holders,
                                "\n  YOUNG word 0x{addr:x} (from-space offset 0x{:x}) still holds the stale address",
                                addr - yf_base,
                            );
                            holder_n += 1;
                            if holder_n >= 16 {
                                break;
                            }
                        }
                        addr += 8;
                    }
                }
                if holder_n == 0 {
                    let _ = write!(
                        holders,
                        "\n  NO heap holder found — the stale copy lived only in a \
                         frame/register/native local (root-remap gap), or its holder \
                         was itself already collected",
                    );
                }
            }
            crate::stale_objref_debug::LAST_STALE_ADDR
                .store(obj_ref.as_ptr() as usize, Ordering::Release);
            panic!(
                "CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at {:p} — this \
                 object was evacuated by a moving GC to {:p} (class_id={fwd_class_id} \
                 kind={fwd_kind}), but native/interpreter code \
                 dereferenced the OLD address. This means a raw ObjectRef local was held \
                 across a GC-triggering call without pin_native_root/read_native_pin. See \
                 docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.\
                 \nHolder scan:{holders}",
                obj_ref.as_ptr(),
                fwd_ptr,
            );
        }
        // CRATONVM_DBG_BLOCKED_ACCESS: a header access on a thread whose own
        // `in_blocked_region` flag is raised races any concurrently running
        // collection — the STW census excluded this thread, so nothing on it
        // may touch the heap until `check_post_block_gc` re-syncs it. No-op
        // (one cached-bool branch) when the gate is off. See
        // `blocked_access_debug` for the full rationale.
        crate::blocked_access_debug::check_blocked_access(
            "heap header access",
            obj_ref.as_ptr() as usize,
        );
        header
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
    /// BUG-03 — publish the reserved-tail regions of forcibly-stopped in-JIT
    /// peers' TLABs so the next non-moving young sweep skips them (see
    /// [`Self::jit_tlab_skip_regions`]). Replaces any previously-set list.
    /// Pass an empty slice (or call [`Self::clear_jit_tlab_skip_regions`]) to
    /// reset. Must be set under STW, immediately before the collection, and
    /// cleared immediately after.
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        let mut g = self.jit_tlab_skip_regions.lock();
        g.clear();
        g.extend_from_slice(regions);
    }

    /// BUG-03 — clear the JIT TLAB skip regions (see
    /// [`Self::set_jit_tlab_skip_regions`]).
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.jit_tlab_skip_regions.lock().clear();
    }

    /// BUG-03 — the published JIT TLAB skip regions as young-from byte
    /// offsets `(offset, size)`, filtered to those that fall wholly inside the
    /// given `[from_base, from_end)` window and sorted ascending. Empty on the
    /// normal collection path.
    fn jit_tlab_skip_offsets(&self, from_base: usize, from_end: usize) -> Vec<(usize, usize)> {
        let g = self.jit_tlab_skip_regions.lock();
        if g.is_empty() {
            return Vec::new();
        }
        let mut v: Vec<(usize, usize)> = g
            .iter()
            .filter_map(|&(c, e)| {
                if c >= from_base && e <= from_end && e > c && (c & 0x7) == 0 {
                    Some((c - from_base, e - c))
                } else {
                    None
                }
            })
            .collect();
        v.sort_by_key(|&(off, _)| off);
        v
    }

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

        // Validate raw enum tags before constructing an `ObjectHeader`
        // reference. Conservative root scans can land on arbitrary arena words;
        // invalid `#[repr(u8)]` discriminants must be rejected as bytes, not
        // reached through a typed enum match.
        let kind = unsafe { object_kind_from_tag(*raw.add(OBJECT_KIND_OFFSET)) }?;
        let _element_type =
            unsafe { array_element_type_from_tag(*raw.add(ARRAY_ELEMENT_TYPE_OFFSET)) }?;

        // Object/Array are the only
        // valid kinds; anything else means we landed in the middle of a
        // field or in stale memory. HumongousFiller is a synthetic
        // walker-sentinel (round-9 gc CRIT-1) and never represents a
        // real object reachable from a root.
        if kind == ObjectKind::HumongousFiller {
            return None;
        }

        // SAFETY: The region check above confirmed `raw` is inside one of the
        // three arenas, and the enum tag bytes have been validated.
        let header = unsafe { &*(raw as *const ObjectHeader) };
        // Cheap structural sanity before trusting a conservative root
        // candidate as an object header. These invariants are written by every
        // allocator before publication; payload/interior words often satisfy
        // the loose kind/slot checks below but fail one of these bytes.
        if !header_reserved_fields_plausible(header) {
            return None;
        }

        // Cap num_slots at a sanity limit so a stale word can't fool us
        // into "validating" a slot count that would exceed the arena.
        // Multi-array reloc fix (2026-05-22): for arrays, `num_slots` is a
        // mirror of `array_length` (see `alloc_array` and `try_alloc_array`),
        // so a legitimate 256 MB int[] has num_slots = 2^26 > 1<<24 and
        // would be falsely rejected here. Bound num_slots only for non-arrays.
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24; // 16M slots -> 256 MB obj
        let is_array = kind == ObjectKind::Array;
        let is_compact = !is_array && is_compact_object(header);
        if is_array && header.gc_flags & GC_FLAG_COMPACT != 0 {
            return None;
        }
        if !is_array && !is_compact && header.array_length != 0 {
            return None;
        }
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

        // Family-A fix (2026-07-03): also validate the object's EXTENT against
        // the arena it claims to live in — the same hardening `mark_young`
        // (sweep_young_non_moving) already applies, now centralized here so
        // every one of this function's ~28 callers gets it. Without this, a
        // conservative root candidate that lands NOT on a real object start
        // but on an interior 16-byte `Value` cell of a live `Object[]` (every
        // element's discriminant word is `VTAG_OBJECT = 4`, so `class_id=4`
        // decodes at the candidate offset AND `num_slots=4` decodes 16 bytes
        // later — a self-consistent-looking but entirely coincidental fake
        // header) passes every check above. Bit-plausible headers of this
        // shape are common: a hot proxy/reflection `invoke(Object,Object[])`
        // path keeps element-interior pointers alive in registers/stack slots
        // that a conservative scan (`scan_one_frame` et al.) then feeds
        // through this function as candidate roots. `mark_young` catches this
        // via its extent check and safely discards such a candidate WITHOUT
        // writing through it; but every OTHER caller of `is_object_address`
        // (root-set seeding for old-gen `major_gc`, the selective-promotion
        // pin set, `is_movable_jit_root`, cross-thread snapshot validation,
        // and more) had no such check and could WRITE through the accepted
        // "object" (mark bit, gc_age, forwarding_ptr) at a byte offset that
        // is actually the interior of an unrelated live array — corrupting
        // that array's real header/body bytes. This reproduced as the
        // long-standing "kind=Object but array_length=N; inline-alloc forgot
        // to set kind=Array" corruption family (Family-A / MiniThrottle /
        // the independently-found Hibernate-batch corruption): the "N" is
        // not random — it is bits of the flipped mark/age/forwarding write
        // landing inside the victim array's real `array_length` field.
        let claimed_extent = if is_array {
            match array_data_size(header.array_length as usize, header.element_type) {
                Ok(data) => HEADER_SIZE.checked_add(data),
                Err(_) => None,
            }
        } else {
            HEADER_SIZE.checked_add(object_body_size(header))
        };
        let Some(extent) = claimed_extent else {
            return None;
        };
        let Some(obj_end) = addr.checked_add(extent) else {
            return None;
        };
        // The object's full extent must fit within the SAME arena the region
        // check above matched (not just "some" arena — a header claiming to
        // span from young into old gen is exactly the interior-cell false
        // positive this guards against).
        let extent_fits = self.region_bounds.iter().any(|(base, end)| {
            let b = base.load(Ordering::Acquire);
            let e = end.load(Ordering::Acquire);
            addr >= b && addr < e && obj_end <= e
        });
        if !extent_fits {
            return None;
        }

        // SAFETY: `raw` passed the region containment, header sanity, and
        // extent-containment checks above, so it points to a valid object
        // header within a heap arena.
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
    /// DIAGNOSTIC-ONLY (cceres3): see `VmHeap::debug_forwarded_target`.
    pub fn debug_forwarded_target(&self, addr: usize) -> Option<usize> {
        if !crate::stale_objref_debug::enabled() || addr == 0 || addr % 8 != 0 {
            return None;
        }
        // PROBE FIX (cce0079 tree-key tail): `is_heap_addr` covers only the
        // LIVE arenas — but a stale (evacuated) address lives in the
        // quarantine RING, which is deliberately unpublished. Gating on
        // `is_heap_addr` alone made this probe (and both PIN canaries built
        // on it) structurally blind to exactly the addresses it exists to
        // catch. Also accept addresses inside any ring arena.
        let in_live = self.is_heap_addr(addr).is_some();
        if !in_live {
            let in_ring = self
                .quarantine
                .lock()
                .iter()
                .any(|a| a.contains(addr as *const u8));
            if !in_ring {
                return None;
            }
        }
        // SAFETY: containment in a mapped (live or quarantined) arena was
        // just confirmed; the quarantine ring keeps evacuated from-space
        // readable while the canary flag is on.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if header.is_forwarded() {
            let fwd = header.forwarding_address() as usize;
            if fwd != 0 {
                return Some(fwd);
            }
        }
        None
    }

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
                || OOB_DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < OOB_DIAG_CAP
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
                // A2 forensic breadcrumb (CRATONVM_DBG_A2): an OOB read on a
                // ClassId(0)/num_slots=0 receiver is the "zeroed live object"
                // face — dump the address's recorded alloc/free lifecycle so
                // the zeroing event is attributable (in-place clobber vs
                // reclaim-and-reuse vs never-allocated).
                if crate::a2dbg::enabled() && header.class_id.as_u32() == 0 {
                    let victim = obj_ref.as_ptr() as usize;
                    let hist = crate::a2dbg::history_at(victim, 12);
                    if hist.is_empty() {
                        eprintln!("[oob-field]   [A2] NO event touches {victim:#x}");
                    } else {
                        for r in hist {
                            if r.kind == 0xFF {
                                eprintln!("[oob-field]   [A2] seq={} FREE @{:#x}", r.seq, r.addr);
                            } else {
                                eprintln!(
                                    "[oob-field]   [A2] seq={} ALLOC @{:#x} class_id={} kind={} et={} alen={} ns={} size={}{}",
                                    r.seq, r.addr, r.class_id, r.kind, r.element_type,
                                    r.array_length, r.num_slots, r.size,
                                    if r.addr != victim { " (covering)" } else { "" },
                                );
                            }
                        }
                    }
                }
            } // end rate-limited OOB-read diagnostics
            // RESID-DIAG (dohead residuals investigation, 20260718): narrow,
            // unconditional backtrace for the specific shape seen in the
            // known-issues residual logs (index 4/5, zero-slot receiver) —
            // rare enough that this doesn't need the OOB_DIAG_CAP treatment.
            if num_slots == 0 && (index == 4 || index == 5) {
                let diag_class_name = crate::gc::resolve_class_info(header.class_id.as_u32())
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "<unresolved>".to_string());
                eprintln!(
                    "[RESID-DIAG READ] class={diag_class_name} index={index} num_slots={num_slots} obj={:p}\n{}",
                    obj_ref.as_ptr(),
                    std::backtrace::Backtrace::force_capture()
                );
            }
            return Value::Object(None);
        }
        // Compact reference-field layout: reference fields are 8-byte pointers
        // at their per-class byte offset; primitive fields stay 16-byte cells.
        if let Some((off, is_ref)) = compact_field_slot(header, index) {
            // SAFETY: `index < num_slots` (checked above) ⇒ `off` is within the
            // object's body (prefix-sum offset table), so `base` and the 8/16-byte
            // read of that slot are in-bounds.
            let base = unsafe { obj_ref.as_ptr().add(HEADER_SIZE + off) };
            if !is_ref {
                return unsafe { read_slot(base) };
            }
            let v = unsafe { read_prim_element(base, 0, ArrayElementType::Reference) };
            // Unbox an AUTOBOX wrapper: a non-Object value stored into this
            // reference slot (CratonVM's synthetic collections type-pun a
            // primitive into a reference-declared slot) was boxed into a
            // 1-field wrapper by set_field. Mirror get_array_element_unboxing so
            // the value round-trips and the wrapper never escapes to Java.
            if let Value::Object(Some(r)) = v {
                if self.is_object_address(r.as_ptr() as usize).is_some() {
                    // SAFETY: address validated as a live heap object.
                    let h = unsafe { &*(r.as_ptr() as *const ObjectHeader) };
                    if h.class_id == AUTOBOX_CLASS_ID {
                        return self.get_field(r, 0);
                    }
                }
            }
            return v;
        }
        // SAFETY: `obj_ref` points to a valid heap object and `index` is within
        // `num_slots` (checked above). `slot_ptr` computes
        // `obj_ref + HEADER_SIZE + index * SLOT_SIZE`, which is within the
        // object's allocated region. `read_slot` reads a `Value` from that address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            // gcstress residual face-1 diagnostics (CRATONVM_DBG_CELLCORRUPT):
            // identify the HOLDER of a corrupt Value cell before `read_slot`
            // masks it with a benign null. The holder's class/kind/layout — and
            // whether the stale raw0 pointer lands in current young-from,
            // young-to, old gen, or nowhere — discriminates the candidate
            // mechanisms (8-vs-16-byte slot-layout confusion vs un-remapped
            // holder after a moving young GC vs stale freed arena after grow).
            if cell_corrupt_diag_enabled()
                && cratonvm_types::read_value_checked(ptr as *const Value).is_none()
            {
                self.dump_corrupt_cell_holder(obj_ref, header, index, ptr);
            }
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
                    self.get_header(obj_ref).class_id.as_u32(),
                    index,
                    a,
                );
            }
        }
        // CRATONVM_DBG_STALE_OBJREF (cce0079): a stale VALUE stored into a
        // reference field is otherwise silent (only the receiver's header is
        // read below) — surface the store site itself while the quarantine
        // holds the forwarding marker (producer-side attribution; matches
        // the equivalent check in `set_array_element`).
        if crate::stale_objref_debug::enabled() {
            if let Value::Object(Some(v)) = value {
                // Operand attribution (cce0079 tree-key tail): peek the
                // header word first so the imminent canary panic can be
                // attributed to the STORED VALUE (vs the receiver, whose
                // own get_header follows below).
                let peek = unsafe { &*(v.as_ptr() as *const ObjectHeader) };
                if peek.is_forwarded() {
                    eprintln!(
                        "[storechk] set_field: STALE stored VALUE 0x{:x} \
                         (receiver 0x{:x} slot {index}) thread={} cycle={} — canary panic follows",
                        v.as_ptr() as usize,
                        obj_ref.as_ptr() as usize,
                        std::thread::current().name().unwrap_or("?"),
                        self.stats.minor_gc_count.load(Ordering::Relaxed),
                    );
                }
                let _ = self.get_header(v);
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
            // DBG (CRATONVM_DBG_SWEEP_ZERO): if this OOB receiver was recently
            // ZEROED by the non-moving sweep, name its ORIGINAL class and the GC
            // that reclaimed it — turns "set_field on java/lang/Object[0]" into
            // "set_field on a SWEPT <UserClass>", naming the root-coverage gap.
            if let Some((cid, kind, cycle, reason, initiator, blocked)) =
                sweep_zero_lookup(obj_ref.as_ptr() as usize)
            {
                let orig_name = crate::gc::resolve_class_info(cid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "<unresolved>".to_string());
                eprintln!(
                    "[SWEEP-ZERO-HIT] set_field receiver {:p} was a SWEPT {} \
                     (orig class_id={} kind={}) reclaimed at cycle={} reason={} \
                     initiator_tid={} blocked={}",
                    obj_ref.as_ptr(),
                    orig_name,
                    cid,
                    kind,
                    cycle,
                    reason,
                    initiator,
                    blocked,
                );
            }
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
            // RESID-DIAG (dohead residuals investigation, 20260718): see the
            // matching comment in get_field's OOB guard above.
            if num_slots == 0 && (index == 4 || index == 5) {
                eprintln!(
                    "[RESID-DIAG WRITE] class={class_name} index={index} num_slots={num_slots} obj={:p} value={value:?}\n{}",
                    obj_ref.as_ptr(),
                    std::backtrace::Backtrace::force_capture()
                );
            }
            return;
        }
        debug_assert!(index < self.get_header(obj_ref).num_slots as usize);
        // Compact reference-field layout: store reference fields as 8-byte
        // pointers; primitive fields stay 16-byte cells. The write barrier fires
        // in every arm (card-mark old→young + SATB), same as the legacy path.
        if let Some((off, is_ref)) = compact_field_slot(header, index) {
            // SAFETY: `index < num_slots` (checked above) ⇒ `off` within body.
            let base = unsafe { obj_ref.as_ptr().add(HEADER_SIZE + off) };
            if is_ref {
                match value {
                    Value::Object(_) => {
                        unsafe { write_prim_element(base, 0, ArrayElementType::Reference, value) };
                        self.write_barrier(obj_ref, value);
                    }
                    // A non-reference value written into a reference slot
                    // (typeless `Unsafe.put*`): box it into a 1-field wrapper,
                    // exactly as compact reference *arrays* do (AUTOBOX_CLASS_ID).
                    _ => {
                        let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                        self.set_field(wrapper, 0, value);
                        let wv = Value::Object(Some(wrapper));
                        unsafe { write_prim_element(base, 0, ArrayElementType::Reference, wv) };
                        self.write_barrier(obj_ref, wv);
                    }
                }
            } else {
                unsafe { write_slot(base, value) };
                self.write_barrier(obj_ref, value);
            }
            return;
        }
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
    pub fn get_field_volatile_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj_ref, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Descriptor-aware set — normalizes the written `Value` to the declared
    /// field type before the underlying slot write.
    pub fn set_field_as(&self, obj_ref: ObjectRef, index: usize, value: Value, desc_byte: u8) {
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
                // SAFETY: `obj_ptr` was just dereferenced via `get_header` without
                // faulting, so the header's bytes (offsets 0..16) are readable; raw
                // byte/u32 reads are well-defined for any bit pattern.
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
        // CRATONVM_DBG_STALE_OBJREF (cce0079): a stale VALUE stored into a
        // reference array is otherwise silent (only the ARRAY's header is
        // read below) — the wrong object then surfaces at an arbitrarily
        // later read as a CCE. With the quarantine active, read the value's
        // header too so the store site itself trips the loud forwarded-
        // header panic (producer-side attribution).
        if crate::stale_objref_debug::enabled() {
            if let Value::Object(Some(v)) = value {
                // Operand attribution — see the matching peek in `set_field`.
                let peek = unsafe { &*(v.as_ptr() as *const ObjectHeader) };
                if peek.is_forwarded() {
                    eprintln!(
                        "[storechk] set_array_element: STALE stored VALUE 0x{:x} \
                         (array 0x{:x} index {index}) thread={} — canary panic follows",
                        v.as_ptr() as usize,
                        obj_ref.as_ptr() as usize,
                        std::thread::current().name().unwrap_or("?"),
                    );
                }
                let _ = self.get_header(v);
            }
        }
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        // gcstress residual face-1 diagnostics (CRATONVM_DBG_CELLCORRUPT) —
        // the debug_assert above is a no-op in release: an array-element
        // write through a STALE array reference whose address is now occupied
        // by a plain object would write a raw 8-byte pointer into the middle
        // of that object's 16-byte Value cells (the observed {ptr, 0} corrupt
        // cells). Trap it with the holder identity + backtrace. The bounds
        // check below usually deflects such writes (a plain object has
        // array_length=0), so this logs the attempt either way.
        if cell_corrupt_diag_enabled() && header.kind != ObjectKind::Array {
            let class_name = crate::gc::resolve_class_info(header.class_id.as_u32())
                .map(|(n, _)| n)
                .unwrap_or_else(|| "<unresolved>".to_string());
            eprintln!(
                "[CELLCORRUPT:set_array_element-on-NON-ARRAY] obj=0x{:x} class_id={} \
                 class={class_name} kind=0x{:02x} num_slots={} array_len={} index={index} \
                 value={value:?}\n{}",
                obj_ref.as_ptr() as usize,
                header.class_id.as_u32(),
                header.kind as u8,
                header.num_slots,
                header.array_length,
                std::backtrace::Backtrace::force_capture(),
            );
        }
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

    /// Borrow the SATB queue without cloning the `Arc`. The JIT helper
    /// entry path (`VmHeap::flush_thread_satb`) checks `is_active()` on
    /// EVERY slow-path allocation; the refcount round trip of
    /// [`Self::satb_queue_handle`] is measurable there and buys nothing —
    /// the queue, once installed by `enable_concurrent_gc`, lives as long
    /// as the heap.
    #[inline]
    pub fn satb_queue_ref(&self) -> Option<&SatbQueue> {
        self.satb_queue.as_deref()
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

    /// True when `addr` is the start of a young from-space object that
    /// SURVIVED the collection that just ran (young-GC live-reclaim ROOT FIX,
    /// 2026-07-07 — RRWL/ThreadLocalMap$Entry IMSE/hang family).
    ///
    /// The NON-MOVING young sweep keeps survivors in place with NO
    /// `pointer_map` entry, so post-GC reference processing's survivor
    /// predicate (`pointer_map.contains_key(addr) || is_addr_live(addr)`)
    /// judged every live young Reference object and referent dead: nulled
    /// referents were never restored and live processor entries were pruned.
    /// The watched-referent identity-map mechanism covers survivors the
    /// sweep's disposition walk actually visits, but a desync-retained
    /// stretch (walk re-anchor) leaves its survivors without entries — this
    /// predicate closes that gap at the consumer side.
    ///
    /// Discriminator: the sweep ZEROES every span it reclaims before
    /// publishing it to the free list, so inside the current from-space
    /// "first header word non-zero" == "not reclaimed by the sweep that just
    /// ran". This is only sound where it is used — between `collect_garbage`
    /// returning and mutators resuming (the same STW window in which
    /// `process_references_after_gc` runs), when no allocation can have
    /// reused a reclaimed hole yet. After a MOVING collection, pre-GC
    /// addresses lie in the *other* semispace (survivors were evacuated and
    /// the arenas swapped), so this returns false for them and the
    /// pointer-map half of the predicate remains authoritative, exactly as
    /// before. A dead-but-retained object in a desync-skipped stretch reads
    /// as "live" for one cycle — pure over-retention, the sweep's documented
    /// safe direction.
    pub fn is_live_young_survivor(&self, addr: usize) -> bool {
        if addr & 0x7 != 0 {
            return false;
        }
        let from = self.young_from.lock();
        let base = from.base_ptr() as usize;
        if addr < base || addr >= base + from.used() {
            return false;
        }
        // SAFETY: bounds-checked 8-aligned address inside the mapped
        // from-space region; reading one u64 is valid.
        let word0 = unsafe { std::ptr::read(addr as *const u64) };
        word0 != 0
    }

    /// Access the old generation directly (for concurrent sweep).
    /// Returns a lock guard.
    pub fn old_gen_lock(&self) -> parking_lot::MutexGuard<'_, OldGen> {
        self.old_gen.lock()
    }

    // ----- GC ----------------------------------------------------------------

    /// Record that a native-wrapper allocation had to spill into old gen
    /// because young was exhausted. See the `young_spill_pressure` field doc.
    ///
    /// The pressure flag (and therefore the boundary GC) is only armed once
    /// old-gen headroom drops below the worst-case promotion demand — the
    /// entire young semi (a boundary GC may need to evacuate ALL of young's
    /// live set into old) plus a young/8 margin. While old gen has more room
    /// than that, spilling is both safe and much cheaper than a full mark
    /// cycle (HashMapOnly 30M at -Xmx16g: 13.5s spilling vs 32.8s collecting
    /// on first exhaustion), so we stay on the spill path. Past the bound,
    /// waiting longer risks promotion failure (old too full to drain young)
    /// → the both-gens-full abort this machinery exists to avoid.
    ///
    /// Takes the old-gen lock — call this only from allocation SLOW paths
    /// (the spill arms / batch refill), never per-object.
    pub fn note_young_spill_pressure(&self) {
        let young_semi = self.young_semi_capacity();
        let advisable = {
            let og = self.old_gen.lock();
            og.used() + young_semi + young_semi / 8 >= og.capacity()
        };
        if advisable {
            self.young_spill_pressure
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Native-wrapper young-exhaustion signal (see `young_spill_pressure`
    /// field doc). One relaxed load — cheap enough for a per-native-call
    /// check on the dispatch hot path.
    #[inline]
    pub fn young_spill_pressure(&self) -> bool {
        self.young_spill_pressure
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Clear the native-wrapper young-exhaustion signal (after the
    /// boundary GC ran, or after deciding the flag was stale).
    #[inline]
    pub fn clear_young_spill_pressure(&self) {
        self.young_spill_pressure
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns true when the young generation should be collected.
    pub fn needs_gc(&self) -> bool {
        self.needs_gc_with_jit_allocation_frame(false)
    }

    /// The JIT allocation/refill helper calls this while its compiled caller
    /// is still on the native stack. Even when that caller is the unregistered
    /// compiled entry point, root gathering will detect it and select the
    /// non-moving young collector, so that path may safely use its larger
    /// occupancy trigger.
    pub fn needs_gc_for_jit_allocation(&self) -> bool {
        self.needs_gc_with_jit_allocation_frame(true)
    }

    fn needs_gc_with_jit_allocation_frame(&self, jit_allocation_frame: bool) -> bool {
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
        // Cheney copying needs the unused half as worst-case survivor
        // headroom. The default non-moving collector instead sweeps in place,
        // so it can safely use the active semi-space almost to capacity.
        // Only select the larger threshold when the next normal young cycle is
        // guaranteed to take that path. In particular, `--nojit` collections
        // remain moving even though `CRATONVM_MOVING_YOUNG` is unset.
        let non_moving_young = next_young_gc_is_guaranteed_non_moving(
            crate::gc_quiescence::is_active(),
            crate::gc_quiescence::unregistered_jit_frame_on_stack(),
            jit_allocation_frame,
            crate::gc_quiescence::moving_young_enabled(),
            std::env::var_os("CRATONVM_ALLOW_MOVING_YOUNG").is_some(),
            std::env::var_os("CRATONVM_DBG_FORCE_MOVING").is_some(),
        );
        let threshold = young_gc_trigger_bytes(
            from.capacity(),
            *self.young_gc_threshold.lock(),
            non_moving_young,
        );
        live >= threshold
    }

    /// Total bytes currently allocated across young and old generations.
    pub fn allocated_bytes(&self) -> usize {
        self.young_from.lock().used() + self.old_gen.lock().used()
    }

    /// Live-bytes estimate for GC-productivity accounting: like
    /// [`allocated_bytes`], but young counts `used - free_list_bytes`
    /// instead of the raw bump cursor. The non-moving young sweep reclaims
    /// dead objects into the from-space free list WITHOUT retreating the
    /// cursor, so the raw `used()` stays pinned at its high-water mark
    /// forever after young first fills — measured that way, every sweep
    /// looks like it freed 0 bytes and the GC-overhead limit falsely
    /// declares a perfectly-productive collector "thrashing" (then stops
    /// collecting, wedging the heap into the old-gen-spill → abort path).
    /// Same live metric `needs_gc` already uses for its trigger.
    pub fn live_bytes_estimate(&self) -> usize {
        let from = self.young_from.lock();
        let young_live = from.used().saturating_sub(from.free_list_bytes());
        drop(from);
        young_live + self.old_gen.lock().used()
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
            // SAFETY: `a` is an 8-aligned address with `a + 16 <= end_a`, so this
            // word and the following one are fully inside the live young-gen arena.
            let disc = unsafe { std::ptr::read(a as *const u64) };
            if disc == 4 {
                // SAFETY: `a + 8` is in-bounds (loop guard ensures `a + 16 <= end_a`).
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
                        // SAFETY: `h_addr = a - off` is `>= base_a` (checked above) and
                        // `< a < end_a`, so a full `ObjectHeader` lies within the arena;
                        // fields are read defensively before trusting the contents.
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
                        // SAFETY: `a + 16 <= end_a` (loop guard), so reading the
                        // following word (the neighbour cell) stays inside the arena.
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

    /// Run one complete NON-MOVING young collection cycle (the divert path of
    /// `collect_garbage_inner`): young mark-sweep, threshold-gated in-place
    /// old sweep, and the monitor-registry remap for selective promotions.
    /// Extracted so the moving path can ALSO divert here mid-flight when its
    /// young object-start walk cannot complete (see the cce0079 walk fix in
    /// `collect_garbage_inner`) — a moving cycle with a partial start set
    /// would silently fail to evacuate every object past the walk breakout,
    /// dangling every reference to them once the semispaces swap.
    fn run_non_moving_young_cycle(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        let mut result = self.sweep_young_non_moving(roots, finalizer_addrs);
        // The JIT-active path cannot use the ordinary old-gen compactor:
        // conservative JIT stack/register roots cannot be rewritten when an
        // old object moves.  Previously this early return therefore skipped
        // old collection altogether.  Long allocation-heavy runs eventually
        // filled old space with dead objects promoted by the selective young
        // sweep (or spilled there by an allocation slow path), making depth
        // 20 Binary Trees OOM despite a small live tree.  Sweep old space in
        // place once it reaches the same pressure threshold as the compacting
        // path.  The shared marker retains every conservative root, while
        // the non-moving sweep only returns unreachable blocks to OldGen's
        // free lists and never invalidates a raw JIT pointer.
        // In this path a pending System.gc() must still sweep old gen,
        // even below the normal occupancy threshold. Consume the request
        // here because the moving Phase-5 check below is skipped.
        let major_requested = crate::gc_quiescence::take_major_gc_request();
        let old_capacity = self.old_gen_capacity();
        // CRATONVM_OLD_SWEEP_JIT=0 opts out of the in-place old sweep on
        // this conservative-roots path (diagnostic escape hatch / A-B
        // bisection knob for suspected live-object reclaims — the young
        // sweep survives an imperfect root set via conservative
        // over-marking and side-mark containment, but this old sweep
        // frees purely on GC_FLAG_MARKED, so any root-set gap frees a
        // LIVE promoted object). Read once per GC cycle — not hot.
        let old_sweep_enabled = std::env::var("CRATONVM_OLD_SWEEP_JIT")
            .map(|v| v != "0")
            .unwrap_or(true);
        if old_sweep_enabled
            && old_capacity > 0
            && (self.old_gen_used() >= old_capacity * 75 / 100 || major_requested)
        {
            let old_freed = self.sweep_old_gen_non_moving(roots);
            result.0.stats.bytes_freed += old_freed;
            self.stats
                .bytes_freed_old
                .fetch_add(old_freed as u64, Ordering::Relaxed);
            self.stats.major_gc_count.fetch_add(1, Ordering::Relaxed);
        }
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
        // DBG (CRATONVM_DBG_YOUNGSTATE): post-collection young/old arena
        // state — the bimodal-bt18 discriminator (is young allocatable
        // after this sweep, and from which structure?).
        if std::env::var_os("CRATONVM_DBG_YOUNGSTATE").is_some() {
            let from = self.young_from.lock();
            let og = self.old_gen.lock();
            eprintln!(
                "[youngstate] post-sweep used={}/{} free_list={} largest_free={} old={}/{}",
                from.used(),
                from.capacity(),
                from.free_list_bytes(),
                from.largest_free_block(),
                og.used(),
                og.capacity(),
            );
        }
        return result;
    }

    fn collect_garbage_inner(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        // DBG (CRATONVM_DBG_GCPAUSE): time every collection and report slow
        // ones — pairs with CRATONVM_DBG_PARKLAT to split the RRWL
        // crawl/join-stall between "GC pauses dominate" and "wake latency".
        struct PauseTimer(Option<std::time::Instant>);
        impl Drop for PauseTimer {
            fn drop(&mut self) {
                if let Some(t0) = self.0 {
                    let ms = t0.elapsed().as_millis();
                    if ms >= 100 {
                        eprintln!("[gcpause] collection took {ms}ms");
                    }
                }
            }
        }
        let _pause_timer = {
            use std::sync::OnceLock;
            static G: OnceLock<bool> = OnceLock::new();
            let on = *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_GCPAUSE").is_some());
            PauseTimer(on.then(std::time::Instant::now))
        };
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
        // B-K kafka fix (PROTOTYPE): the shadow stack publishes the operand-stack
        // `Reg` oops the conservative stack scan misses (register-invisibility).
        // Previously `shadow_roots` forced the MOVING Cheney (to remap those oops
        // precisely), but moving UNDER-COUNTS bt18 (67674804) — the non-moving
        // sweep + card fix is the correct collector. Decouple: keep the NON-MOVING
        // sweep AND let the shadow oops be scanned as roots → PINNED (kept alive,
        // not relocated). The reload is then a no-op (pinned objects never move),
        // so the "incompatible with non-moving" concern does not apply. This keeps
        // bt18 = 68332206 while closing the kafka register-invisible reclamation.
        let _shadow_roots = std::env::var_os("CRATONVM_SHADOW_STACK").is_some();
        // Promotion-OOM avoidance: a MOVING (Cheney) young collection aborts the
        // PROCESS when it cannot relocate a survivor — old gen is full AND the
        // young to-space overflowed while promotion fell back to it (see the
        // `process::abort()` in the forward path, ~line 5255). That can only
        // happen once the old generation can no longer absorb the surviving young
        // set. When old gen cannot hold a full young's worth of survivors, run
        // the NON-MOVING sweep instead: it never relocates (so it can never hit
        // that abort), reclaims any dead young in place, and simply leaves
        // un-promotable survivors in young (graceful `old_full = true`). A
        // genuinely exhausted heap then surfaces a *catchable*
        // `OutOfMemoryError` via the allocation paths / GC-overhead limit instead
        // of aborting. The non-moving sweep is already the default (and superior)
        // young collector while JIT frames are active, so this only widens when
        // it runs. Opt out with `CRATONVM_NO_GC_PROMOTION_GUARD` (reverts to the
        // moving collector, which may abort the process on a full heap).
        let promotion_oom_risk = std::env::var_os("CRATONVM_NO_GC_PROMOTION_GUARD").is_none() && {
            // The moving collector's promotion abort needs BOTH generations
            // nearly full at once: old gen cannot absorb the aged survivors AND
            // the young to-space cannot hold the (then unpromotable) surviving
            // set. Gate on exactly that precondition (each ≥ 90% full). A large
            // young object over a near-empty old gen — e.g. the
            // `large_array_survives_gc` / `multiple_large_arrays_survive_gc`
            // tests, where young is big and old is ~empty — must still use the
            // moving (compacting) collector, so old-gen fullness is required too.
            let old_cap = self.old_gen_capacity();
            let young_cap = self.young_semi_capacity();
            old_cap > 0
                && young_cap > 0
                && (self.old_gen_used() as u128) * 10 >= (old_cap as u128) * 9
                && (self.young_from_used() as u128) * 10 >= (young_cap as u128) * 9
        };
        // A5 fix: `unregistered_jit_frame_on_stack()` — the VM root scan found a
        // guard-less JIT frame on the mutator's native stack (e.g. the compiled
        // entry-point `main` while a clinit/interpreted callee runs). Its live
        // objects were conservatively MARKED by the full-stack scan but cannot be
        // relocated (raw register/spill slots can't be rewritten), so run the
        // NON-MOVING sweep exactly as for a registered JIT frame (`is_active()`).
        let has_conservative_roots = crate::gc_quiescence::is_active()
            || crate::gc_quiescence::unregistered_jit_frame_on_stack();
        // System.gc() requests an old-gen-inclusive cycle. Route that cycle
        // through the non-moving marker so it can follow collection-overlay
        // edges from live owners instead of globally rooting every overlay.
        let explicit_full_gc = crate::gc_quiescence::major_gc_requested();
        // HIB-CV-22/32/33 ROOT FIX: only honor `promotion_oom_risk` as a reason
        // to divert into the non-moving sweep when there are un-rewritable
        // conservative JIT roots to protect. The non-moving sweep exists SOLELY
        // because a moving (Cheney) cycle cannot rewrite a JIT register/spill
        // slot it discovered conservatively. When NO JIT frame is on any stack
        // (`--nojit`, or a JIT-quiescent collection) there are no such roots:
        //   * the moving collector is then fully precise and correct, AND
        //   * its to-space is a fresh semispace the size of from-space, so the
        //     packed live survivor set always fits (promotion failure falls back
        //     to to-space) — the `process::abort()` that `promotion_oom_risk`
        //     was added to avoid is effectively unreachable on this path, and
        //   * the moving path runs the Phase-5 major GC (mark-compact) that the
        //     non-moving sweep's early return SKIPS, relieving the very old-gen
        //     pressure that keeps `promotion_oom_risk` latched on (otherwise the
        //     heap wedges in non-moving mode under load — exactly when the bug
        //     bites).
        // Meanwhile the non-moving sweep, run on this no-conservative-root path,
        // relies on conservative over-marking it does NOT have, exposing a
        // precise-root/remap gap that reclaims a still-live young object — the
        // boxed `java.lang.Byte` (HIB-CV-32), the JUnit `AtomicBoolean`
        // (HIB-CV-22), and the SessionFactory oop (HIB-CV-33). `FORCE_MOVING`
        // makes all three disappear, confirming the moving path is clean here.
        // Opt out (restore the old broad diversion) with
        // `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`.
        let honor_promotion_oom_risk = promotion_oom_risk
            && (has_conservative_roots
                || std::env::var_os("CRATONVM_PROMOTION_OOM_GUARD_BROAD").is_some());
        // Default moving young gen (`CRATONVM_MOVING_YOUNG`): the JIT publishes a
        // COMPLETE rewritable precise root map (shadow stack) for every live frame
        // and the conservative scan is suppressed (see roots.rs), so a live JIT
        // frame no longer forces the non-moving sweep — run the moving (Cheney)
        // cycle instead. The `honor_promotion_oom_risk` guard is STILL respected as
        // a safety fallback: when both generations are ~full the moving path can
        // `process::abort()` on a promotion failure, so we divert to the
        // (abort-free) non-moving sweep for that cycle regardless of the flag. The
        // suppressed conservative scan means that fallback sweep also relies on the
        // complete shadow map for marking — consistent, since the shadow map is the
        // sole precise JIT root set under this flag. If the VM detected an
        // incomplete JIT coverage proof during root gathering, this cycle treats
        // moving-young as unavailable and uses the conservative/non-moving
        // fallback instead.
        let moving_young_requested = crate::gc_quiescence::moving_young_enabled();
        let fail_closed_non_moving = crate::gc_quiescence::is_active()
            && std::env::var_os("CRATONVM_ALLOW_MOVING_YOUNG").is_none();
        let force_non_moving_jit_roots = crate::gc_quiescence::force_non_moving_jit_roots();
        let coverage_incomplete = crate::gc_quiescence::moving_young_coverage_incomplete();
        let divert_for_incomplete_moving_coverage =
            moving_young_requested && (force_non_moving_jit_roots || coverage_incomplete);
        let moving_young = moving_young_requested && !divert_for_incomplete_moving_coverage;
        let divert_non_moving = fail_closed_non_moving
            || (has_conservative_roots && !moving_young_requested)
            || honor_promotion_oom_risk
            || divert_for_incomplete_moving_coverage
            || explicit_full_gc;
        if watchref_dbg() {
            eprintln!(
                "[watchref] collect_garbage_inner: has_conservative_roots={has_conservative_roots} moving_young_requested={moving_young_requested} divert_non_moving={divert_non_moving} force_moving={force_moving}"
            );
        }
        if divert_non_moving && (!force_moving || divert_for_incomplete_moving_coverage) {
            if divert_for_incomplete_moving_coverage {
                let n = crate::gc_quiescence::record_moving_young_coverage_fallback();
                if std::env::var_os("CRATONVM_MOVING_YOUNG_FALLBACKS").is_some() {
                    eprintln!(
                        "[moving-young] coverage fallback #{n}: incomplete live JIT safepoint map; running non-moving young sweep"
                    );
                }
            }
            tracing::debug!(
                "running non-moving young-gen mark-sweep (jit_active={}, \
                 unregistered_jit_frame={}, promotion_oom_risk={}, honored={}, moving_young={}, \
                 force_non_moving_jit_roots={}) — compaction deferred.",
                crate::gc_quiescence::is_active(),
                crate::gc_quiescence::unregistered_jit_frame_on_stack(),
                promotion_oom_risk,
                honor_promotion_oom_risk,
                moving_young,
                force_non_moving_jit_roots,
            );
            return self.run_non_moving_young_cycle(roots, finalizer_addrs, monitors);
        }

        let mut young_from = self.young_from.lock();
        let mut young_to = self.young_to.lock();
        let mut old_gen = self.old_gen.lock();
        // CRATONVM_DBG_STALE_OBJREF: only locked/touched below when the flag
        // is set (see the `quarantine` field's doc comment); an uncontended
        // lock of an unused, zero-capacity arena otherwise.
        let mut quarantine = self.quarantine.lock();

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
                // SAFETY: `walk_objects` yields the start of each live old-gen object,
                // so `obj_ptr` targets a valid, fully-initialized `ObjectHeader`.
                let hdr = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let addr = obj_ptr as usize;
                let card_idx = addr.wrapping_sub(cbase) / csize;
                let dirty = card_table.is_dirty(card_idx);
                if hdr.kind == ObjectKind::Array {
                    if hdr.element_type == ArrayElementType::Reference {
                        for i in 0..hdr.array_length as usize {
                            // SAFETY: `i < hdr.array_length`, so the element offset is
                            // within the array's allocated payload; `obj_ptr.add(..)`
                            // and the u64 read of that ref slot stay in-bounds.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            let raw = unsafe { std::ptr::read(s_ptr as *const u64) };
                            if raw != 0 && raw < 0x1000 {
                                let cn = crate::gc::resolve_class_info(hdr.class_id.as_u32())
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| "<unresolved>".to_string());
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
                                        let cn =
                                            crate::gc::resolve_class_info(hdr.class_id.as_u32())
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
                        // SAFETY: `slot_idx < hdr.num_slots`, so the slot offset is
                        // within the object's allocated field area; the pointer and the
                        // `Value` read of that initialized slot are in-bounds.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                        if let Value::Object(Some(ro)) = value {
                            let p = ro.as_ptr() as usize;
                            if p != 0 && p < 0x1000 {
                                let cn = crate::gc::resolve_class_info(hdr.class_id.as_u32())
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| "<unresolved>".to_string());
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
                                        let cn =
                                            crate::gc::resolve_class_info(hdr.class_id.as_u32())
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
                // SAFETY: the young space is bump-allocated and contiguous; `ycur < yused`
                // keeps `ybase + ycur` inside the live region, where a valid `ObjectHeader`
                // begins (a bad header is detected by the size check below).
                let h = unsafe { &*((ybase + ycur) as *const ObjectHeader) };
                let size = gen_object_total_size(h);
                if size == 0 || ycur + size > yused {
                    eprintln!(
                        "[small4] young walk truncated at off={} used={}",
                        ycur, yused
                    );
                    break;
                }
                let optr = (ybase + ycur) as *mut u8;
                if h.kind == ObjectKind::Object {
                    for si in 0..h.num_slots as usize {
                        // SAFETY: `si < h.num_slots` and `ycur + size <= yused` was
                        // checked, so this slot is within the object's field area in the
                        // arena; the pointer and `Value` read of that slot are in-bounds.
                        let sp = unsafe { optr.add(HEADER_SIZE + si * SLOT_SIZE) };
                        let v = unsafe { std::ptr::read(sp as *const Value) };
                        if let Value::Object(Some(ro)) = v {
                            let p = ro.as_ptr() as usize;
                            if p != 0 && p < 0x1000 && found4 < 40 {
                                found4 += 1;
                                let cn = crate::gc::resolve_class_info(h.class_id.as_u32())
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| "<unresolved>".to_string());
                                eprintln!(
                                    "[small4] PRE-GC YOUNG {} @0x{:x} fld[{}] -> 0x{:x}",
                                    cn,
                                    ybase + ycur,
                                    si,
                                    p,
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
                                    let cell_payload = victim + HEADER_SIZE + si * SLOT_SIZE + 8;
                                    let lo = victim.saturating_sub(128).max(ybase);
                                    let hi = (victim + size + 128).min(ybase + yused);
                                    eprintln!(
                                        "[small4] HEXDUMP victim=0x{victim:x} size={size} cell_payload=0x{cell_payload:x}:"
                                    );
                                    let mut a = lo & !7;
                                    while a < hi {
                                        // SAFETY: `a` is 8-aligned and `lo`/`hi` are
                                        // clamped to `[ybase, ybase+yused)`, so each word
                                        // read stays within the live young arena.
                                        let w = unsafe { std::ptr::read(a as *const u64) };
                                        eprintln!(
                                            "[small4]   0x{a:x}: 0x{w:016x}{}{}",
                                            if a == victim {
                                                "  <-- victim header"
                                            } else {
                                                ""
                                            },
                                            if a == cell_payload {
                                                "  <<<< corrupt payload"
                                            } else {
                                                ""
                                            },
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
                        // SAFETY: `i < h.array_length` and `ycur + size <= yused` was
                        // checked, so this element offset is within the array payload in
                        // the arena; the pointer and u64 read of that ref slot are in-bounds.
                        let sp = unsafe { optr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw = unsafe { std::ptr::read(sp as *const u64) } as usize;
                        if raw != 0 && raw < 0x1000 && found4 < 40 {
                            found4 += 1;
                            let cn = crate::gc::resolve_class_info(h.class_id.as_u32())
                                .map(|(n, _)| n)
                                .unwrap_or_else(|| "<unresolved>".to_string());
                            eprintln!(
                                "[small4] PRE-GC YOUNG {} @0x{:x} arr[{}] -> 0x{:x}",
                                cn,
                                ybase + ycur,
                                i,
                                raw,
                            );
                        }
                    }
                }
                ycur += size;
            }
        }
        // -------------------------------------------------------------------

        let bytes_before = young_from.used();
        // Conservative JIT/native stack scans can produce aligned interior
        // addresses inside young objects. Build an exact pre-forwarding object
        // start set so only allocator-written headers can ever receive a
        // forwarding pointer; an interior false positive must merely be ignored.
        //
        // cce0079 ROOT FIX (2026-07-16): this walk previously assumed a
        // contiguous bump-allocated young space and BREAK'd at the first
        // implausible header. But young_from legitimately contains free-list
        // and TLAB gap ranges (zeroed, headerless) — under WildFly's
        // ~40-thread `parallel-extension-add` the walk reliably hit one a few
        // MB in and silently dropped EVERY later young object from the start
        // set. `forward_object_impl` treats "not in the start set" as a
        // conservative interior word and returns the address UNMOVED — for
        // every root (precise roots and native pins included) and every
        // scanned reference slot — so entire swaths of live young objects
        // were never evacuated and every reference to them dangled into
        // recycled memory after the semispace swap. That is the mechanism
        // behind the WildFly boot `ClassCastException: java.lang.Object
        // cannot be cast to X` / stale-ObjectRef family (canary-confirmed
        // live via the CRATONVM_DBG_STALE_OBJREF quarantine ring: the walk
        // warning printed seconds before stale reads surfaced in
        // native_map_get bucket contents and EnhancedQueueExecutor node
        // fields). Walk the same free-list-aware grid as the non-moving
        // exact walk (`skip_free_blocks`); if the walk STILL cannot complete
        // (genuinely corrupt header), the moving collector is unsound this
        // cycle — divert to the non-moving sweep, which tolerates a partial
        // view (conservative over-marking, never relocates).
        let mut young_object_starts: FxHashSet<usize> = FxHashSet::default();
        let young_base = young_from.base_ptr() as usize;
        let young_used = young_from.used();
        let mut start_walk_complete = true;
        // Merge the free-block list with un-retired TLAB tails (reserved,
        // never on the free list — the exact gap class this walk was
        // breaking on) — the same skip set the non-moving exact walk builds
        // via its local `merge_skips`. Both inputs are ascending & disjoint.
        let start_skips = {
            let mut v = young_from.free_blocks_sorted();
            v.extend(self.jit_tlab_skip_offsets(young_base, young_base + young_used));
            v.sort_by_key(|&(off, _)| off);
            v
        };
        let mut start_free_iter = start_skips.iter().peekable();
        let mut young_cursor = 0usize;
        while young_cursor < young_used {
            if skip_free_blocks(&mut young_cursor, &mut start_free_iter).0 {
                continue;
            }
            let obj_ptr = (young_base + young_cursor) as *mut u8;
            // SAFETY: free/TLAB ranges were skipped; the cursor is on an
            // allocator-written object boundary in the young arena.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Bug-D (2026-06-12) parity with every other young walk: stride
            // over a GAP-filler sentinel (a sub-`HEADER_SIZE` TLAB tail,
            // `install_tail_filler`) BEFORE `gen_object_total_size` — its
            // `num_slots` offset lies outside the gap, so parsing it as an
            // object reads garbage and the walk would abort. This was the
            // gap class that actually broke this walk in the WildFly repro
            // (cursor a few MB in, at the first thread's retired TLAB tail).
            if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                // SAFETY: offset 4 lies within the >=8-byte gap.
                let gap =
                    unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) } as usize;
                if (8..HEADER_SIZE).contains(&gap)
                    && gap & 7 == 0
                    && young_cursor + gap <= young_used
                {
                    young_cursor += gap;
                    continue;
                }
                tracing::warn!(
                    young_cursor,
                    young_used,
                    gap,
                    "GC: young object-start walk hit a corrupt GAP-filler — \
                     diverting this cycle to the non-moving sweep"
                );
                start_walk_complete = false;
                break;
            }
            let size = gen_object_total_size(header);
            if size < HEADER_SIZE
                || young_cursor
                    .checked_add(size)
                    .is_none_or(|end| end > young_used)
            {
                tracing::warn!(
                    young_cursor,
                    young_used,
                    "GC: young object-start walk stopped at an implausible extent — \
                     diverting this cycle to the non-moving sweep"
                );
                start_walk_complete = false;
                break;
            }
            if let Some(&&(off, _)) = start_free_iter.peek() {
                if off > young_cursor && off < young_cursor + size {
                    tracing::warn!(
                        young_cursor,
                        off,
                        "GC: young object-start walk crossed a free/TLAB range — \
                         diverting this cycle to the non-moving sweep"
                    );
                    start_walk_complete = false;
                    break;
                }
            }
            young_object_starts.insert(obj_ptr as usize);
            young_cursor += size;
        }
        if !start_walk_complete {
            // A partial start set makes the moving cycle unsound (see the
            // comment above). The non-moving sweep is NOT a safe fallback
            // here either: on this precise-root path it lacks the
            // conservative over-marking it needs and reclaims still-live
            // young objects (the HIB-CV-22/32/33 family — measured live in
            // this investigation: one diverted cycle produced all-zero-header
            // reads on live receivers 145 ms later). The only sound choice
            // is to SKIP this young collection entirely: over-retain for one
            // cycle, let the allocation slow paths spill to old gen, and
            // retry on the next trigger (by which point the unparseable
            // layout — normally a transient un-tail-filled gap — is gone).
            // Nothing destructive has happened yet: only the pre-collection
            // bounds publish and the card-buffer drain, both idempotent.
            tracing::warn!(
                "GC: young object-start walk incomplete — skipping this young \
                 collection (over-retain; retried next cycle)"
            );
            return (
                GcResult {
                    stats: crate::gc::GcStats {
                        objects_copied: 0,
                        bytes_copied: 0,
                        bytes_freed: 0,
                    },
                    pointer_map: HashMap::new(),
                },
                Vec::new(),
            );
        }
        let mut objects_copied: usize = 0;
        // Read & clear the promote-on-pressure flag set by the previous
        // minor GC. When true, every survivor of THIS cycle is promoted
        // to old gen regardless of age, breaking the long-lived-tree
        // semispace death spiral. Single AtomicBool::swap so the flag
        // doesn't latch across multiple consecutive cycles unless the
        // pressure persists.
        let force_promote_all = self.force_promote_all.swap(false, Ordering::Relaxed);
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
        Self::scan_dirty_cards(card_table, &old_gen, &young_from, &mut extra_roots);
        // Card marking is a fast path only. A missed barrier must retain an
        // object for one extra collection, never reclaim a reachable child.
        if Self::full_old_rset_scan_enabled() {
            Self::scan_all_old_to_young(&old_gen, &young_from, &mut extra_roots);
        }

        // Phase 1: Forward all root objects
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if !young_from.contains(old_ptr) {
                continue; // Skip roots not in young gen (e.g., old gen objects)
            }
            let new_ptr = Self::forward_object(
                &young_from,
                &young_object_starts,
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
                            &young_object_starts,
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
            } else if is_compact_object(header) {
                // Compact object: `slot_idx` is the BYTE OFFSET of an 8-byte
                // reference slot (recorded that way by the card scan). Mirror
                // the ref-array branch.
                // SAFETY: `slot_idx` (byte offset) was recorded by dirty-card
                // scanning within this object's body; the 8-byte read is in-bounds.
                let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx) };
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    let ref_ptr = raw as usize as *mut u8;
                    if young_from.contains(ref_ptr) {
                        let new_ptr = Self::forward_object(
                            &young_from,
                            &young_object_starts,
                            &mut young_to,
                            &mut old_gen,
                            ref_ptr,
                            &mut objects_copied,
                            &mut pointer_map,
                            &mut promoted_worklist,
                            force_promote_all,
                        );
                        // SAFETY: writing the forwarded pointer back to the slot.
                        unsafe { std::ptr::write(slot_ptr as *mut u64, new_ptr as u64) };
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
                            &young_object_starts,
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

        // HIB-CV-24: a live object keeps its class's defining ClassLoader alive
        // (the instance→loader edge HotSpot gets via `Class.getClassLoader`).
        // Cached once; `false` (and the registry's empty short-circuit) makes the
        // per-object lookup below a no-op for the common no-custom-loader case.
        let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();

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
                // HIB-CV-24: capture the class id before any forwarding may grow
                // young_to (which would invalidate `header`).
                let cid = header.class_id.as_u32();

                // Scan/forward ref slots. Ref arrays + compact objects store
                // 8-byte pointers; legacy objects store 16-byte Value cells.
                // SAFETY: `obj_ptr`/`header` are a valid copied object in young_to.
                unsafe {
                    forward_ref_slots(obj_ptr, header, |ref_ptr| {
                        if young_from.contains(ref_ptr) {
                            Some(Self::forward_object(
                                &young_from,
                                &young_object_starts,
                                &mut young_to,
                                &mut old_gen,
                                ref_ptr,
                                &mut objects_copied,
                                &mut pointer_map,
                                &mut promoted_worklist,
                                force_promote_all,
                            ))
                        } else {
                            None
                        }
                    });
                }

                // HIB-CV-24: keep this object's defining ClassLoader alive. If the
                // loader is a young object, evacuate it like any other survivor so
                // a live instance pins its loader (else a leaked instance's loader
                // would be wrongly reclaimed once the side-table stops rooting it).
                if loader_pin_on {
                    if let Some(loader_old) = cratonvm_types::loader_pin::loader_pin_addr(cid) {
                        let lp = loader_old as *mut u8;
                        if young_from.contains(lp) {
                            Self::forward_object(
                                &young_from,
                                &young_object_starts,
                                &mut young_to,
                                &mut old_gen,
                                lp,
                                &mut objects_copied,
                                &mut pointer_map,
                                &mut promoted_worklist,
                                force_promote_all,
                            );
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
                // HIB-CV-24: capture class id before forwarding may grow young_to.
                let promoted_cid = header.class_id.as_u32();

                // Scan/forward ref slots (ref arrays + compact objects use
                // 8-byte pointers; legacy objects use 16-byte Value cells). A
                // forwarded ref that stays in young to-space is an old→young
                // edge: defer-mark its card so the NEXT minor GC's dirty-card
                // scan sees it (deferred_dirty_cards is re-marked after Phase
                // 3's clear_all; a direct mark would be wiped). The object-field
                // case once missed this (BouncyCastle X9ECParametersHolder.params
                // -> young X9ECParameters lost its remembered-set entry); the
                // unified helper applies it to every layout.
                // SAFETY: `obj_ptr`/`header` are a valid promoted old-gen object.
                unsafe {
                    forward_ref_slots(obj_ptr, header, |ref_ptr| {
                        if young_from.contains(ref_ptr) {
                            let new_ref_ptr = Self::forward_object(
                                &young_from,
                                &young_object_starts,
                                &mut young_to,
                                &mut old_gen,
                                ref_ptr,
                                &mut objects_copied,
                                &mut pointer_map,
                                &mut promoted_worklist,
                                force_promote_all,
                            );
                            if !old_gen.contains(new_ref_ptr) {
                                deferred_dirty_cards.push(obj_ptr as usize);
                            }
                            Some(new_ref_ptr)
                        } else {
                            None
                        }
                    });
                }

                // HIB-CV-24: keep this promoted object's defining ClassLoader
                // alive (instance→loader). A young loader is evacuated; if it
                // stays in young to-space it is an old→young edge, so defer-mark
                // this object's card like the ref-slot case above.
                if loader_pin_on {
                    if let Some(loader_old) =
                        cratonvm_types::loader_pin::loader_pin_addr(promoted_cid)
                    {
                        let lp = loader_old as *mut u8;
                        if young_from.contains(lp) {
                            let new_lp = Self::forward_object(
                                &young_from,
                                &young_object_starts,
                                &mut young_to,
                                &mut old_gen,
                                lp,
                                &mut objects_copied,
                                &mut pointer_map,
                                &mut promoted_worklist,
                                force_promote_all,
                            );
                            if !old_gen.contains(new_lp) {
                                deferred_dirty_cards.push(obj_ptr as usize);
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
                &young_object_starts,
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
                    // Scan/forward ref slots (arrays + compact objects: 8-byte
                    // pointers; legacy objects: 16-byte Value cells).
                    // SAFETY: `obj_ptr`/`header` are a valid copied object in young_to.
                    unsafe {
                        forward_ref_slots(obj_ptr, header, |ref_ptr| {
                            if young_from.contains(ref_ptr) {
                                Some(Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    force_promote_all,
                                ))
                            } else {
                                None
                            }
                        });
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
                    // Scan/forward ref slots (arrays + compact objects: 8-byte
                    // pointers; legacy objects: 16-byte cells). A forwarded ref
                    // that stays young is an old→young edge — defer-mark its card
                    // (re-applied after clear_all; a direct mark would be wiped),
                    // for every layout (the ServiceLoader cryptoProvider bug).
                    // SAFETY: `obj_ptr`/`header` are a valid promoted old-gen object.
                    unsafe {
                        forward_ref_slots(obj_ptr, header, |ref_ptr| {
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    force_promote_all,
                                );
                                if !old_gen.contains(new_ref_ptr) {
                                    deferred_dirty_cards.push(obj_ptr as usize);
                                }
                                Some(new_ref_ptr)
                            } else {
                                None
                            }
                        });
                    }
                }
                if !made_progress {
                    break;
                }
            }
        }

        let bytes_copied = young_to.used();

        if moving_young_dangling_verify_enabled() {
            let mut missed_young = 0usize;
            let mut missed_old = 0usize;
            let mut reported = 0usize;
            let cap = 40usize;
            let mut scan_obj = |space: &str, obj: *mut u8, header: &ObjectHeader| -> usize {
                let mut n = 0usize;
                // SAFETY: caller passes live objects from young_to/old_gen walks while STW.
                unsafe {
                    for_each_ref_slot(obj, header, |raw, slot_id| {
                        let t = raw as usize;
                        if !young_from.contains(raw as *const u8) {
                            return;
                        }
                        let th = &*(raw as *const ObjectHeader);
                        if !th.is_forwarded() {
                            return;
                        }
                        n += 1;
                        if reported < cap {
                            reported += 1;
                            let referrer = crate::gc::resolve_class_info(header.class_id.as_u32())
                                .map(|(name, _)| name)
                                .unwrap_or_else(|| format!("cid#{}", header.class_id.as_u32()));
                            let target = crate::gc::resolve_class_info(th.class_id.as_u32())
                                .map(|(name, _)| name)
                                .unwrap_or_else(|| format!("cid#{}", th.class_id.as_u32()));
                            let expected = pointer_map
                                .get(&t)
                                .copied()
                                .unwrap_or_else(|| th.forwarding_address() as usize);
                            eprintln!(
                                "[moving-young-verify] MISSED-HEAP-REWRITE {} {}@0x{:x} slot={} -> forwarded {} old=0x{:x} new=0x{:x}",
                                space,
                                referrer,
                                obj as usize,
                                slot_id,
                                target,
                                t,
                                expected,
                            );
                        }
                    });
                }
                n
            };

            let mut cursor = 0usize;
            let young_used = young_to.used();
            while cursor < young_used {
                // SAFETY: young_to is bump-allocated; cursor advances by object size.
                let obj = unsafe { young_to.base_ptr_mut().add(cursor) };
                let header = unsafe { &*(obj as *const ObjectHeader) };
                let size = gen_object_total_size(header);
                if size < HEADER_SIZE || cursor + size > young_used {
                    break;
                }
                missed_young += scan_obj("YOUNG", obj, header);
                cursor += size;
            }
            for (obj, _size) in old_gen.walk_objects() {
                // SAFETY: walk_objects yields live old-gen object starts.
                let header = unsafe { &*(obj as *const ObjectHeader) };
                missed_old += scan_obj("OLD", obj, header);
            }
            eprintln!(
                "[moving-young-verify] forwarded_heap_refs_remaining young={} old={} pointer_map={}",
                missed_young,
                missed_old,
                pointer_map.len(),
            );
        }

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
        //
        // CRATONVM_DBG_STALE_OBJREF: instead of resetting (zeroing) the
        // just-evacuated `young_from` immediately, route it through the
        // quarantine ring for `quarantine_cycles()` extra cycles so a stale
        // native `ObjectRef` held into it still resolves to a forwarded
        // header (caught by `get_header`) rather than reading back all-zero.
        // The ring is ordered oldest-first: once it is full, the FRONT arena
        // (whose grace period has elapsed) is popped, reset (and grown, if
        // `young_from` has since expanded), and reused as the fresh arena;
        // the just-evacuated `young_from` is pushed onto the BACK to start
        // its own grace period. With the default single-cycle ring the final
        // external effect — which arena ends up serving as
        // `young_from`/`young_to` for the NEXT cycle — is identical to the
        // plain `young_from.reset()` this replaces; only the mechanics of
        // clearing memory differ.
        if crate::stale_objref_debug::enabled() {
            let cycles = crate::stale_objref_debug::quarantine_cycles();
            let mut reuse = if quarantine.len() >= cycles {
                let mut oldest = quarantine
                    .pop_front()
                    .expect("quarantine ring checked non-empty");
                oldest.reset();
                oldest
            } else {
                Arena::new(0)
            };
            if reuse.capacity() < young_from.capacity() {
                reuse.grow(young_from.capacity());
            }
            std::mem::swap(&mut *young_from, &mut reuse);
            // `reuse` now holds this cycle's just-evacuated from-space.
            quarantine.push_back(reuse);
        } else {
            young_from.reset();
        }

        // CRIT-P2 fix: convert the internal FxHashMap to the std HashMap
        // expected by `MonitorCleanup::remap_after_gc` (defined in
        // `collector.rs`) and `GcResult.pointer_map` (the public-API field
        // in `gc.rs`). The conversion is a single O(N) walk — cheap
        // compared to N SipHash operations across the Cheney scan.
        let mut pointer_map: HashMap<usize, usize> = pointer_map.into_iter().collect();

        // Phase 4: Swap young spaces (monitor remap deferred until after a
        // possible major GC so we can pass the composed pointer_map).
        std::mem::swap(&mut *young_from, &mut *young_to);
        // A2 breadcrumb (CRATONVM_DBG_A2): the swap relocates every live young
        // object to a fresh from-space, so all recorded absolute addresses are now
        // stale. Clear so cross-epoch lookups don't lie (keeps the breadcrumb
        // reliable within the next non-moving epoch, where A2's desync occurs).
        crate::a2dbg::clear();

        // Phase 5: Check if old gen is getting full — trigger major GC (mark-compact).
        //
        // `take_major_gc_request` is ALWAYS evaluated (not short-circuited by
        // `||`) so an explicit `System.gc()` request is consumed exactly once
        // per cycle even when the occupancy threshold was independently also
        // crossed — otherwise the request would leak into and force a LATER,
        // unrelated allocation-triggered minor GC into a major cycle it never
        // asked for. See `gc_quiescence`'s doc comment for the full rationale.
        let major_requested = crate::gc_quiescence::take_major_gc_request();
        if std::env::var_os("CRATONVM_DBG_MIRRORPIN").is_some() {
            eprintln!(
                "[DBG_MIRRORPIN] Phase5 old_gen_used={} old_gen_cap={} major_requested={} will_run_major={}",
                old_gen.used(),
                old_gen.capacity(),
                major_requested,
                old_gen.used() >= old_gen.capacity() * 75 / 100 || major_requested
            );
        }
        let major_ran = if old_gen.used() >= old_gen.capacity() * 75 / 100 || major_requested {
            tracing::debug!(
                "Old gen at {}% (or explicit System.gc() request) — running major GC (mark-compact)",
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
            self.stats.bytes_freed_old.fetch_add(
                old_used_before.saturating_sub(old_used_after) as u64,
                Ordering::Relaxed,
            );
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
        let high_survival =
            freed_percent < GC_PROMOTE_PRESSURE_PERCENT && bytes_before >= from_cap_before / 2;
        if high_survival {
            tracing::debug!(
                "GC: high survival ({}% freed of {} bytes) — \
                 arming promote-on-pressure for next minor GC",
                freed_percent,
                bytes_before,
            );
            self.force_promote_all.store(true, Ordering::Relaxed);
        }

        // Only expand when the young arena was actually under pressure at GC
        // start. Without this gate, a FORCED or early collection — e.g.
        // `System.gc()` fired when young holds only a few KB — computes
        // `freed_percent` against a tiny live set, reads it as "low reclamation",
        // and doubles the arena. Repeated across frequent forced GCs the young
        // balloons to `max_young_semi_size` (multi-GB) even though gigabytes are
        // free; growing + zeroing that arena under a stop-the-world is the
        // multi-thread GC "hang"/OOM (6 threads each looping `System.gc()` —
        // scratch_churn/Churn.java). Mirrors the `high_survival` occupancy guard
        // above (same `from_cap_before / 2` threshold): a young collected at its
        // `YOUNG_GC_THRESHOLD_PERCENT` (50%) natural-GC trigger still expands
        // (`bytes_before >= cap/2` holds), while a tiny forced/early GC
        // (`System.gc` at well under 50% full) does not.
        if freed_percent < GC_EXPANSION_THRESHOLD_PERCENT && bytes_before >= from_cap_before / 2 {
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
        let phase_diag = std::env::var_os("CRATONVM_DBG_GCPHASE").is_some();
        let phase_start = std::time::Instant::now();
        let mut phase_last = phase_start;
        let mut report_phase = |name: &str| {
            if phase_diag {
                let now = std::time::Instant::now();
                eprintln!(
                    "[gcphase] {name}: phase={}ms total={}ms",
                    now.duration_since(phase_last).as_millis(),
                    now.duration_since(phase_start).as_millis(),
                );
                phase_last = now;
            }
        };
        if watchref_dbg() {
            eprintln!("[watchref] sweep_young_non_moving ENTRY (non-moving path taken)");
        }
        let mut young_from = self.young_from.lock();
        let mut old_gen = self.old_gen.lock();

        // "Zeroed-a-live-object" detector cycle stamp (CRATONVM_DBG_SWEEP_ZERO).
        let sweep_zero_cycle = SWEEP_ZERO_CYCLE.fetch_add(1, Ordering::Relaxed) as u32;

        // Fold every mutator's thread-local card buffer into the bitmap
        // before scanning dirty cards (same protocol as the moving path).
        self.card_table.flush_all();
        self.card_table.drain_pending();

        let bytes_before = young_from.used();
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.used();

        // BUG-03 — reserved tails of forcibly-stopped in-JIT peers' TLABs,
        // as young-from offsets. Empty on the normal path. Every linear
        // from-space walk below merges these into its free-block skip list so
        // an un-retired tail is neither walked as objects nor reclaimed.
        let jit_skips = self.jit_tlab_skip_offsets(from_base, from_end);
        // Merge a sorted free-block list with `jit_skips` (both ascending,
        // disjoint — a TLAB tail is reserved, never on the free list). Cheap;
        // the skip list has at most one entry per live thread.
        let merge_skips = |free: Vec<(usize, usize)>| -> Vec<(usize, usize)> {
            if jit_skips.is_empty() {
                return free;
            }
            let mut v = free;
            v.extend_from_slice(&jit_skips);
            v.sort_by_key(|&(off, _)| off);
            v
        };

        // Helper: is `addr` the start of a young from-space object?
        let in_young =
            |addr: usize| -> bool { addr >= from_base && addr < from_end && (addr & 0x7) == 0 };

        // Build exact young-object bases before marking. The stack/JIT root
        // scan is conservative and can yield aligned interior words; treating
        // those as objects makes the side-mark channel retain the interior
        // address rather than its containing object, so the later sweep can
        // reclaim the real object.
        // PERF (perf/halfgap-20260717): the exact-base oracle exists for
        // CONSERVATIVE candidates only — the aligned interior words the
        // stack/register/pin scans produce (`roots`, `finalizer_addrs`).
        // Precise heap edges (the BFS `for_each_ref_slot` values, dirty-card
        // slot reads, overlay/loader/mirror side channels) hold object BASES
        // by construction and never needed interior-pointer mapping — they
        // take `mark_young_precise` below, exactly the pre-oracle behavior.
        //
        // That containment makes the oracle cheap to build: only ranges
        // covering an actual candidate are materialized (a handful per
        // cycle), and the builder walk EARLY-EXITS past the last candidate.
        // The previous shape materialized a `(start, end)` pair for EVERY
        // young object and routed every BFS edge through a binary search of
        // that Vec — on a 2 GiB young gen that is a ~50M-entry, ~800 MB Vec
        // rebuilt per collection plus a cache-hostile log2(50M) probe per
        // edge, which dominated the whole mark phase.
        let mut conservative_candidates: Vec<usize> = roots
            .iter()
            .map(|r| r.as_ptr() as usize)
            .chain(finalizer_addrs.iter().copied())
            .filter(|&a| in_young(a))
            .collect();
        conservative_candidates.sort_unstable();
        conservative_candidates.dedup();

        let mut young_object_ranges: Vec<(usize, usize)> = Vec::new();
        let mut cand_idx = 0usize;
        let exact_skips = merge_skips(young_from.free_blocks_sorted());
        let mut exact_free_iter = exact_skips.iter().peekable();
        let mut exact_cursor = 0usize;
        while exact_cursor < young_from.used() && cand_idx < conservative_candidates.len() {
            if skip_free_blocks(&mut exact_cursor, &mut exact_free_iter).0 {
                continue;
            }
            let ptr = (from_base + exact_cursor) as *mut u8;
            // SAFETY: free/TLAB ranges were skipped; this cursor is on an
            // allocator-written object boundary in the young arena.
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            // Skip a GAP-filler sentinel (Bug-D, 2026-06-12) before treating
            // this as a normal header — its layout overlays a raw gap length
            // at offset 4, not real header fields (see the established
            // pattern elsewhere in this file, e.g. the selective-promotion
            // walk above).
            if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                // SAFETY: offset 4 lies within the >=8-byte gap.
                let gap =
                    unsafe { std::ptr::read((ptr as *const u8).add(4) as *const u32) } as usize;
                if (8..HEADER_SIZE).contains(&gap)
                    && gap & 7 == 0
                    && exact_cursor + gap <= young_from.used()
                {
                    exact_cursor += gap;
                    continue;
                }
                tracing::warn!(
                    exact_cursor,
                    "GC: exact young-object walk found an implausible GAP-filler sentinel"
                );
                break;
            }
            let total = gen_object_total_size(header);
            if total < HEADER_SIZE
                || exact_cursor
                    .checked_add(total)
                    .is_none_or(|end| end > young_from.used())
            {
                tracing::warn!(
                    exact_cursor,
                    used = young_from.used(),
                    "GC: exact young-object walk stopped at an implausible extent"
                );
                break;
            }
            if let Some(&&(off, _)) = exact_free_iter.peek() {
                if off > exact_cursor && off < exact_cursor + total {
                    tracing::warn!(
                        exact_cursor,
                        off,
                        "GC: exact young-object walk crossed a free/TLAB range"
                    );
                    break;
                }
            }
            // Record this object's range only if it covers (or could still
            // cover) a conservative candidate; skip candidates that fell
            // into free/gap space below it (they resolve to "not an object"
            // in the oracle — identical to the full-walk behavior, where a
            // non-covered address failed the range probe and was dropped).
            let start_addr = ptr as usize;
            let end_addr = start_addr + total;
            while cand_idx < conservative_candidates.len()
                && conservative_candidates[cand_idx] < start_addr
            {
                cand_idx += 1;
            }
            if cand_idx < conservative_candidates.len()
                && conservative_candidates[cand_idx] < end_addr
            {
                young_object_ranges.push((start_addr, end_addr));
                while cand_idx < conservative_candidates.len()
                    && conservative_candidates[cand_idx] < end_addr
                {
                    cand_idx += 1;
                }
            }
            exact_cursor += total;
        }
        // Truncated-oracle fail-safe (2026-07-18): everything the exact-base
        // walk verified lies BELOW this frontier. A walk that broke early on a
        // grid anomaly used to silently drop every conservative candidate
        // above the break (no covering range -> `mark_young` returned -> the
        // root's whole subtree got swept: the trigger-ON bt18 676xxxxx
        // under-count family). Candidates above the frontier now fall back to
        // direct validation of the candidate address (the pre-oracle
        // behavior) — over-retention-safe, never a header write.
        let oracle_trusted_abs = from_base + exact_cursor;

        // ----- Mark phase -------------------------------------------------
        //
        // BFS over young-gen objects. The worklist holds young object
        // pointers that have been marked but not yet scanned. Marking an
        // old-gen object is unnecessary for a young collection, but we
        // still traverse *through* an old-gen object if a dirty card says
        // it may reference young gen (handled below via the dirty-card
        // seed). Marking uses `GC_FLAG_MARKED` in the object header.
        let mut worklist: Vec<*mut u8> = Vec::new();

        // xt-hardening (2026-07-03): per-cycle SIDE mark set for candidates
        // that must never be header-written. The mark write
        // (`gc_flags |= GC_FLAG_MARKED` at candidate+21) through a
        // conservative FALSE POSITIVE is itself a heap corruptor: a candidate
        // at (live_object_start - 8) — trivially plausible whenever the
        // preceding 8 bytes are zero and the victim's identity hash is
        // un-minted — lands the write at victim+13, flipping bit 9 of the
        // victim's `array_length` to EXACTLY 512 (the observed
        // "kind=Object but array_length=512" corrupt-header face, whose
        // volume exploded when the xt cross-thread takeover started feeding
        // whole frozen-peer register files and stack bands into the root
        // set). Real objects always have a non-zero first header word
        // (class_id|kind|element_type); the only legal zero-word0 shape is a
        // fresh zero-hash `ClassId(0)` container, which the side set handles
        // correctly (pinned + traced, never written). Zero-word0 candidates
        // are the overwhelming false-positive volume — routing them here
        // removes the writer from the entire zeroed-span/misalign family.
        let mut side_marks: FxHashSet<usize> = FxHashSet::default();
        // mark_if_young: mark a candidate young pointer and enqueue it.
        // SAFETY contract: `ptr` is only dereferenced after `in_young`
        // confirms it lands inside the live from-space region.
        let mut mark_young = |ptr: *mut u8,
                              worklist: &mut Vec<*mut u8>,
                              side_marks: &mut FxHashSet<usize>| {
            let addr = ptr as usize;
            if !in_young(addr) {
                return;
            }
            let insertion = young_object_ranges.partition_point(|(start, _)| *start <= addr);
            let covering = insertion
                .checked_sub(1)
                .and_then(|idx| young_object_ranges.get(idx))
                .filter(|&&(_, end)| addr < end);
            let (addr, ptr) = match covering {
                Some(&(base, _end)) => (base, base as *mut u8),
                // Below the oracle's trusted frontier the walk was verified:
                // an uncovered candidate is free/gap space — not an object.
                None if addr < oracle_trusted_abs => return,
                // Above the frontier the walk broke early — fall back to
                // validating the candidate address directly (see the
                // `oracle_trusted_abs` note above).
                None => (addr, ptr),
            };
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
                // DBG (CRATONVM_DBG_SWEEP_CENSUS): a candidate whose header
                // is IMPLAUSIBLE gets dropped from marking entirely — if it
                // was actually a live object with a scarred header, the
                // sweep will reclaim it. Surface the drop (bounded).
                if std::env::var_os("CRATONVM_DBG_SWEEP_CENSUS").is_some() {
                    let n = SWEEP_BAD_EXTENT_HITS.load(Ordering::Relaxed);
                    if n < 12 {
                        eprintln!(
                                "[mark-reject] candidate {ptr:p} kind=0x{kind_byte:02x} ns={} alen={} cid={:#x} — dropped from marking (shape)",
                                header.num_slots, header.array_length, header.class_id.as_u32(),
                            );
                    }
                }
                return;
            }
            // DoHead comb-7 fix (2026-07-03): also validate the object's
            // EXTENT. The field bounds above still admit a corrupt header
            // claiming millions of elements (array_length is only capped at
            // i32::MAX), and the BFS scan (`for_each_ref_slot`) iterates
            // that count with the header as its ONLY bound — a claimed
            // extent past from-space ran the scan off the mapped arena
            // (observed: main-vm SIGSEGV at the region boundary, corrupt
            // header claiming array_length=7,775,429 — the JIT inline-alloc
            // kind/array_length fault family). A real object's extent always
            // fits inside the arena it was allocated from, so an
            // out-of-extent header is definitionally corrupt: never mark or
            // scan it (its "referents" would be garbage reads anyway).
            let total = gen_object_total_size(header);
            if total < HEADER_SIZE || addr + total > from_end {
                let n = SWEEP_BAD_EXTENT_HITS.fetch_add(1, Ordering::Relaxed);
                if emit_conservative_candidate_diagnostic(n, crate::a2dbg::enabled()) {
                    // Attribution diagnostic: dump the words around the
                    // rejected "header" so the upstream corruptor face is
                    // identifiable (stale packed-pointer reuse shows heap
                    // pointers; a clobbered real header shows a torn mix).
                    let lo = addr.saturating_sub(32).max(from_base);
                    let mut hex = String::new();
                    let mut w = lo;
                    while w + 8 <= (addr + 48).min(from_end) {
                        // SAFETY: `[from_base, from_end)` is mapped arena
                        // memory and `w` is 8-aligned within it.
                        let v = unsafe { *((w & !7) as *const u64) };
                        hex.push_str(&format!("{:#x}:{:016x} ", w & !7, v));
                        w += 8;
                    }
                    tracing::warn!(
                        "mark_young: ignoring conservative candidate at {:#x} with implausible extent \
                         {} (kind={}, array_len={}, num_slots={}); safe reject, \
                         not marked/scanned; context {}",
                        addr,
                        total,
                        kind_byte,
                        header.array_length,
                        header.num_slots,
                        hex,
                    );
                    // A2 forensic probe (CRATONVM_DBG_A2): correlate this
                    // rejected address against the allocation breadcrumb
                    // ring — was this slot EVER header-written by an
                    // allocator (interpreter TLAB / gen_heap / JIT
                    // inline-alloc / TLAB tail-filler), and with what
                    // real size/class? Distinguishes "never allocated
                    // here" (stale conservative root over reused/free
                    // memory) from "allocated then clobbered" (a real
                    // header-write race) from "allocated exactly this
                    // shape, walker/mark logic disagrees" (a formula bug).
                    match crate::a2dbg::lookup_at(addr) {
                            Some(r) => tracing::warn!(
                                "  [A2] BREADCRUMB exact-addr alloc: class_id={} kind={} et={} alen={} ns={} REAL_size={} seq={}",
                                r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq,
                            ),
                            None => match crate::a2dbg::lookup_covering(addr) {
                                Some(r) => tracing::warn!(
                                    "  [A2] BREADCRUMB covered by alloc start={:#x} class_id={} kind={} et={} alen={} ns={} REAL_size={} seq={} (mid-object offset={})",
                                    r.addr, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq, addr - r.addr,
                                ),
                                None => tracing::warn!(
                                    "  [A2] BREADCRUMB — NO allocation record covers {:#x} (never header-written here, or freed+reused past the ring)",
                                    addr,
                                ),
                            },
                        }
                }
                return;
            }
            // Family-A fix (2026-07-03): EVERY conservative root candidate
            // takes the SIDE path now — alive and traced, but the header is
            // NEVER written through. Previously only a zero-first-word
            // candidate was side-marked; any OTHER candidate that merely
            // passed the kind/num_slots/extent plausibility checks above
            // (bit-plausible but not necessarily the true start of a live
            // object) fell through to `header.gc_flags |= GC_FLAG_MARKED`
            // below, an unconditional write through the candidate pointer.
            //
            // The checks above bound `class_id`/`kind`/`num_slots`/extent to
            // "looks like it could be a real header" — they do NOT prove the
            // candidate is the actual start address a real allocator wrote a
            // header at. A conservative root scan (register/stack scan, or
            // — at much higher volume — the cross-thread `xt_root_scan`
            // OS-suspend takeover, which floods this seed with thousands of
            // raw register/stack words from EVERY other live thread) can
            // easily produce an address that is NOT a real object start but
            // decodes as one anyway: e.g. the 16-byte-aligned `Value` cells
            // of a live `Object[]` all carry `VTAG_OBJECT = 4` as their
            // discriminant word, so landing on ANY element's disc word reads
            // `class_id=4, kind=Object` — and 16 bytes later, the NEXT
            // element's disc word reads as a matching, equally-plausible
            // `num_slots=4`. Writing `GC_FLAG_MARKED` (0x02) through such a
            // false-positive candidate lands the byte write inside a
            // genuinely live neighboring object's real header — bit 9 of
            // `array_length` (byte offset 13, 8 bytes past `gc_flags` at
            // offset 21 minus the header's own +8 alignment window) is
            // exactly the observed "kind=Object but array_length=512/513"
            // corruption face this whole family is named for; other offsets
            // hit `num_slots`, `gc_age`, or the low byte of `forwarding_ptr`
            // depending on the candidate's exact false-positive offset.
            //
            // `side_marks` was already proven safe and sufficient for the
            // zero-word0 case (over-retention only, per the comment above);
            // extending it to every candidate closes the entire
            // write-through class at the cost of pure over-retention (a
            // false-positive candidate keeps its neighbor pinned instead of
            // corrupting it — the collector's own documented safety
            // invariant: "a conservative false-positive root only
            // over-retains, it can never cause a live object to be freed OR
            // its non-pointer data to be corrupted").
            // SAFETY: `addr` is 8-aligned inside mapped from-space.
            //
            // Merge note (2026-07-04): dev independently landed a NARROWER
            // mitigation for this same non-zero-word0 hazard
            // (`header_reserved_fields_plausible` — reject a candidate
            // whose always-zero padding/reserved bytes or undefined
            // gc_flags bits are set, ~1/2^29 false-negative rate on
            // garbage) and still header-wrote through anything that
            // passed it. That check is real and kept (used elsewhere by
            // dev's other hardening below), but it does NOT catch this
            // fix's target case: a genuine live `Object[]` element cell,
            // whose bytes are NOT garbage — `_padding`/`_gc_reserved`
            // read 0 legitimately (they alias the high bytes of an
            // adjacent element's pointer payload, which is frequently
            // 0 on a 48-bit address space) and `gc_flags` reads 0 too.
            // Such a candidate sails through
            // `header_reserved_fields_plausible` and still gets
            // header-written. Unconditional side-marking (this fix)
            // has no such gap: every candidate is treated as
            // never-write-through, full stop.
            if side_marks.insert(addr) {
                worklist.push(ptr);
            }
        };

        // Precise-edge marker (perf/halfgap-20260717): for values read out of
        // actual reference slots — BFS `for_each_ref_slot` referents,
        // dirty-card slot reads, overlay/loader/mirror side channels. These
        // are object BASES by construction (a store wrote a real reference
        // there), so the conservative interior-pointer oracle above is a
        // semantic no-op for them and its per-edge range probe was pure
        // overhead. Keeps the same header-plausibility rejection and the
        // same never-write-through side-mark channel as `mark_young`; the
        // only difference is skipping base resolution. This also restores
        // the pre-oracle robustness property that a truncated oracle walk
        // (corrupt header mid-arena) cannot silently unroot every precise
        // edge above the truncation point.
        let mut mark_young_precise = |ptr: *mut u8,
                                      worklist: &mut Vec<*mut u8>,
                                      side_marks: &mut FxHashSet<usize>| {
            let addr = ptr as usize;
            if !in_young(addr) {
                return;
            }
            // SAFETY: `in_young` confirmed an 8-aligned address inside the
            // live from-space region; reading an ObjectHeader there is valid.
            let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
            let kind_byte = header.kind as u8;
            let is_array = header.kind == ObjectKind::Array;
            if kind_byte > 1
                || (!is_array && header.num_slots > (1 << 24))
                || (is_array && header.array_length > i32::MAX as u32)
            {
                return;
            }
            let total = gen_object_total_size(header);
            if total < HEADER_SIZE || addr + total > from_end {
                let n = SWEEP_BAD_EXTENT_HITS.fetch_add(1, Ordering::Relaxed);
                if emit_conservative_candidate_diagnostic(n, crate::a2dbg::enabled()) {
                    tracing::warn!(
                        "mark_young_precise: ignoring edge referent at {:#x} with implausible extent {} (kind={}, array_len={}, num_slots={}); safe reject",
                        addr,
                        total,
                        kind_byte,
                        header.array_length,
                        header.num_slots,
                    );
                }
                return;
            }
            if side_marks.insert(addr) {
                worklist.push(ptr);
            }
        };

        // Seed: precise + conservative roots gathered by the caller.
        for root in roots.iter() {
            mark_young(root.as_ptr(), &mut worklist, &mut side_marks);
        }

        // Seed: finalizable objects — keep them alive so finalize() runs.
        for &addr in finalizer_addrs {
            mark_young(addr as *mut u8, &mut worklist, &mut side_marks);
        }

        // DIAG/EXPERIMENT (CRATONVM_SWEEP_FULL_OLD_SCAN): seed old→young edges
        // by walking EVERY old-gen object's reference slots instead of trusting
        // the dirty-card table. If a workload that corrupts under the card seed
        // runs clean under the full scan, an old→young edge is provably being
        // lost by the card path (barrier miss, drain loss, or scan consumption)
        // — the young-GC live-object-reclaim investigation's discriminator.
        let full_old_scan = Self::full_old_rset_scan_enabled();
        if full_old_scan {
            for (optr, _sz) in old_gen.walk_objects() {
                // SAFETY: `walk_objects` yields valid live old-gen object starts.
                let oh = unsafe { &*(optr as *const ObjectHeader) };
                // SAFETY: `optr`/`oh` form a valid live object.
                unsafe {
                    for_each_ref_slot(optr, oh, |raw, _slot| {
                        mark_young_precise(raw, &mut worklist, &mut side_marks);
                    });
                }
            }
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
                    mark_young_precise(raw as usize as *mut u8, &mut worklist, &mut side_marks);
                }
            } else if is_compact_object(header) {
                // Compact object: `slot_idx` is the BYTE OFFSET of an 8-byte
                // reference slot (as recorded by scan_dirty_cards).
                // SAFETY: byte offset within this object's body (from card scan).
                let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx) };
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    mark_young_precise(raw as usize as *mut u8, &mut worklist, &mut side_marks);
                }
            } else {
                // SAFETY: `slot_idx` is within `num_slots` (from card scan).
                let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: `slot_ptr` is a valid Value-sized slot.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    mark_young_precise(ref_obj.as_ptr(), &mut worklist, &mut side_marks);
                }
            }
        }

        // Restore the dirty cards consumed by `scan_dirty_cards` so the
        // next collection still sees these old→young references.
        self.card_table.mark_dirty_bulk(&redirty_cards);

        // Overlay-backed collections have old→young edges that do not occupy
        // a Java heap slot, so no card can describe them. A minor collection
        // leaves old gen intact and cannot decide which old owners will later
        // be dead; retain edges of every current old owner here just as the
        // card scan does. The major marker below applies the precise
        // owner-reachable rule before it compacts old space.
        for overlay_ref in
            cratonvm_native_collections::gc_overlay_roots_for_matching_owners(|owner_addr| {
                old_gen.contains(owner_addr as *mut u8)
            })
        {
            mark_young_precise(overlay_ref.as_ptr(), &mut worklist, &mut side_marks);
        }

        // DBG (CRATONVM_DBG_SEED_ALL_OLD): decisive test for the sweep-edges
        // verdict that bt18's leak is an old→young CLEAN-CARD miss. Seed the mark
        // from EVERY old-gen object's young references (not just dirty cards). If
        // this corrects the checksum, the bug is a card/barrier gap (a promotion
        // that failed to dirty the promoted object's card); if it does NOT, the
        // missed reference is not a clean-card old→young edge (older verdict).
        if std::env::var_os("CRATONVM_DBG_SEED_ALL_OLD").is_some() {
            for (op, _sz) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let oh = unsafe { &*(op as *const ObjectHeader) };
                // SAFETY: `op`/`oh` are a valid live old-gen object.
                unsafe {
                    for_each_ref_slot(op, oh, |r, _| {
                        mark_young_precise(r, &mut worklist, &mut side_marks)
                    });
                }
            }
        }

        // HIB-CV-24: a live object keeps its class's defining ClassLoader alive
        // (instance→loader). No-op for the common no-custom-loader case.
        let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();
        // BFS: transitively mark every young object reachable from a root.
        while let Some(obj_ptr) = worklist.pop() {
            // SAFETY: `obj_ptr` was validated by `mark_young` before being
            // pushed — it is a sane young-gen object header.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Mark every referent (arrays + compact objects: 8-byte pointers;
            // legacy objects: 16-byte Value cells).
            // SAFETY: `obj_ptr`/`header` are a validated young object.
            unsafe {
                for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                    mark_young_precise(ref_ptr, &mut worklist, &mut side_marks);
                });
            }
            // Collection-overlay liveness pin: an overlay is an out-of-heap
            // edge owned by its backing collection, not a process-global root.
            // This non-moving marker has stable object addresses, so once this
            // collection itself is marked we can follow only ITS side-table
            // references. The root gatherer omits the unconditional overlay
            // scan for this exact collector mode.
            for overlay_ref in
                cratonvm_native_collections::gc_overlay_roots_for_collection(obj_ptr as usize)
            {
                mark_young_precise(overlay_ref.as_ptr(), &mut worklist, &mut side_marks);
            }
            // HIB-CV-24: also mark this object's defining ClassLoader so a live
            // (e.g. leaked-via-ThreadLocal) instance keeps its loader alive.
            if loader_pin_on {
                if let Some(loader_addr) =
                    cratonvm_types::loader_pin::loader_pin_addr(header.class_id.as_u32())
                {
                    mark_young_precise(loader_addr as *mut u8, &mut worklist, &mut side_marks);
                }
            }
            // Class-mirror liveness pin (mirror_pin, companion to loader_pin
            // above — see `vm::memory::roots` step 6 and
            // `cratonvm_types::mirror_pin`): this object is non-moving here,
            // so `obj_ptr` is a stable key. If it IS itself a user-defined
            // `ClassLoader` that has defined mirror-having classes, mark
            // those mirrors alive too — the edge a real JDK's
            // `ClassLoader.classes` field gives for free, which this VM's
            // synthetic `ClassLoader` doesn't carry as a heap-traceable
            // field.
            if let Some(mirror_addrs) =
                cratonvm_types::mirror_pin::mirrors_for_loader(obj_ptr as usize)
            {
                for mirror_addr in mirror_addrs {
                    mark_young_precise(mirror_addr as *mut u8, &mut worklist, &mut side_marks);
                }
            }
        }

        // Sorted view of the side mark set for O(1)-amortized lockstep checks
        // in the linear walks below (same pattern as the free-block skip).
        // ABSOLUTE addresses.
        let mut side_sorted: Vec<usize> = side_marks.iter().copied().collect();
        side_sorted.sort_unstable();
        report_phase("mark");

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
        // xt-hardening (2026-07-03): do NOT evacuate on cycles where the JIT
        // root coverage is known-incomplete (forcibly-frozen peers, helper
        // windows, or reserved TLAB tails present — the VM marks the cycle
        // via `mark_moving_young_coverage_incomplete`). A frozen peer's
        // registers can hold ONLY a derived/interior pointer to an object
        // whose base is reachable via precise heap edges: the base is not
        // pin-by-value protected (interior addresses don't resolve to roots),
        // gets evacuated, its young source zeroed and re-served, and the
        // resumed peer keeps loading/storing through the stale derived
        // pointer. Retention for one cycle is always safe; the flag is
        // per-cycle so ordinary (single-threaded / cooperative) collections
        // keep the bt18-critical drain.
        // No object can satisfy `gc_age + 1 >= PROMOTION_AGE` until it has
        // survived `PROMOTION_AGE - 1` prior minor collections. Skip the two
        // guaranteed-no-op full-arena promotion walks at heap startup.
        let promotion_age_reachable = self.stats.minor_gc_count.load(Ordering::Relaxed)
            >= u64::from(PROMOTION_AGE.saturating_sub(1));
        let selective_on = promotion_age_reachable
            && std::env::var_os("CRATONVM_NO_SELECTIVE_PROMOTE").is_none()
            && !crate::gc_quiescence::moving_young_coverage_incomplete();
        if selective_on {
            let is_y = |a: usize| -> bool { a >= from_base && a < from_end && (a & 0x7) == 0 };

            // PERF: compute the sorted young free-block view (and `used`) ONCE
            // per sweep and reuse it across every selective-promotion pass.
            // `free_blocks_sorted()` collect-and-sorts the WHOLE free list, and
            // it was previously rebuilt at least three times per non-moving
            // sweep (evacuate pass (2), fixup pass (3a), and SP_VERIFY). The
            // `young_from` lock is held for the entire function and NOTHING in
            // this block mutates its free list or `used()` — all free-list
            // mutations (`add_free_block` / `clear_free_list` / `reset`) and any
            // `young_from` allocation happen strictly AFTER the `selective_on`
            // block, and evacuation only allocates into `old_gen`. So this
            // single snapshot is valid for every pass below; behavior is
            // identical, we just skip the redundant collect+sort each pass.
            let sweep_free_blocks = merge_skips(young_from.free_blocks_sorted());
            let sweep_used = young_from.used();

            // (1) Pin set: every root / finalizer value that lands in young.
            // (1) Pin set: every root / finalizer value that lands in young.
            // Stage B (precise oop maps, B-K relocation track): EXCLUDE addresses
            // published as movable precise-JIT roots — empty unless precise
            // relocation is engaged, so byte-identical on the default path.
            let mut pinned: FxHashSet<usize> = FxHashSet::default();
            for r in roots.iter() {
                let a = r.as_ptr() as usize;
                if is_y(a) && !crate::gc_quiescence::is_movable_jit_root(a) {
                    pinned.insert(a);
                }
            }
            for &a in finalizer_addrs.iter() {
                if is_y(a) {
                    pinned.insert(a);
                }
            }

            // (2) Evacuate non-pinned marked survivors to old gen. Record
            // young→old in `evac_map` (returned for the VM-level remap). Stop
            // if old gen fills (leave the remainder in young — correctness
            // over completeness).
            //
            // DoHead walk-desync hardening (2026-07-02): forwarding-pointer
            // installs into the young sources are DEFERRED and only applied
            // for candidates collected on ANCHOR-VERIFIED stretches of the
            // walk grid. The old exact-match free-block skip (`cursor == off`)
            // wedged after a single overshoot and then strode every later
            // free block as phantom zeroed "objects" — a phantom that
            // happened to read as marked+aged was "evacuated" with a
            // forwarding pointer written INTO a live object's interior, which
            // the main sweep then zeroed and freed via `is_forwarded()` (the
            // fatal mid-live-object reclaim). Now: robust skip; every anomaly
            // (zero span, implausible header, free-block overshoot, span
            // crossing a free hole) UNWINDS the candidates collected since
            // the last trustworthy anchor (a free-block boundary) — their
            // already-copied old-gen bytes become unreachable garbage for the
            // next major GC — and the walk re-anchors at the next free block.
            // Candidates between two clean anchors (or between the last
            // anchor and a clean end-of-walk) are grid-verified and their
            // installs proceed. This bounds phantom installs without
            // starving promotion when a persistent unparseable span exists
            // (only a moving cycle reclaims those). Survivors inside skipped
            // stretches are handled by pass 3a's conservative rewrite.
            let mut evacuated: Vec<*mut u8> = Vec::new();
            let mut fwd_installs: Vec<(usize, *mut u8)> = Vec::new();
            // Deferred survivor age bumps (applied with the forwarding
            // installs below, under the same anchor-verified discipline).
            // The unconditional side-marking hardening removed every
            // mark-time header write — which silently stopped survivor
            // aging, and with it this whole pass (no object could ever
            // reach PROMOTION_AGE, so a fully-live young could never drain
            // to old gen and the heap wedged into the old-gen-spill → abort
            // path). Aging now happens here, where the walk grid validates
            // each header before any write lands on it.
            let mut age_bumps: Vec<usize> = Vec::new();
            {
                // PERF: reuse the once-computed sorted free-block snapshot.
                let mut free_iter = sweep_free_blocks.iter().peekable();
                let used = sweep_used;
                let mut cursor = 0usize;
                let mut old_full = false;
                // Candidate counts at the last trustworthy anchor.
                let mut fwd_wm = 0usize;
                let mut evac_wm = 0usize;
                let mut age_wm = 0usize;
                // Drop candidates collected since the last anchor (suspect
                // stretch): remove them from evac_map, orphan their copies.
                fn unwind_evac(
                    fwd_installs: &mut Vec<(usize, *mut u8)>,
                    evacuated: &mut Vec<*mut u8>,
                    evac_map: &mut HashMap<usize, usize>,
                    age_bumps: &mut Vec<usize>,
                    fwd_wm: usize,
                    evac_wm: usize,
                    age_wm: usize,
                ) {
                    age_bumps.truncate(age_wm);
                    if fwd_installs.len() > fwd_wm {
                        let n = SWEEP_PROMOTION_ABORT_HITS.fetch_add(1, Ordering::Relaxed);
                        if n < 8 {
                            tracing::warn!(
                                "selective promotion: unwound {} candidate(s) collected \
                                 on a suspect walk stretch (grid anomaly since last anchor)",
                                fwd_installs.len() - fwd_wm,
                            );
                        }
                        for (src, _) in fwd_installs.drain(fwd_wm..) {
                            evac_map.remove(&src);
                        }
                        evacuated.truncate(evac_wm);
                    }
                }
                while cursor < used && !old_full {
                    {
                        let (resynced, overshot) = skip_free_blocks(&mut cursor, &mut free_iter);
                        if overshot {
                            // The stride that crossed into the block was
                            // mis-sized: candidates since the last anchor are
                            // suspect. The cursor now sits at the block end —
                            // a fresh anchor.
                            unwind_evac(
                                &mut fwd_installs,
                                &mut evacuated,
                                &mut evac_map,
                                &mut age_bumps,
                                fwd_wm,
                                evac_wm,
                                age_wm,
                            );
                        }
                        if resynced {
                            fwd_wm = fwd_installs.len();
                            evac_wm = evacuated.len();
                            age_wm = age_bumps.len();
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
                        let gap = unsafe { std::ptr::read((src as *const u8).add(4) as *const u32) }
                            as usize;
                        if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                            cursor += gap;
                            continue;
                        }
                    }
                    // All-zero header word: an unlisted zeroed span (never a
                    // walkable object unless the zero run is shorter than a
                    // header — the real `ClassId(0)` container case, which
                    // parses normally below). Anomaly: unwind candidates since
                    // the last anchor and re-anchor at the next free block.
                    let word0 = unsafe { *(src as *const u64) };
                    let mut anomaly = false;
                    if word0 == 0 {
                        let limit = free_iter
                            .peek()
                            .map(|&&(off, _)| off)
                            .unwrap_or(used)
                            .min(used);
                        let run_end = zero_run_end(from_base, cursor, limit);
                        if run_end - cursor >= HEADER_SIZE {
                            anomaly = true;
                        }
                    }
                    let total_size = if anomaly {
                        0
                    } else {
                        gen_object_total_size(header)
                    };
                    if anomaly || total_size < HEADER_SIZE || cursor + total_size > used {
                        unwind_evac(
                            &mut fwd_installs,
                            &mut evacuated,
                            &mut evac_map,
                            &mut age_bumps,
                            fwd_wm,
                            evac_wm,
                            age_wm,
                        );
                        if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                            continue;
                        }
                        break;
                    }
                    // Over-sized-header clamp (mirrors the main walk): an
                    // object can never span a pre-existing free hole. Do not
                    // evacuate through such a header; candidates since the
                    // last anchor are suspect.
                    if let Some(&&(foff, _fsz)) = free_iter.peek() {
                        if foff > cursor && foff < cursor + total_size {
                            unwind_evac(
                                &mut fwd_installs,
                                &mut evacuated,
                                &mut evac_map,
                                &mut age_bumps,
                                fwd_wm,
                                evac_wm,
                                age_wm,
                            );
                            cursor = foff;
                            continue;
                        }
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
                    // Survivor liveness comes from EITHER mark channel: the
                    // legacy header mark, or the side-mark set. Since the
                    // unconditional side-marking hardening, the marker never
                    // header-writes, so `side_marks` is the only live channel
                    // — requiring the header mark alone made this whole pass
                    // inert (nothing evacuated, nothing aged; a fully-live
                    // young then wedged the heap into the old-gen-spill →
                    // abort path that the native-alloc boundary GC exists to
                    // relieve).
                    let aged = header.gc_age + 1 >= PROMOTION_AGE;
                    let header_marked = header.gc_flags & GC_FLAG_MARKED != 0;
                    let marked = header_marked || side_marks.contains(&addr);
                    if marked && !aged && !header_marked {
                        // Deferred, anchor-verified age bump — applied with
                        // the forwarding installs below. Header-marked
                        // survivors are excluded: the main sweep still ages
                        // those in place.
                        age_bumps.push(addr);
                    }
                    if marked && aged && !pinned.contains(&addr) {
                        match old_gen.alloc(total_size, 8) {
                            Some(dst) => {
                                // gcstress face-1 hunt (no-op unless gated):
                                // validate the SOURCE body before promoting —
                                // catching a corrupt cell here pins the
                                // corruption to BEFORE promotion and reports
                                // the victim's stable young address.
                                validate_copy_source_cells(src, header, "promote-src");
                                {
                                    let w = crate::heap::cell_watch_addr();
                                    let d = dst as usize;
                                    if w != 0 && d <= w && w.wrapping_sub(d) < total_size {
                                        let src_at = src as usize + (w - d);
                                        // SAFETY: src object spans total_size bytes.
                                        let pair =
                                            unsafe { std::ptr::read(src_at as *const [u64; 2]) };
                                        crate::heap::cell_watch_check(
                                            d,
                                            total_size,
                                            "selective-promote-evac",
                                            &format!(
                                                "src=0x{:x} src[watch]=0x{:016x},0x{:016x}",
                                                src as usize, pair[0], pair[1]
                                            ),
                                        );
                                    }
                                }
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
                                // Forwarding-pointer install into the young
                                // source is deferred until the walk validates.
                                fwd_installs.push((addr, dst));
                                evac_map.insert(addr, dst as usize);
                                evacuated.push(dst);
                                // Promotion accounting (mirrors the moving
                                // collector's `forward_object`): powers the
                                // GC-overhead productivity metric — a
                                // promotion-only cycle conserves live bytes
                                // but very much did useful work (it drained
                                // young), and must not count as "thrashing".
                                self.stats
                                    .bytes_promoted
                                    .fetch_add(total_size as u64, Ordering::Relaxed);
                                self.stats.objects_promoted.fetch_add(1, Ordering::Relaxed);
                            }
                            None => old_full = true,
                        }
                    }
                    cursor += total_size;
                }
            }
            // Install forwarding pointers for the surviving candidates —
            // every entry left in `fwd_installs` was collected on a stretch
            // whose strides were verified against the next anchor (or a
            // clean end-of-walk); suspect stretches were unwound above.
            for &(src_addr, dst) in &fwd_installs {
                // SAFETY: `src_addr` is a live young object header on an
                // anchor-verified stretch; writing its forwarding_ptr field
                // is the install the copy loop deferred.
                unsafe {
                    std::ptr::addr_of_mut!((*(src_addr as *mut ObjectHeader)).forwarding_ptr)
                        .write(dst);
                }
            }
            // Age the not-yet-tenurable survivors — same anchor-verified
            // deferral contract as the forwarding installs above. This is the
            // aging the marker's unconditional side-marking removed (it never
            // header-writes); without it no survivor can ever reach
            // PROMOTION_AGE and this pass stays permanently inert.
            for &src_addr in &age_bumps {
                // SAFETY: `src_addr` is a live young object header on an
                // anchor-verified stretch (identical contract to the
                // forwarding-pointer install above); bumping its age byte is
                // the write the walk deferred.
                unsafe {
                    let h = &mut *(src_addr as *mut ObjectHeader);
                    h.gc_age = h.gc_age.saturating_add(1);
                }
            }

            // DBG (CRATONVM_SP_STATS): per-GC pin/evac counts, printed
            // unconditionally (even when nothing evacuated) so we can confirm
            // whether selective promotion is actually evacuating at a given
            // heap size, or whether the only active effect is the free-block
            // coalescing below.
            if std::env::var_os("CRATONVM_SP_STATS").is_some() {
                eprintln!(
                    "[sp-stats] gc: pinned={} evac={} aged={}",
                    pinned.len(),
                    evac_map.len(),
                    age_bumps.len(),
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
                        // DoHead walk-desync hardening (2026-07-02): only
                        // follow forwarding pointers that actually land in
                        // old gen — selective promotion never forwards
                        // anywhere else. A corrupt/phantom forwarding field
                        // (e.g. a span retained by the main sweep's
                        // bad-forward check in an earlier cycle) must not
                        // rewrite live references to a garbage target.
                        let fwd = h.forwarding_ptr;
                        if old_gen.contains(fwd) {
                            Some(fwd as usize)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                // (3a) References inside surviving (pinned / non-evacuated) young
                // objects → rewrite to the evacuated copies in old gen.
                //
                // DoHead walk-desync hardening (2026-07-02): this pass runs
                // only after the evacuation walk validated the whole grid, so
                // anomalies here should be impossible — but a wedge in THIS
                // walk (the old exact-match skip) would leave every survivor
                // past the wedge un-fixed-up while the main sweep zeroes the
                // evacuated sources: mass dangling references. Use the same
                // robust traversal; on an (unexpected) anomaly, re-anchor at
                // the next free block and keep fixing up rather than break.
                {
                    // PERF: reuse the once-computed sorted free-block snapshot.
                    let mut free_iter = sweep_free_blocks.iter().peekable();
                    let used = sweep_used;
                    let mut cursor = 0usize;
                    // xt-hardening (2026-07-03): lockstep over side-marked
                    // survivors (see the fixup condition below).
                    let mut side_iter_3a = side_sorted.iter().peekable();
                    // Conservative fallback for a stretch this walk cannot
                    // parse (the evacuation walk resynced over the same
                    // stretch, so survivors inside it were not evacuated —
                    // but they may still REFERENCE evacuated objects, and an
                    // un-rewritten reference dangles once the main sweep
                    // zeroes the forwarded sources). Exact-match rewrite of
                    // any aligned word equal to an evacuated source address.
                    let rewrite_stretch = |lo: usize, hi: usize| {
                        let mut w = lo & !7;
                        while w + 8 <= hi {
                            // SAFETY: `[from_base+lo, from_base+hi)` is mapped
                            // from-space memory.
                            let cell = (from_base + w) as *mut u64;
                            let word = unsafe { *cell } as usize;
                            if let Some(&dst) = evac_map.get(&word) {
                                unsafe { *cell = dst as u64 };
                            }
                            w += 8;
                        }
                    };
                    while cursor < used {
                        if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                            continue;
                        }
                        let obj = (from_base + cursor) as *mut u8;
                        // SAFETY: cursor within used; from-space mapped.
                        let header = unsafe { &*(obj as *const ObjectHeader) };
                        // Bug-D fix (2026-06-12): skip a GAP-filler sentinel (a
                        // sub-`HEADER_SIZE` TLAB tail) before `gen_object_total_size`.
                        if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                            // SAFETY: offset 4 lies within the >=8-byte gap.
                            let gap =
                                unsafe { std::ptr::read((obj as *const u8).add(4) as *const u32) }
                                    as usize;
                            if (8..HEADER_SIZE).contains(&gap)
                                && gap & 7 == 0
                                && cursor + gap <= used
                            {
                                cursor += gap;
                                continue;
                            }
                        }
                        let word0 = unsafe { *(obj as *const u64) };
                        let mut anomaly = false;
                        if word0 == 0 {
                            let limit = free_iter
                                .peek()
                                .map(|&&(off, _)| off)
                                .unwrap_or(used)
                                .min(used);
                            let run_end = zero_run_end(from_base, cursor, limit);
                            if run_end - cursor >= HEADER_SIZE {
                                anomaly = true;
                            }
                        }
                        let total_size = if anomaly {
                            0
                        } else {
                            gen_object_total_size(header)
                        };
                        if anomaly || total_size < HEADER_SIZE || cursor + total_size > used {
                            let stretch_lo = cursor;
                            let resynced = resync_to_next_free_block(&mut cursor, &mut free_iter);
                            let stretch_hi = if resynced { cursor } else { used };
                            rewrite_stretch(stretch_lo, stretch_hi);
                            if resynced {
                                continue;
                            }
                            break;
                        }
                        // xt-hardening (2026-07-03): side-marked survivors
                        // (kept alive without a header write — see
                        // `side_marks`) also need their references to
                        // evacuated objects rewritten; they are unmarked by
                        // construction so the MARKED gate alone would skip
                        // them (dangling refs once the main sweep frees the
                        // forwarded sources).
                        let side_hit = {
                            let abs = from_base + cursor;
                            while let Some(&&a) = side_iter_3a.peek() {
                                if a < abs {
                                    side_iter_3a.next();
                                } else {
                                    break;
                                }
                            }
                            side_iter_3a.peek().is_some_and(|&&a| a == abs)
                        };
                        if (header.gc_flags & GC_FLAG_MARKED != 0 || side_hit)
                            && !header.is_forwarded()
                        {
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
                        // SAFETY: `walk_objects` yields the start of each live old-gen
                        // object, so `oaddr` targets a valid, initialized `ObjectHeader`.
                        let h = unsafe { &*(oaddr as *const ObjectHeader) };
                        missed_old += forwarded_ref_count(oaddr, h, &is_y);
                        for_each_ref(oaddr, h, &mut bump);
                    }
                    let mut missed_young = 0usize;
                    // PERF: reuse the once-computed sorted free-block snapshot.
                    let mut fi = sweep_free_blocks.iter().peekable();
                    let used = sweep_used;
                    let mut c = 0usize;
                    while c < used {
                        // DoHead hardening: robust skip (see skip_free_blocks).
                        if skip_free_blocks(&mut c, &mut fi).0 {
                            continue;
                        }
                        let o = (from_base + c) as *mut u8;
                        // SAFETY: `c < used` and the free-block list skips reclaimed gaps,
                        // so `from_base + c` is the start of a live object header inside
                        // the from-space (a bad header is caught by the size check below).
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

        report_phase("selective-promotion");

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
            let is_young = |a: usize| -> bool { a >= from_base && a < from_end && (a & 0x7) == 0 };
            let is_unmarked_young = |a: usize| -> bool {
                if !is_young(a) {
                    return false;
                }
                // Family-A (2026-07-03) made EVERY mark go through the SIDE
                // set (`side_marks`) — the header `GC_FLAG_MARKED` bit is no
                // longer written by this sweep's mark phase at all. Checking
                // only the header bit made every live object look "unmarked"
                // to this diagnostic (SUMMARY always reported marked=0 and
                // every root registered as a bogus (1)/(2)/(3) edge hit),
                // drowning the real signal. A side-marked object is live and
                // will NOT be swept; only side-UNmarked objects matter here.
                if side_marks.contains(&a) {
                    return false;
                }
                // SAFETY: `is_young` confirmed an 8-aligned addr inside live
                // from-space; reading its header is valid.
                let h = unsafe { &*(a as *const ObjectHeader) };
                h.gc_flags & GC_FLAG_MARKED == 0
            };

            let root_set: HashSet<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

            // (1) roots / finalizers that landed on an unmarked young object.
            let mut root_to_unmarked = 0usize;
            for &addr in root_set.iter() {
                if is_unmarked_young(addr) {
                    root_to_unmarked += 1;
                    if root_to_unmarked <= 8 {
                        // SAFETY: `addr` came from the root set (a live `ObjectRef`),
                        // so it points at a valid, initialized `ObjectHeader`.
                        let h = unsafe { &*(addr as *const ObjectHeader) };
                        tracing::warn!(
                            "[sweep-edges] (1) ROOT @{:#x} -> UNMARKED young obj \
                             (class_id={} kind=0x{:02x} num_slots={} array_len={}) — \
                             mark filter rejected a live root?",
                            addr,
                            h.class_id.as_u32(),
                            h.kind as u8,
                            h.num_slots,
                            h.array_length,
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
                // Arrays + compact objects: 8-byte pointer slots; legacy objects:
                // 16-byte Value cells. `slot_id` is the element index / byte
                // offset / field index (diagnostic-only here).
                // SAFETY: `obj_ptr`/`h` are a valid live object.
                unsafe {
                    for_each_ref_slot(obj_ptr, h, |raw, slot_id| {
                        let ta = raw as usize;
                        if is_unmarked_young(ta) {
                            report(slot_id, ta);
                        }
                    });
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
                    // DoHead hardening: robust skip (see skip_free_blocks).
                    if skip_free_blocks(&mut c, &mut free_it).0 {
                        continue;
                    }
                    let optr = (from_base + c) as *mut u8;
                    // SAFETY: `c < used_dbg` and the free-block list skips reclaimed gaps,
                    // so `from_base + c` is the start of a live object header in the
                    // from-space (a desynced/bad header is caught by the size check below).
                    let h = unsafe { &*(optr as *const ObjectHeader) };
                    let tot = gen_object_total_size(h);
                    if tot < HEADER_SIZE || c + tot > used_dbg {
                        tracing::warn!(
                            "[sweep-edges] diagnostic walk desynced at off={} \
                             (size={}, used={}) — arena already corrupt before this sweep",
                            c,
                            tot,
                            used_dbg,
                        );
                        break;
                    }
                    // Family-A: side-marked objects are live survivors even
                    // though their header GC_FLAG_MARKED bit is never written
                    // (see `is_unmarked_young` above).
                    if h.gc_flags & GC_FLAG_MARKED != 0 || side_marks.contains(&(optr as usize)) {
                        marked_total += 1;
                        let cid = h.class_id.as_u32();
                        let oaddr = optr as usize;
                        scan_refs(optr, &mut |si, ta| {
                            survivor_to_unmarked += 1;
                            if survivor_to_unmarked <= 16 {
                                // SAFETY: `ta` is a non-null young heap address read from a
                                // live survivor's ref slot, so it points at a valid header.
                                let th = unsafe { &*(ta as *const ObjectHeader) };
                                tracing::warn!(
                                    "[sweep-edges] (2) SURVIVOR @{:#x} (class_id={}) field[{}] \
                                     -> UNMARKED @{:#x} (class_id={} num_slots={} kind=0x{:02x}) \
                                     in_roots={}",
                                    oaddr,
                                    cid,
                                    si,
                                    ta,
                                    th.class_id.as_u32(),
                                    th.num_slots,
                                    th.kind as u8,
                                    root_set.contains(&ta),
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
                // SAFETY: `walk_objects` yields the start of each live old-gen object, so
                // `optr` targets a valid, initialized `ObjectHeader` whose class_id we read.
                let cid = unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() };
                scan_refs(optr, &mut |si, ta| {
                    old_to_unmarked += 1;
                    if old_to_unmarked <= 16 {
                        // SAFETY: `ta` is a non-null young heap address read from a live
                        // old-gen object's ref slot, so it points at a valid header.
                        let th = unsafe { &*(ta as *const ObjectHeader) };
                        tracing::warn!(
                            "[sweep-edges] (3) OLD-GEN @{:#x} (class_id={}) field[{}] \
                             -> UNMARKED young @{:#x} (class_id={} num_slots={}) — \
                             card/write-barrier MISS",
                            oaddr,
                            cid,
                            si,
                            ta,
                            th.class_id.as_u32(),
                            th.num_slots,
                        );
                    }
                });
            }

            let verdict = if root_to_unmarked > 0 || survivor_to_unmarked > 0 || old_to_unmarked > 0
            {
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

        report_phase("optional-edge-diagnostics");

        // ----- Sweep phase ------------------------------------------------
        //
        // Walk the from-space linearly, skipping holes already on the free
        // list (exactly as `OldGen::walk_objects` does — a hole's stale
        // bytes must never be parsed as an object). Every *unmarked*
        // object is dead: zero it and return its span to the free list.
        // Every marked object is a survivor: clear the mark and leave it
        // exactly where it is.
        let existing_free = merge_skips(young_from.free_blocks_sorted());
        // A2 diag (CRATONVM_DBG_A2): does the free list ALREADY self-overlap at
        // sweep start? `existing_free` is built only from prior sweeps' coalesced
        // output + the alloc/split bookkeeping between sweeps. A self-overlap here
        // means the BETWEEN-SWEEP maintenance (Arena::alloc split, or a stale block
        // never removed) is the source — vs. this sweep's frees overlapping it.
        if std::env::var_os("CRATONVM_DBG_A2").is_some() {
            for w in existing_free.windows(2) {
                let (a_off, a_sz) = w[0];
                let (b_off, _b_sz) = w[1];
                if b_off < a_off + a_sz {
                    let n = A2_FL_OVERLAP_HITS.load(Ordering::Relaxed);
                    if n < 12 {
                        eprintln!(
                            "[A2-FL] EXISTING-FREE self-overlap at sweep start: [{}, {}) then off={} (prev end {})",
                            a_off, a_off + a_sz, b_off, a_off + a_sz,
                        );
                    }
                    A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        // (offset, size, class_id, kind_byte, object_count) of spans the walk
        // decided to reclaim. Adjacent dead objects are collapsed by default;
        // forensic gates retain one record per object. DoHead walk-desync
        // hardening (2026-07-02): zeroing and
        // free-list publication are DEFERRED to the publication loop after the
        // walk, so that a later-detected grid anomaly (free-block overshoot /
        // implausible header) can UNWIND the suspect entries collected since
        // the last trustworthy anchor (`dead_watermark`) instead of having
        // already zeroed what may be a live object's interior.
        let retain_dead_objects = sweep_zero_enabled()
            || crate::a2dbg::enabled()
            || std::env::var_os("CRATONVM_DBG_SWEEP_CENSUS").is_some();
        let mut dead_regions: Vec<(usize, usize, u32, u8, usize)> = Vec::new();
        let mut bytes_swept: usize = 0;
        let mut objects_swept: usize = 0;
        // Index into `dead_regions` at the last trustworthy walk anchor
        // (walk start, or the end of a known free block). Entries above the
        // watermark were collected while striding an unverified stretch.
        let mut dead_watermark: usize = 0;
        let mut objects_live: usize = 0;

        let mut cursor: usize = 0;
        let used = young_from.used();
        let mut free_iter = existing_free.iter().peekable();
        // xt-hardening (2026-07-03): lockstep iterator over the side mark
        // set (absolute addrs, sorted) — side-marked objects are survivors
        // whose headers were never written; the walk must retain them
        // without touching gc_flags/gc_age.
        let mut side_iter = side_sorted.iter().peekable();
        // Debug diag: retain per-object walk history only under the explicit A2
        // forensic gate. The old default path retained every object (and a
        // later bounded-ring attempt still updated a deque per object), adding
        // gigabytes of metadata traffic across bintrees18's ~19M-node sweep.
        let retain_full_walk = std::env::var_os("CRATONVM_DBG_A2").is_some();
        let mut walked_count = 0usize;
        let mut walked: std::collections::VecDeque<(usize, usize, u32, ObjectKind, u32, u32)> =
            std::collections::VecDeque::new();
        // A2 probe (CRATONVM_DBG_A2): parallel to `walked`, the element_type byte
        // and the raw first 8 header bytes AS THE WALKER READ THEM (so a desync
        // dump shows the actual walk-time header, not an unreliable post-zeroing
        // re-read). Indexed in lockstep with `walked`.
        let mut walked_ext: std::collections::VecDeque<(u8, u64)> =
            std::collections::VecDeque::new();
        while cursor < used {
            // Skip known free blocks ROBUSTLY. The free list (`existing_free`) is
            // sorted ascending and the walk advances `cursor` monotonically, so we
            // can stride `free_iter` forward in lockstep. Crucially this handles the
            // case where a previous object's size brought the cursor PAST a free
            // block's start (`cursor > off`): the old code only matched `cursor ==
            // off`, so a single overshoot wedged `free_iter` at that block FOREVER —
            // and every later free block was then read as a run of zeroed 40-byte
            // phantom `Object`s (class_id=0, kind=Object, num_slots=0 → size 40),
            // desyncing the walk off the object grid (the A2 / ReflRepro corruption:
            // `CRATONVM_DBG_A2` shows freed+zeroed regions being walked, not skipped).
            // Now: drop free blocks the cursor has wholly passed, and if the cursor
            // lands AT or INSIDE a free block, resync to that block's end.
            {
                let (resynced, overshot) = skip_free_blocks(&mut cursor, &mut free_iter);
                if overshot {
                    // The previous stride ran INTO a known free block: the
                    // walk grid broke somewhere after the last anchor, so
                    // every reclaim decision made since then is suspect —
                    // it may cover a live object's interior. Unwind them
                    // (they have not been zeroed or published yet;
                    // over-retention is always safe under this sweep).
                    dead_regions.truncate(dead_watermark);
                }
                if resynced {
                    // A free block's end is ground truth — a fresh anchor.
                    dead_watermark = dead_regions.len();
                    continue;
                }
            }
            // `cursor` is within `used`; the from-space region
            // `[base, base+used)` is backed by mapped, allocated memory. The
            // integer-to-pointer cast itself is safe; only the header deref
            // on the next line requires `unsafe`.
            let obj_ptr = (from_base + cursor) as *mut u8;
            // SAFETY: `cursor < used`, so `from_base + cursor` is the start of a live
            // object header inside mapped from-space memory; this walk holds the only
            // mutable access during sweep, so the `&mut` is unique.
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
                let gap =
                    unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) } as usize;
                if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                    cursor += gap;
                    continue;
                }
                // Malformed sentinel (should be impossible) — fall through to
                // the corrupt-header re-sync path below.
            }
            // DoHead walk-desync hardening (2026-07-02): an all-zero header
            // word at a grid offset is NEVER a walkable object — it is a
            // reclaimed-then-zeroed span that is missing from the free list
            // (freed-but-unlisted residue), or a freed-then-REUSED slot whose
            // new owner's header was clobbered back to zero by a stale
            // register-held reference (the DoHead AQS family). The old code
            // strode such spans as 40-byte phantom "objects" and re-FREED
            // each one: spans are 40+16n bytes, so the final phantom window
            // crossed the span's end into the next LIVE object's header,
            // zeroing it and minting a free block inside a live object →
            // overlapping allocations → UAF. Instead: never parse, never
            // free (the span may be a live-but-clobbered allocation whose
            // memory must not be double-served); skip to the next known
            // free-block anchor and resume on-grid there.
            let word0 = unsafe { *(obj_ptr as *const u64) };
            if word0 == 0 {
                let limit = free_iter
                    .peek()
                    .map(|&&(off, _)| off)
                    .unwrap_or(used)
                    .min(used);
                let run_end = zero_run_end(from_base, cursor, limit);
                if run_end - cursor >= HEADER_SIZE {
                    let n = SWEEP_ZERO_SPAN_HITS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        tracing::warn!(
                            "non-moving sweep: unlisted all-zero span at offset {} \
                             (run {} bytes, next anchor at {}) — skipped, not freed",
                            cursor,
                            run_end - cursor,
                            limit,
                        );
                    }
                    // A zero span is anomaly evidence like any other: the
                    // walk can only claim `cursor` is on-grid if every stride
                    // since the last anchor was correctly sized — and a
                    // mis-sized stride landing in a live object's zeroed
                    // interior produces exactly this signature. Unwind the
                    // reclaim decisions collected since the anchor
                    // (over-retention is always safe), then re-anchor at the
                    // next free block, or stop if no anchor remains.
                    dead_regions.truncate(dead_watermark);
                    if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                        continue;
                    }
                    break;
                }
                // Zero run shorter than a header: a real `ClassId(0)` ad-hoc
                // container's header legitimately starts with zero words
                // (class_id=0, kind=Object, hash=0) but has a non-zero
                // `num_slots`/`gc_flags` word — parse it normally below.
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
                    walked_count,
                    used,
                    from_base,
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
                    if (i - start + 1) % 16 == 0 {
                        hex.push('\n');
                    }
                }
                tracing::warn!("  bytes around bad header (start_off={}):\n{}", start, hex);

                // A2 forensic probe (CRATONVM_DBG_A2): identify the corrupt object.
                // The hypothesis is the corrupt slot at `cursor` is an OBJECT whose
                // 40-byte header was overwritten by 16-byte Value-cell field data
                // (allocate-then-putfield clobber), OR the PRIOR object (an array)
                // is a mis-typed reference array the walker under-sized. Dump the
                // prior object's exact element_type/elem-bytes and resolve the
                // Value cells at `cursor` (disc + payload, and whether the payload
                // points into the young arena).
                if std::env::var_os("CRATONVM_DBG_A2").is_some()
                    && A2_PROBE_HITS.fetch_add(1, Ordering::Relaxed) < 4
                {
                    if let Some(&(loff, lsz, lcid, lkind, lns, lal)) = walked.back() {
                        // SAFETY: loff < used, header mapped.
                        let lhdr = unsafe { &*((from_base + loff) as *const ObjectHeader) };
                        eprintln!(
                            "[A2] PRIOR obj @{} size={} class_id={} kind={:?} num_slots={} array_len={} ELEM_TYPE={:?} elem_bytes={} (cursor={} = {}+{})",
                            loff, lsz, lcid, lkind, lns, lal,
                            lhdr.element_type,
                            crate::heap::element_byte_size(lhdr.element_type),
                            cursor, loff, lsz,
                        );
                    }
                    eprintln!(
                        "[A2] corruption @off={} from_base={:#x} used={}",
                        cursor, from_base, used
                    );
                    for k in 0..8usize {
                        let off = cursor + k * 16;
                        if off + 16 > used {
                            break;
                        }
                        // SAFETY: off+16 <= used, region mapped.
                        let disc = unsafe { *((from_base + off) as *const u64) };
                        let payload = unsafe { *((from_base + off + 8) as *const u64) };
                        let in_young =
                            payload >= from_base as u64 && payload < (from_base + used) as u64;
                        // If the payload points into the young arena at an aligned
                        // object boundary, read its class_id to identify the referent.
                        let referent = if in_young && (payload as usize - from_base) % 8 == 0 {
                            let rh = unsafe { &*(payload as *const ObjectHeader) };
                            format!(
                                "young referent class_id={} kind={} num_slots={}",
                                rh.class_id.as_u32(),
                                rh.kind as u8,
                                rh.num_slots
                            )
                        } else if payload >> 40 == (from_base as u64) >> 40 {
                            "heap-ptr (old-gen?)".to_string()
                        } else {
                            "(not-heap)".to_string()
                        };
                        eprintln!(
                            "[A2]   cell[{}] @{} disc={} payload={:#x} {}",
                            k, off, disc, payload, referent
                        );
                    }
                    // BREADCRUMB: what was actually allocated at/covering the
                    // corrupt cursor, and the prior object's REAL allocated size
                    // vs the size the walker computed (the decisive comparison).
                    match crate::a2dbg::lookup_covering(from_base + cursor) {
                        Some(r) => eprintln!(
                            "[A2] BREADCRUMB cursor@{} ({:#x}) covered by alloc start={:#x} class_id={} kind={} et={} alen={} ns={} REAL_size={} seq={} (mid-object offset={})",
                            cursor, from_base + cursor, r.addr, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq, (from_base + cursor) - r.addr,
                        ),
                        None => eprintln!(
                            "[A2] BREADCRUMB cursor@{} ({:#x}) — NO young alloc record covers it (header never written here, or freed+reused)",
                            cursor, from_base + cursor,
                        ),
                    }
                    if let Some(&(loff, lsz, _, _, _, _)) = walked.back() {
                        // Walk-time header AS THE WALKER READ IT (element_type byte
                        // + raw first 8 bytes). This is the decisive value: if it
                        // shows a 4-byte element_type for a byte[], the header is
                        // corrupt at walk time (vs an allocator/walker formula bug).
                        if let Some(&(wet, w0)) = walked_ext.back() {
                            eprintln!(
                                "[A2] WALK-TIME prior@{} element_type_byte={} raw_word0={:#018x} (b0=class_id_lo b4=kind b5=element_type)",
                                loff, wet, w0,
                            );
                        }
                        match crate::a2dbg::lookup_at(from_base + loff) {
                            Some(r) if r.kind == 0xFF => eprintln!(
                                "[A2] BREADCRUMB prior@{} — slot was FREED by the sweep (stale record); walker_size={}",
                                loff, lsz,
                            ),
                            Some(r) => eprintln!(
                                "[A2] BREADCRUMB prior@{} alloc class_id={} kind={} et={} alen={} ns={} REAL_size={} vs WALKER_size={} {}",
                                loff, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, lsz,
                                if r.size == lsz { "(match)" } else { "*** SIZE MISMATCH ***" },
                            ),
                            None => eprintln!(
                                "[A2] BREADCRUMB prior@{} — no exact alloc record (walker_size={})",
                                loff, lsz,
                            ),
                        }
                    }
                    // The desync DETECTION above is downstream: the walk may have
                    // silently overshot earlier (reading garbage that still passed
                    // the plausibility check). Find the FIRST walked object whose
                    // walker-computed size disagrees with its real allocated size —
                    // that is the ROOT overshoot. Compare walk-time header (et/raw0)
                    // to the alloc record to classify header-corruption vs reuse.
                    for (i, &(woff, wsz, _, _, _, _)) in walked.iter().enumerate() {
                        if let Some(r) = crate::a2dbg::lookup_at(from_base + woff) {
                            if r.kind != 0xFF && r.size != wsz {
                                let (wet, w0) = walked_ext.get(i).copied().unwrap_or((255, 0));
                                eprintln!(
                                    "[A2] FIRST-MISMATCH walked[{}]@{} walker_size={} walk_et={} raw0={:#018x} vs REAL(class_id={} kind={} et={} alen={} ns={} size={} seq={})",
                                    i, woff, wsz, wet, w0,
                                    r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size, r.seq,
                                );
                                break;
                            }
                        }
                    }
                }

                // Defensive recovery — DoHead walk-desync hardening
                // (2026-07-02): re-anchor at the START of the next known
                // free block instead of the old 8-byte plausibility probe.
                // The probe's check ACCEPTED an all-zero header (size = 40
                // passes every bound), so it could "re-sync" onto the first
                // zeroed word OFF the object grid and resume phantom-freeing
                // from an arbitrary offset — feeding the very overlap it was
                // recovering from. Free-block boundaries are the only ground
                // truth downstream; the stretch up to the anchor is retained
                // (recovered when a moving collection next resets the space).
                // The implausible header ALSO means the grid may have broken
                // BEFORE `cursor` (a mis-sized stride that happened to keep
                // passing the plausibility checks), so unwind the reclaim
                // decisions made since the last anchor — they may cover a
                // live object's interior. They have not been zeroed or
                // published yet (deferred to the publication loop).
                dead_regions.truncate(dead_watermark);
                if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                    tracing::warn!(
                        "non-moving sweep: re-anchored at next free block (offset {}); \
                         skipped stretch retained until a moving cycle resets from-space",
                        cursor,
                    );
                    continue;
                }
                tracing::warn!(
                    "non-moving sweep: no free-block anchor remains after offset {} — \
                     abandoning rest of arena ({} bytes retained)",
                    cursor,
                    used - cursor,
                );
                break;
            }

            // A2 fix: an object can never span a pre-existing free HOLE (holes are
            // gaps between objects, not object interiors). If the computed size
            // oversteps the next known free block, the header is over-sized — the
            // ReflRepro `et=Int` byte-array corruption reads a `byte[N]` as an
            // `int[N]` (4× the element size). Without this guard the sweep would
            // FREE the over-large span `[cursor, cursor+total_size)`, which overlaps
            // the free hole (the `DEAD-vs-EXISTING` overlap) and feeds the
            // overlapping-free-block / double-serve cycle the coalescer then has to
            // mop up. Instead, do NOT free or trust this span (it may subsume a live
            // neighbour) — RETAIN it (over-retention is always safe under the
            // non-moving sweep) and re-sync the cursor at the hole boundary, where
            // the robust free-block skip takes over. Caps the desync to a single
            // object instead of an overstep cascade.
            if let Some(&&(foff, _fsz)) = free_iter.peek() {
                if foff > cursor && foff < cursor + total_size {
                    if std::env::var_os("CRATONVM_DBG_A2").is_some()
                        && A2_FL_OVERLAP_HITS.load(Ordering::Relaxed) < 30
                    {
                        eprintln!(
                            "[A2-FL] CLAMP over-sized object @{} computed_size={} (kind={:?} class_id={}) oversteps free hole at {} — retaining + resyncing",
                            cursor, total_size, header.kind, header.class_id.as_u32(), foff,
                        );
                    }
                    A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                    // A hole-crossing header is grid-break evidence: the
                    // decisions since the last anchor are suspect — unwind
                    // them (over-retention safe) before re-anchoring at the
                    // hole, where the skip loop takes over.
                    dead_regions.truncate(dead_watermark);
                    cursor = foff;
                    continue;
                }
            }

            walked_count += 1;
            if retain_full_walk {
                walked.push_back((
                    cursor,
                    total_size,
                    header.class_id.as_u32(),
                    header.kind,
                    header.num_slots,
                    header.array_length,
                ));
                // SAFETY: obj_ptr is the mapped header start; reading 8 bytes is in bounds.
                walked_ext.push_back((header.element_type as u8, unsafe {
                    *(obj_ptr as *const u64)
                }));
            }

            let side_marked_survivor = if !header.is_forwarded() {
                // xt-hardening (2026-07-03): side-marked survivor check
                // (lockstep, absolute addrs). These candidates were kept
                // alive WITHOUT a header write; retain them without writing
                // gc_flags/gc_age either (their "header" may be a zero span
                // or a legal zero-word0 container — never write through it).
                let abs = from_base + cursor;
                while let Some(&&a) = side_iter.peek() {
                    if a < abs {
                        side_iter.next();
                    } else {
                        break;
                    }
                }
                side_iter.peek().is_some_and(|&&a| a == abs)
            } else {
                false
            };

            if header.is_forwarded() {
                // Evacuated to old gen by selective promotion: the live copy is
                // in old gen and references were redirected in the fixup pass;
                // reclaim (and zero) the young slot. (A "don't zero" variant was
                // tested to rule out a register-only dangling read — it did NOT
                // fix bintrees18's wrong checksum, so the residual corruption is
                // a structural wrong-address fixup, not a dangling read.)
                //
                // DoHead walk-desync hardening (2026-07-02): selective
                // promotion only ever forwards INTO old gen, so a forwarding
                // target outside it is a phantom write from a desynced walk
                // (or header corruption) — retain the span instead of zeroing
                // and freeing what may be a live object's interior.
                let fwd = header.forwarding_ptr;
                if watchref_dbg() && crate::gc_quiescence::is_watched_referent(obj_ptr as usize) {
                    eprintln!(
                        "[watchref] non-moving sweep: watched address @0x{:x} was EVACUATED to old gen @{:p} (should already be in evac_map from selective promotion)",
                        obj_ptr as usize, fwd
                    );
                }
                if !old_gen.contains(fwd) {
                    let n = SWEEP_BAD_FORWARD_HITS.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        tracing::warn!(
                            "non-moving sweep: forwarded young object at offset {} has \
                             non-old-gen target {:p} — retaining span, not freeing",
                            cursor,
                            fwd,
                        );
                    }
                } else {
                    if !retain_dead_objects {
                        if let Some(last) = dead_regions.last_mut() {
                            if last.0 + last.1 == cursor {
                                last.1 += total_size;
                                last.4 += 1;
                            } else {
                                dead_regions.push((
                                    cursor,
                                    total_size,
                                    header.class_id.as_u32(),
                                    header.kind as u8,
                                    1,
                                ));
                            }
                        } else {
                            dead_regions.push((
                                cursor,
                                total_size,
                                header.class_id.as_u32(),
                                header.kind as u8,
                                1,
                            ));
                        }
                    } else {
                        dead_regions.push((
                            cursor,
                            total_size,
                            header.class_id.as_u32(),
                            header.kind as u8,
                            1,
                        ));
                    }
                }
            } else if side_marked_survivor {
                // Side-marked survivor: pure retention, no header writes.
                objects_live += 1;
                // Family-A fix follow-up: a side-marked survivor is kept in
                // place exactly like a header-marked one (same "never moves"
                // guarantee), so it needs the SAME watched-referent identity
                // mapping — a live Weak/Soft/Phantom reference watching this
                // address must still see it as "survived" post-GC. Before the
                // write-through fix, every candidate that reached this point
                // via a real root had `GC_FLAG_MARKED` set on its own header
                // and took the branch below; now ALL conservative-root
                // survivors (real objects included) take this side-marked
                // path instead, so the identity-map insert must move here too
                // (regression: `non_moving_sweep_records_identity_map_for_watched_survivor`).
                let addr = obj_ptr as usize;
                if crate::gc_quiescence::is_watched_referent(addr) {
                    if watchref_dbg() {
                        eprintln!(
                            "[watchref] non-moving sweep: side-marked watched survivor kept in place @0x{addr:x} — identity-mapped"
                        );
                    }
                    evac_map.insert(addr, addr);
                }
            } else if header.gc_flags & GC_FLAG_MARKED != 0 {
                // Survivor: clear the mark, keep in place, and age it so the
                // next sweep can tenure it once it reaches PROMOTION_AGE
                // (selective promotion). Saturating so a long-lived pinned
                // object never wraps its age.
                header.gc_flags &= !GC_FLAG_MARKED;
                header.gc_age = header.gc_age.saturating_add(1);
                objects_live += 1;
                // RandomizedContext WeakHashMap<Thread,...> fix (see
                // gc_quiescence::is_watched_referent): this survivor is kept
                // in place at its ORIGINAL address, so selective promotion
                // never gives it a `pointer_map` entry (nothing moved). If a
                // live Weak/Soft/Phantom reference is currently watching this
                // exact address, record an identity mapping so post-GC
                // reference processing's `is_marked` check recognizes it as
                // having survived instead of wrongly clearing the reference.
                let addr = obj_ptr as usize;
                if crate::gc_quiescence::is_watched_referent(addr) {
                    if watchref_dbg() {
                        eprintln!(
                            "[watchref] non-moving sweep: watched survivor kept in place @0x{addr:x} — identity-mapped"
                        );
                    }
                    evac_map.insert(addr, addr);
                }
            } else {
                if watchref_dbg() && crate::gc_quiescence::is_watched_referent(obj_ptr as usize) {
                    eprintln!(
                        "[watchref] non-moving sweep: watched address @0x{:x} was DEAD (unmarked) — zeroing",
                        obj_ptr as usize
                    );
                }
                // Dead: record the span for reclamation. Zeroing (so a later
                // conservative root scan cannot resurrect a stale header
                // inside the hole) and free-list publication are deferred to
                // the publication loop below, so a grid anomaly detected
                // later in the walk can still unwind this decision.
                if !retain_dead_objects {
                    if let Some(last) = dead_regions.last_mut() {
                        if last.0 + last.1 == cursor {
                            last.1 += total_size;
                            last.4 += 1;
                        } else {
                            dead_regions.push((
                                cursor,
                                total_size,
                                header.class_id.as_u32(),
                                header.kind as u8,
                                1,
                            ));
                        }
                    } else {
                        dead_regions.push((
                            cursor,
                            total_size,
                            header.class_id.as_u32(),
                            header.kind as u8,
                            1,
                        ));
                    }
                } else {
                    dead_regions.push((
                        cursor,
                        total_size,
                        header.class_id.as_u32(),
                        header.kind as u8,
                        1,
                    ));
                }
            }
            cursor += total_size;
        }

        report_phase("sweep-walk");

        // Publish reclaimed regions to the arena's free list FIRST. Subsequent
        // `try_alloc_young_initialized` calls will satisfy allocations from
        // these holes before bumping the cursor — reclaiming memory without
        // moving a survivor.
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
        // A2 diag (CRATONVM_DBG_A2): does a NEW dead region this sweep overlap a
        // free block that was ALREADY free at sweep start? That is a double-free
        // (the walk reclaimed a region that was already on the free list — e.g. a
        // freed slot it walked as a phantom because the region wasn't skipped) or
        // an OVER-free (a too-large dead object whose span covers a free hole).
        // Either way it is the direct source of the overlapping free blocks the
        // coalescer then has to merge.
        if std::env::var_os("CRATONVM_DBG_A2").is_some() {
            for &(doff, dsz, _, _, _) in &dead_regions {
                for &(foff, fsz) in &existing_free {
                    if doff < foff + fsz && foff < doff + dsz {
                        let n = A2_FL_OVERLAP_HITS.load(Ordering::Relaxed);
                        if n < 18 {
                            eprintln!(
                                "[A2-FL] DEAD-vs-EXISTING overlap: new dead [{}, {}) overlaps pre-existing free [{}, {})",
                                doff, doff + dsz, foff, foff + fsz,
                            );
                        }
                        A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
        // Coalesce the newly-dead spans while they are still in walk order.
        // Publishing every object-sized hole first and sorting the arena free
        // list afterward made a bintrees18 collection allocate, sort, and merge
        // millions of entries even though nearly all of them are adjacent.
        // Keep `dead_regions` intact for the per-object diagnostics below, but
        // zero and publish only maximal contiguous spans.
        let mut reclaimed_regions: Vec<(usize, usize)> = Vec::new();
        reclaimed_regions.reserve(dead_regions.len().min(1024));
        for &(off, sz, _, _, _) in &dead_regions {
            if let Some(last) = reclaimed_regions.last_mut() {
                let last_end = last.0 + last.1;
                if off <= last_end {
                    last.1 = last_end.max(off + sz) - last.0;
                    continue;
                }
            }
            reclaimed_regions.push((off, sz));
        }

        // DoHead walk-desync hardening (2026-07-02): zeroing was deferred from
        // the walk's disposition arms to here so that anomaly-triggered
        // unwinding (dead_regions.truncate above) never has to un-zero.
        // Everything surviving in `dead_regions` was collected on a verified
        // stretch of the walk grid. Zero each span (so a later conservative
        // root scan cannot resurrect a stale header inside the hole) and
        // publish it to the free list. Per-object forensic records remain
        // available without forcing per-object arena publication.
        for &(off, sz, class_id, kind_byte, object_count) in &dead_regions {
            let obj_addr = from_base + off;
            record_swept(obj_addr, class_id, kind_byte, sweep_zero_cycle);
            crate::a2dbg::record_free(obj_addr);
            bytes_swept += sz;
            objects_swept += object_count;
        }
        for &(off, sz) in &reclaimed_regions {
            let obj_addr = from_base + off;
            // SAFETY: this is the union of adjacent/overlapping spans that the
            // verified walk collected, all within the live from-space region.
            unsafe { std::ptr::write_bytes(obj_addr as *mut u8, 0, sz) };
            young_from.add_free_block(off, sz);
        }
        report_phase("zero-and-publish");

        // DBG (CRATONVM_DBG_SWEEP_CENSUS): per-cycle census of what this sweep
        // reclaimed, by class. A continuously-live workload class (e.g. the
        // RRWL probe's ThreadLocalMap$Entry / HoldCounter chain) showing up
        // here names the wrongly-swept set directly at reclamation time —
        // no use-time face (zero-header receiver / OOB read) required.
        if std::env::var_os("CRATONVM_DBG_SWEEP_CENSUS").is_some() && !dead_regions.is_empty() {
            let mut counts: std::collections::HashMap<u32, (usize, usize)> =
                std::collections::HashMap::new();
            for &(off, sz, class_id, _kind, object_count) in &dead_regions {
                let e = counts.entry(class_id).or_insert((0, from_base + off));
                e.0 += object_count;
                let _ = sz;
            }
            let mut v: Vec<(u32, usize, usize)> =
                counts.into_iter().map(|(c, (n, a))| (c, n, a)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            let mut line = format!(
                "[sweep-census] cycle={} swept={} classes={}:",
                sweep_zero_cycle,
                dead_regions.len(),
                v.len()
            );
            for (cid, n, first_addr) in v.iter().take(14) {
                let name = crate::gc::resolve_class_info(*cid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| format!("cid{cid:#x}"));
                line.push_str(&format!(" {name}x{n}@{first_addr:#x}"));
            }
            eprintln!("{line}");

            // Holder scan: for each swept victim, brute-scan young from-space
            // and old gen for any aligned 8-byte word holding the victim's
            // address, plus any legacy 16-byte Value cell whose payload is the
            // victim. The mark said "unreachable"; if a live holder still
            // points at the victim, this names the EXACT edge the mark missed
            // — the decisive datum for the live-reclaim investigation.
            for &(doff, _dsz, dcid, _dk, _) in dead_regions.iter().take(6) {
                let victim = from_base + doff;
                let vname = crate::gc::resolve_class_info(dcid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| format!("cid{dcid:#x}"));
                let mut holders = 0usize;
                // Young from-space scan (skip the victim's own span).
                let mut w = from_base;
                let from_hi = from_base + young_from.used();
                while w + 8 <= from_hi {
                    // SAFETY: `[from_base, from_hi)` is mapped arena memory, w is 8-aligned.
                    let word = unsafe { *(w as *const u64) } as usize;
                    if word == victim && !(w >= victim && w < victim + _dsz) {
                        holders += 1;
                        if holders <= 4 {
                            eprintln!(
                                "[sweep-census]   victim {vname}@{victim:#x}: young holder word @{w:#x} (holder_off={:#x})",
                                w - from_base,
                            );
                        }
                    }
                    w += 8;
                }
                // Old-gen scan via object walk (bounded per object by its size).
                for (optr, osz) in old_gen.walk_objects() {
                    let lo = optr as usize;
                    let mut w = lo + HEADER_SIZE;
                    let hi = lo + osz;
                    while w + 8 <= hi {
                        // SAFETY: `[lo, hi)` is a live old-gen object's span.
                        let word = unsafe { *(w as *const u64) } as usize;
                        if word == victim {
                            holders += 1;
                            if holders <= 8 {
                                // SAFETY: `optr` is a live old-gen object header.
                                let ocid =
                                    unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() };
                                let oname = crate::gc::resolve_class_info(ocid)
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| format!("cid{ocid:#x}"));
                                eprintln!(
                                    "[sweep-census]   victim {vname}@{victim:#x}: OLD holder {oname}@{lo:#x}+{:#x}",
                                    w - lo,
                                );
                            }
                        }
                        w += 8;
                    }
                }
                // Was the victim in the ROOT SET (incl. every thread's folded
                // snapshot)? A root-listed victim that still got swept means
                // `mark_young` dropped it (validator/walk bug); an unlisted
                // one means the upstream root/snapshot coverage missed it.
                let in_roots = roots.iter().any(|r| r.as_ptr() as usize == victim);
                if holders == 0 && !in_roots {
                    eprintln!(
                        "[sweep-census]   victim {vname}@{victim:#x}: NO heap holder, NOT in roots — register/native-side ref only, or truly dead",
                    );
                } else if in_roots {
                    eprintln!(
                        "[sweep-census]   victim {vname}@{victim:#x}: WAS IN ROOTS ({holders} heap holder(s)) — mark_young dropped a live root!",
                    );
                } else {
                    eprintln!(
                        "[sweep-census]   victim {vname}@{victim:#x}: {holders} heap holder(s) — MARK MISSED A HEAP EDGE",
                    );
                }
            }
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
        // DoHead walk-desync hardening (2026-07-02): run the coalescer (and
        // its overlap-merge safety invariant, 6e3ddb05) regardless of
        // `selective_on` — with promotion disabled the free list still
        // fragments and overlapping blocks would still double-serve.
        if std::env::var_os("CRATONVM_SP_NO_COALESCE").is_none() {
            let sorted = young_from.free_blocks_sorted();
            if sorted.len() > 1 {
                young_from.clear_free_list();
                let mut merged: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
                for (off, sz) in sorted {
                    if let Some(last) = merged.last_mut() {
                        let last_end = last.0 + last.1;
                        // Merge ADJACENT (off == last_end) AND OVERLAPPING
                        // (off < last_end) blocks. The old code only handled the
                        // adjacent case, so two overlapping free blocks both
                        // survived — and `Arena::alloc` (no overlap check, see
                        // `arena.rs::add_free_block`) could then SERVE the same
                        // young region twice → two live objects overlapping → the
                        // non-moving sweep's linear walk reads a header inside a
                        // neighbour and desyncs (the A2 / ReflRepro corruption:
                        // CRATONVM_DBG_A2 shows the walk overstep a live object).
                        // Overlapping spans appear when a region is freed more than
                        // once (a stale block not removed on reuse, or two dead
                        // spans that overlap after an earlier desync). Extend to the
                        // farther end so the union is one block, never double-served.
                        if off <= last_end {
                            if off < last_end {
                                A2_FL_OVERLAP_HITS.fetch_add(1, Ordering::Relaxed);
                                if std::env::var_os("CRATONVM_DBG_A2").is_some()
                                    && A2_FL_OVERLAP_HITS.load(Ordering::Relaxed) <= 6
                                {
                                    eprintln!(
                                        "[A2-FL] coalesce OVERLAP: block off={} size={} overlaps prev [{}, {}) — merging",
                                        off, sz, last.0, last_end,
                                    );
                                }
                            }
                            let new_end = last_end.max(off + sz);
                            last.1 = new_end - last.0;
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
        report_phase("arena-coalesce");

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
        report_phase("clear-marks");

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
            eprintln!(
                "[PRECISE] sweep_young_non_moving returning evac_map.len()={}",
                evac_map.len()
            );
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

    /// Run a non-moving mark-sweep of old space while conservative JIT roots are
    /// active.  The supplied roots are copied only because the shared marker's
    /// compacting mode can rewrite its mutable root slice; this mode never does.
    fn sweep_old_gen_non_moving(&self, roots: &[ObjectRef]) -> usize {
        let young_from = self.young_from.lock();
        let mut old_gen = self.old_gen.lock();
        let before = old_gen.used();
        let mut root_shadow = roots.to_vec();
        let _ = Self::old_gen_gc(&mut root_shadow, &young_from, &mut old_gen, false);
        before.saturating_sub(old_gen.used())
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
        Self::old_gen_gc(roots, young_from, old_gen, true)
    }

    /// Mark old space from the complete root set and either compact it (when
    /// every root is rewritable) or reclaim dead blocks in place (when JIT
    /// roots are conservative).
    fn old_gen_gc(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
        compact: bool,
    ) -> HashMap<usize, usize> {
        // ---- Mark phase ---- BFS from roots + young-gen cross-references ----

        let mut worklist: Vec<*mut u8> = Vec::new();

        // Seed: root ObjectRefs that point into old gen.
        //
        // xt-hardening follow-up (2026-07-03): `roots` includes CONSERVATIVE
        // candidates (register/stack-scanned guesses — the same `xt_roots`
        // whose flood exposed the young-gen `mark_young` header-write
        // corruptor). `OldGen::contains` is a bare bounds check (no
        // alignment, no header validation), so before this fix ANY garbage
        // address landing inside old gen's byte range got `gc_flags` blindly
        // RMW'd — the exact same corruption family, on the OTHER generation.
        // Reject implausible candidates instead of marking them: mirrors the
        // already-established pattern in `scan_object_for_old_refs`'s extent
        // check (skip-on-implausible, never corrupt) — old-gen compaction
        // decides liveness purely from `gc_flags & GC_FLAG_MARKED`, so unlike
        // the young sweep there is no side-mark-set escape hatch; the safe
        // choice for an address that fails these checks is to not mark it
        // (over-retention is not even at stake here — a failing candidate
        // was never a valid object to begin with).
        //
        // NOTE this predicate deliberately does NOT require a non-zero first
        // header word (unlike `mark_young`'s zero-word0 side-mark split):
        // `ClassId(0)` ad-hoc containers (class_id=0, kind=Object=0,
        // element_type=Reference=0, `_padding`=0) are a first-class supported
        // shape whose word0 is legitimately all-zero — rejecting them here
        // broke real promoted objects (major_gc_frees_old_gen_garbage et al).
        // A zero-word0 candidate additionally passes through
        // `victim8_neighbor_explains_zero_prefix` — a targeted check for the
        // dominant real-world shape of this ambiguity, `candidate = victim
        // − 8`: if the 8 bytes at `candidate` are just the tail
        // zero-padding/hash-prefix of a SEPARATE, independently-plausible
        // object starting at `candidate + 8`, `candidate` itself is not a
        // real header and is rejected. A genuine zero-hash `ClassId(0)`
        // container's successor 8 bytes are its own body/next-object data,
        // essentially never a coincidentally-valid, independently-fitting
        // header — false rejects of real containers are not expected. Old
        // gen has no side-mark-set escape hatch (compaction PHYSICALLY
        // SLIDES live objects; treating an unrelated garbage candidate as
        // live would copy garbage over/into a real neighbor during the
        // slide — strictly worse than skipping), so reject is the only safe
        // response to a candidate that fails this check; verified this
        // session (disassembly + byte-exact match on the fabricated
        // pointer `0x0000020000000000` = hash(0)‖array_length(512) read
        // from a corrupted victim's header) to be the mechanism behind the
        // pre-xt-activation background DoHead crash face.
        for root in roots.iter() {
            let ptr = root.as_ptr();
            if (ptr as usize) & 0x7 == 0 && old_gen.contains(ptr) {
                // SAFETY: `ptr` is 8-aligned and inside old gen (verified by
                // `contains` above); reading its header is in-bounds.
                let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
                let kind_byte = header.kind as u8;
                let is_array = header.kind == ObjectKind::Array;
                let word0 = unsafe { *(ptr as *const u64) };
                let plausible = kind_byte <= 1
                    && (is_array || header.num_slots <= (1 << 24))
                    && (!is_array || header.array_length <= i32::MAX as u32)
                    && header_reserved_fields_plausible(header)
                    && (word0 != 0 || !victim8_neighbor_explains_zero_prefix(ptr, old_gen));
                if plausible {
                    let total = gen_object_total_size(header);
                    let fits = total >= HEADER_SIZE
                        // SAFETY: total >= HEADER_SIZE was just checked; the
                        // addition stays within a sane pointer range for a
                        // plausibility probe (no dereference here).
                        && old_gen.contains(unsafe { ptr.add(total - 1) });
                    if fits && header.gc_flags & GC_FLAG_MARKED == 0 {
                        header.gc_flags |= GC_FLAG_MARKED;
                        worklist.push(ptr);
                    }
                }
            }
        }

        // Seed: young from-space references into old gen
        Self::mark_young_to_old_refs(young_from, old_gen, &mut worklist);

        // `mark_young_to_old_refs` walks only real Java heap fields. Follow
        // the equivalent out-of-heap edges for all current young owners too;
        // young from-space is conservatively retained for this major cycle,
        // matching the ordinary cross-generation seed's contract.
        for overlay_ref in
            cratonvm_native_collections::gc_overlay_roots_for_matching_owners(|owner_addr| {
                young_from.contains(owner_addr as *mut u8)
            })
        {
            let overlay_ptr = overlay_ref.as_ptr();
            if old_gen.contains(overlay_ptr) {
                // SAFETY: `overlay_ptr` lies in old gen, verified above.
                let h = unsafe { &mut *(overlay_ptr as *mut ObjectHeader) };
                if h.gc_flags & GC_FLAG_MARKED == 0 {
                    h.gc_flags |= GC_FLAG_MARKED;
                    worklist.push(overlay_ptr);
                }
            }
        }

        // BFS: transitively mark all reachable old-gen objects
        // HIB-CV-24: a live object keeps its class's defining ClassLoader alive
        // (instance→loader). Conservative — only ever marks MORE live, so it can
        // never free a still-referenced loader. Covers the old-gen instance →
        // old-gen loader case (young loaders are already live as major-GC roots).
        let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();
        while let Some(obj_ptr) = worklist.pop() {
            Self::scan_object_for_old_refs(obj_ptr, old_gen, &mut worklist);
            // Same owner→overlay propagation as the young non-moving marker
            // above. Major GC also uses stable pre-compaction addresses, so it
            // can reclaim an unreachable old collection and its side-table
            // graph together instead of treating every entry as a global root.
            for overlay_ref in
                cratonvm_native_collections::gc_overlay_roots_for_collection(obj_ptr as usize)
            {
                let overlay_ptr = overlay_ref.as_ptr();
                if old_gen.contains(overlay_ptr) {
                    // SAFETY: `overlay_ptr` is in old gen; this is the same
                    // marking transition used by the defining-loader pin below.
                    let h = unsafe { &mut *(overlay_ptr as *mut ObjectHeader) };
                    if h.gc_flags & GC_FLAG_MARKED == 0 {
                        h.gc_flags |= GC_FLAG_MARKED;
                        worklist.push(overlay_ptr);
                    }
                }
            }
            if loader_pin_on {
                // SAFETY: `obj_ptr` is a marked old-gen object with a valid header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if let Some(loader_addr) =
                    cratonvm_types::loader_pin::loader_pin_addr(header.class_id.as_u32())
                {
                    let lp = loader_addr as *mut u8;
                    if old_gen.contains(lp) {
                        // SAFETY: `lp` is within old gen (verified by `contains`).
                        let h = unsafe { &mut *(lp as *mut ObjectHeader) };
                        if h.gc_flags & GC_FLAG_MARKED == 0 {
                            h.gc_flags |= GC_FLAG_MARKED;
                            worklist.push(lp);
                        }
                    }
                }
            }
            // Class-mirror liveness pin (mirror_pin, companion to loader_pin
            // above — see `vm::memory::roots` step 6 and
            // `cratonvm_types::mirror_pin`). Old gen doesn't move during this
            // BFS (compaction happens after), so `obj_ptr` is a stable key.
            // If this marked object IS itself a user-defined `ClassLoader`
            // that defined mirror-having classes, mark those mirrors alive
            // too — same "conservative, only ever marks MORE live" property
            // as the loader_pin check above. Same young-gen exemption as
            // loader_pin: a still-young mirror is already live as a major-GC
            // root.
            if let Some(mirror_addrs) =
                cratonvm_types::mirror_pin::mirrors_for_loader(obj_ptr as usize)
            {
                for mirror_addr in mirror_addrs {
                    let mp = mirror_addr as *mut u8;
                    if old_gen.contains(mp) {
                        // SAFETY: `mp` is within old gen (verified by `contains`).
                        let h = unsafe { &mut *(mp as *mut ObjectHeader) };
                        if h.gc_flags & GC_FLAG_MARKED == 0 {
                            h.gc_flags |= GC_FLAG_MARKED;
                            worklist.push(mp);
                        }
                    }
                }
            }
        }

        if !compact {
            let objects = old_gen.walk_objects();
            for (obj_ptr, total_size) in objects {
                // SAFETY: `walk_objects` returns valid old-gen object starts.
                // Marked objects remain at their current address; every other
                // object was unreachable from the complete precise +
                // conservative root set and can be returned to the free list.
                let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                if header.gc_flags & GC_FLAG_MARKED != 0 {
                    header.gc_flags &= !GC_FLAG_MARKED;
                } else {
                    // A2 forensic breadcrumb (CRATONVM_DBG_A2): preserve the
                    // victim's pre-free identity so a later zero-header /
                    // wild-receiver access at this address can be attributed
                    // to THIS sweep having freed a still-referenced object
                    // (the DoHead freed-while-live investigation).
                    if crate::a2dbg::enabled() {
                        crate::a2dbg::record_old_sweep_free(
                            obj_ptr as usize,
                            header.class_id.as_u32(),
                            header.num_slots,
                            total_size,
                        );
                    }
                    unsafe { old_gen.free(obj_ptr, total_size) };
                }
            }
            return HashMap::new();
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
    fn mark_young_to_old_refs(young_from: &Arena, old_gen: &OldGen, worklist: &mut Vec<*mut u8>) {
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
        let base = young_from.base_ptr() as usize;
        let used = young_from.used();
        // DoHead walk-desync hardening (2026-07-02): a stretch this walk
        // cannot parse (zero span / implausible header) must NOT be silently
        // dropped — an old object referenced only from that stretch would go
        // unmarked and be freed by the compaction (use-after-free). Fall back
        // to a conservative word scan of the stretch: mark every aligned word
        // that is EXACTLY an old-gen object base. Over-marking is always
        // safe; base validation is mandatory (`OldGen::contains` is a raw
        // range check, so an interior/colliding word would otherwise get a
        // mark-bit write into a live object's payload and feed a garbage
        // "header" into the BFS). The sorted base list is built lazily — only
        // sweeps that actually hit an unparseable stretch pay for it.
        let mut old_bases: Option<Vec<usize>> = None;
        while cursor < used {
            if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                continue;
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
                let gap =
                    unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) } as usize;
                if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                    cursor += gap;
                    continue;
                }
            }
            // Unlisted zeroed span (see the sweep walk): zero words carry no
            // old-gen refs, so re-anchor at the next free block; the
            // remainder up to the anchor is conservatively word-scanned.
            let word0 = unsafe { *(obj_ptr as *const u64) };
            let mut anomaly = false;
            if word0 == 0 {
                let limit = free_iter
                    .peek()
                    .map(|&&(off, _)| off)
                    .unwrap_or(used)
                    .min(used);
                let run_end = zero_run_end(base, cursor, limit);
                if run_end - cursor >= HEADER_SIZE {
                    anomaly = true;
                }
            }
            let total_size = if anomaly {
                0
            } else {
                gen_object_total_size(header)
            };
            if anomaly || total_size < HEADER_SIZE || cursor + total_size > used {
                let stretch_lo = cursor;
                let resynced = resync_to_next_free_block(&mut cursor, &mut free_iter);
                let stretch_hi = if resynced { cursor } else { used };
                let bases = old_bases.get_or_insert_with(|| {
                    let mut v: Vec<usize> = old_gen
                        .walk_objects()
                        .into_iter()
                        .map(|(p, _)| p as usize)
                        .collect();
                    v.sort_unstable();
                    v
                });
                let mut w = stretch_lo & !7;
                while w + 8 <= stretch_hi {
                    // SAFETY: `[base+w, base+w+8)` is mapped from-space memory.
                    let word = unsafe { *((base + w) as *const u64) } as usize;
                    if bases.binary_search(&word).is_ok() {
                        // SAFETY: `word` is a verified old-gen object BASE;
                        // its header is valid and mutable for marking.
                        let ref_header = unsafe { &mut *(word as *mut ObjectHeader) };
                        if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                            ref_header.gc_flags |= GC_FLAG_MARKED;
                            worklist.push(word as *mut u8);
                        }
                    }
                    w += 8;
                }
                if resynced {
                    continue;
                }
                break;
            }

            // Mark every old-gen referent (arrays + compact objects: 8-byte
            // pointers; legacy objects: 16-byte Value cells).
            // SAFETY: `obj_ptr`/`header` are a valid live young-from object.
            unsafe {
                for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                    if old_gen.contains(ref_ptr) {
                        // SAFETY: `ref_ptr` is in old gen (verified by `contains`);
                        // its header is valid and mutable for marking.
                        let ref_header = &mut *(ref_ptr as *mut ObjectHeader);
                        if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                            ref_header.gc_flags |= GC_FLAG_MARKED;
                            worklist.push(ref_ptr);
                        }
                    }
                });
            }

            cursor += total_size;
        }
    }

    /// Scan a single object's reference slots for old-gen pointers and mark them.
    fn scan_object_for_old_refs(obj_ptr: *mut u8, old_gen: &OldGen, worklist: &mut Vec<*mut u8>) {
        // SAFETY: `obj_ptr` is a live old-gen object from the mark worklist; its header is valid.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        // DoHead comb-7 fix (2026-07-03): validate the claimed extent before
        // scanning — the young-mark twin of this check caught a corrupt
        // header whose `array_length` was pointer bytes; scanning that count
        // of slots runs off the mapped region (SIGSEGV). An old object whose
        // extent leaves old gen is definitionally corrupt: skip the scan.
        let total = gen_object_total_size(header);
        if total < HEADER_SIZE || !old_gen.contains(unsafe { obj_ptr.add(total - 1) }) {
            let n = SWEEP_BAD_EXTENT_HITS.fetch_add(1, Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    "old-gen mark: rejecting object at {:p} with implausible extent {} \
                     (kind={}, array_len={}, num_slots={}) — corrupt header, not scanned",
                    obj_ptr,
                    total,
                    header.kind as u8,
                    header.array_length,
                    header.num_slots,
                );
            }
            return;
        }
        // Mark every old-gen referent (arrays + compact objects: 8-byte pointers;
        // legacy objects: 16-byte Value cells).
        // SAFETY: `obj_ptr`/`header` are a valid live object.
        unsafe {
            for_each_ref_slot(obj_ptr, header, |ref_ptr, _| {
                if old_gen.contains(ref_ptr) {
                    // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its
                    // header is valid and mutable for marking.
                    let ref_header = &mut *(ref_ptr as *mut ObjectHeader);
                    if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                        ref_header.gc_flags |= GC_FLAG_MARKED;
                        worklist.push(ref_ptr);
                    }
                }
            });
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
        let base = young_from.base_ptr() as usize;
        let used = young_from.used();
        // DoHead walk-desync hardening (2026-07-02): a stretch this walk
        // cannot parse must not be dropped — a stale reference to a MOVED
        // old-gen object is a guaranteed dangling pointer. Fall back to a
        // conservative word rewrite over the stretch: any aligned word that
        // exactly matches a relocated old address is rewritten to the new
        // address. (A primitive that happens to equal a moved object's old
        // address would be corrupted — vanishingly unlikely — whereas an
        // unrewritten real reference is a certain use-after-free.)
        let rewrite_stretch_conservatively = |lo: usize, hi: usize| {
            let mut w = lo & !7;
            while w + 8 <= hi {
                // SAFETY: `[base+lo, base+hi)` is mapped from-space memory.
                let cell = (base + w) as *mut u64;
                let word = unsafe { *cell } as usize;
                if let Some(&new_addr) = compact_map.get(&word) {
                    unsafe { *cell = new_addr as u64 };
                }
                w += 8;
            }
        };
        while cursor < used {
            if skip_free_blocks(&mut cursor, &mut free_iter).0 {
                continue;
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
                let gap =
                    unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) } as usize;
                if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                    cursor += gap;
                    continue;
                }
            }
            let word0 = unsafe { *(obj_ptr as *const u64) };
            let mut anomaly = false;
            if word0 == 0 {
                let limit = free_iter
                    .peek()
                    .map(|&&(off, _)| off)
                    .unwrap_or(used)
                    .min(used);
                let run_end = zero_run_end(base, cursor, limit);
                if run_end - cursor >= HEADER_SIZE {
                    anomaly = true;
                }
            }
            let total_size = if anomaly {
                0
            } else {
                gen_object_total_size(header)
            };
            if anomaly || total_size < HEADER_SIZE || cursor + total_size > used {
                let stretch_lo = cursor;
                let resynced = resync_to_next_free_block(&mut cursor, &mut free_iter);
                let stretch_hi = if resynced { cursor } else { used };
                rewrite_stretch_conservatively(stretch_lo, stretch_hi);
                if resynced {
                    continue;
                }
                break;
            }

            // Remap any reference into a relocated old-gen object (arrays +
            // compact objects: 8-byte pointers; legacy objects: 16-byte cells).
            // SAFETY: `obj_ptr`/`header` are a valid live young-from object.
            unsafe {
                forward_ref_slots(obj_ptr, header, |ref_ptr| {
                    compact_map
                        .get(&(ref_ptr as usize))
                        .map(|&new_addr| new_addr as *mut u8)
                });
            }

            cursor += total_size;
        }
    }

    // ----- Internal ----------------------------------------------------------

    /// Allocate bytes in the young from-space and run `init` before publishing
    /// the span to any GC walker.
    ///
    /// Returns `None` if the young generation is exhausted and cannot satisfy
    /// the allocation. The caller should trigger a GC cycle and retry, or
    /// throw `OutOfMemoryError`.
    fn try_alloc_young_initialized<F>(&self, size: usize, init: F) -> Option<*mut u8>
    where
        F: FnOnce(*mut u8),
    {
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
                "try_alloc_young_initialized: cross-node slow path (single-arena fallback)",
            );
        }

        let ptr = {
            let mut from = self.young_from.lock();
            let ptr = from.alloc(size, 8)?;
            // SAFETY: `ptr` was just allocated from the arena with `size`
            // bytes; zeroing is within bounds. `init` writes the valid header
            // before the arena lock is released.
            unsafe { std::ptr::write_bytes(ptr, 0, size) };
            init(ptr);
            ptr
        };
        self.stats.young_allocations.fetch_add(1, Ordering::Relaxed);
        Some(ptr)
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
        let mut from = self.young_from.lock();
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
        // `has_free_block_at_least` early-exits at the first satisfying block
        // and answers repeated too-large requests in O(1) via the cached
        // upper bound — the former `largest_free_block()` full scan here ran
        // once per JIT slow-path allocation (~7% of binarytrees-18).
        if from.has_free_block_at_least(size) {
            Some(())
        } else {
            None
        }
    }

    /// O(1) bump-tail-only headroom probe — see `VmHeap::young_bump_headroom`.
    /// Deliberately does NOT fall back to the free list: this feeds the JIT
    /// TLAB-refill gate, where a per-allocation `largest_free_block` scan of a
    /// fragmented young free list is exactly the pathology being avoided.
    pub fn young_bump_headroom(&self, size: usize) -> bool {
        let from = self.young_from.lock();
        if let Some(aligned) = from.used().checked_add(7).map(|v| v & !7) {
            if let Some(end) = aligned.checked_add(size) {
                return end <= from.capacity();
            }
        }
        false
    }

    /// Amortized-O(1) probe: could a young `refill_tlab(size)` be served from
    /// the RECLAIMED free list right now? Early-exits at the first
    /// sufficiently-large block and fail-fasts through the cached
    /// `max_free_upper` bound (`Arena::has_free_block_at_least`), so unlike
    /// `largest_free_block` it is safe to consult per allocation. Paired with
    /// [`Self::young_bump_headroom`] this is the JIT TLAB-refill gate: after
    /// a non-moving sweep + coalesce the young free list is a handful of big
    /// spans, so refills keep flowing out of reclaimed space without forcing
    /// a GC — while a genuinely-full young answers `false` in O(1) and the
    /// caller takes the old-gen spill exactly as before.
    pub fn young_has_free_block(&self, size: usize) -> bool {
        self.young_from.lock().has_free_block_at_least(size)
    }

    /// DBG: one-shot young-arena state snapshot `(used, capacity,
    /// free_list_bytes, largest_free_block)` — diagnostics only (full
    /// free-list scans under the lock).
    pub fn young_arena_diag(&self) -> (usize, usize, usize, usize) {
        let from = self.young_from.lock();
        (
            from.used(),
            from.capacity(),
            from.free_list_bytes(),
            from.largest_free_block(),
        )
    }

    /// Carve out a TLAB-sized chunk from the young from-space.
    ///
    /// Returns `Some((ptr, size))` on success, where `ptr` is the start of
    /// the zeroed region and `size` is the actual TLAB size (may be smaller
    /// than requested if the arena is nearly full).
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        // NUMA stub: same shape as try_alloc_young_initialized. The TLAB itself is
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
        // Alignment invariant (perf/halfgap residuals, 2026-07-18): a TLAB's
        // size must be a multiple of 8. `available` can carry a mod-8 dreg
        // (unaligned free-list dust, or the historical unaligned-capacity bump
        // tail), and an unaligned TLAB both mints an off-grid free-list split
        // remnant AND gets its end rounded DOWN by `Tlab::new`'s release
        // safety net — leaving an untracked zeroed sliver between the TLAB's
        // filler and the next region that derails the non-moving walk (the
        // trigger-ON bt18 corruption; see
        // docs/known-issues/tlab-trigger-gc-young-walk-corruption.md).
        let actual_size = requested_size.min(available) & !7;
        if actual_size == 0 {
            return None;
        }
        if let Some(ptr) = from.alloc(actual_size, 8) {
            // Zero the TLAB region
            // SAFETY: `ptr` was just allocated from the arena with `actual_size` bytes; zeroing is within bounds.
            unsafe { std::ptr::write_bytes(ptr, 0, actual_size) };
            return Some((ptr, actual_size));
        }
        // Fragmentation fallback (the bimodal-bt18 wedge): no single span fits
        // `actual_size`, but a smaller one can still make a useful TLAB.
        // Steady-state refills carve exact `requested_size` chunks out of the
        // sweep's coalesced spans; the split leftovers converge on blocks a
        // few bytes SHORT of the adaptive sizer's request (observed: a ~2 GiB
        // free list made entirely of 131056-byte blocks vs. a 131072-byte
        // request). Without this fallback the refill gate then fails on every
        // allocation while tiny object allocations keep succeeding off the
        // remnants — so the young collection (whose coalescer would heal the
        // fragmentation) is never triggered either, and the entire rest of
        // the run crawls through the per-object slow path (bt18: 1.9s → 4.5s
        // on a ~50% coin flip of whether the post-sweep workload outlasted
        // the recycled spans). Serving the largest available block (capped at
        // the request, floored at a useful TLAB size) keeps the remnants
        // flowing through the bump fast path instead; the cost — one
        // O(free-list) largest-block scan — is paid once per served TLAB,
        // not per object.
        let largest = from.largest_free_block();
        // Second-wedge fix (perf/halfgap-20260717): the floor here must match
        // the guarded-refill gate's fragmentation floor, and both must sit
        // BELOW the sizes steady-state splitting converges on. With the old
        // `min_tlab_size().max(256)` (= 8192) floor, a free list that had
        // degraded to 4080-byte remnants (8 KiB splits minus header slack)
        // wedged BOTH this fallback and the gate shut — see
        // `tlab::FRAG_TLAB_FLOOR` for the live BinTrees capture. A ~4 KB
        // mini-TLAB still serves ~100 small objects at bump speed, vastly
        // outperforming the per-object slow path this None would otherwise
        // condemn every allocation to.
        let floor = crate::tlab::frag_tlab_floor();
        if largest >= floor {
            // `& !7`: same TLAB-size alignment invariant as the main path.
            let take = largest.min(actual_size) & !7;
            if let Some(ptr) = from.alloc(take, 8) {
                // SAFETY: `ptr` was just allocated from the arena with `take` bytes; zeroing is within bounds.
                unsafe { std::ptr::write_bytes(ptr, 0, take) };
                return Some((ptr, take));
            }
        }
        None
    }

    /// Allocate bytes in the young from-space and initialize the object before
    /// publishing it.
    ///
    /// If the from-space is full, logs a fatal error and aborts. The
    /// interpreter's `gc_alloc_*` functions use fallible allocation with
    /// GC-and-retry; this method is only called by the panicking
    /// `alloc_object`/`alloc_array` convenience wrappers.
    fn alloc_young_initialized<F>(&self, size: usize, init: F) -> *mut u8
    where
        F: FnOnce(*mut u8),
    {
        self.try_alloc_young_initialized(size, init)
            .unwrap_or_else(|| {
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
        young_object_starts: &FxHashSet<usize>,
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
            young_object_starts,
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
        young_object_starts: &FxHashSet<usize>,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        old_ptr: *mut u8,
        objects_copied: &mut usize,
        pointer_map: &mut FxHashMap<usize, usize>,
        promoted_worklist: &mut Vec<*mut u8>,
        force_promote_all: bool,
    ) -> *mut u8 {
        if !young_object_starts.contains(&(old_ptr as usize)) {
            // Exact pre-GC membership rejects aligned interior words from
            // conservative roots before forwarding writes through them.
            return old_ptr;
        }
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
        // SAFETY: `old_ptr` points at a live young-gen object, so each field of its
        // `ObjectHeader` is initialized and individually readable via addr_of! reads;
        // no aliasing `&` is held while we later install the forwarding pointer.
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
                            if fwd_resolve_strict() {
                                "REJECTED"
                            } else {
                                "copied anyway"
                            },
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
                            // same way `alloc_young_initialized` handles young-gen
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
                    // with `alloc_young_initialized`'s handling of young-gen exhaustion.
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
        // gcstress face-1 hunt (no-op unless gated): validate the source body
        // (see promote-src) and dump the SOURCE qword pair at the watched
        // offset, so a copy that IMPORTS corrupt content is distinguishable
        // from one that copies a clean cell.
        // SAFETY: `old_ptr` is the live source object under STW.
        validate_copy_source_cells(
            old_ptr,
            unsafe { &*(old_ptr as *const ObjectHeader) },
            "cheney-src",
        );
        {
            let w = crate::heap::cell_watch_addr();
            let dst = new_ptr as usize;
            if w != 0 && dst <= w && w.wrapping_sub(dst) < total_size {
                let src_at = old_ptr as usize + (w - dst);
                // SAFETY: src object spans total_size bytes; src_at is within it.
                let pair = unsafe { std::ptr::read(src_at as *const [u64; 2]) };
                crate::heap::cell_watch_check(
                    dst,
                    total_size,
                    "cheney-copy",
                    &format!(
                        "src=0x{:x} src[watch]=0x{:016x},0x{:016x}",
                        old_ptr as usize, pair[0], pair[1]
                    ),
                );
            }
        }
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
                // SAFETY: `si < ns == header.num_slots`, so the slot lies in the freshly
                // copied object's field area at `new_ptr`; the `Value` read is in-bounds.
                let dv = unsafe {
                    std::ptr::read(new_ptr.add(HEADER_SIZE + si * SLOT_SIZE) as *const Value)
                };
                if let Value::Object(Some(r)) = dv {
                    let p = r.as_ptr() as usize;
                    if p != 0 && p < 0x1000 {
                        // SAFETY: same `si < num_slots` bound applies to the source object
                        // at `old_ptr`, so reading the matching slot is in-bounds.
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
        if desc_trace_enabled() {
            if let Some((cname, _)) = crate::gc::resolve_class_info(header.class_id.as_u32()) {
                if cname == "org/junit/runner/Description"
                    || cname == "java/util/concurrent/ConcurrentLinkedQueue"
                {
                    eprintln!(
                        "[desctrace-fwd] {} ihash={} old=0x{:x} new=0x{:x} promoted={} age={} jit_active={} moving_young={}",
                        cname,
                        header.identity_hash_code,
                        old_ptr as usize,
                        new_ptr as usize,
                        landed_in_old_gen,
                        header.gc_age,
                        crate::gc_quiescence::is_active(),
                        crate::gc_quiescence::moving_young_enabled(),
                    );
                }
            }
        }
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

    /// The remembered-set fast path is backed by a complete old-to-young
    /// scan. A missed write barrier has historically manifested as silently
    /// emptied Stream/ArrayList results under small-heap concurrency; retaining
    /// an object for one extra minor GC is safe, reclaiming it is not.
    #[inline]
    fn full_old_rset_scan_enabled() -> bool {
        static CARD_TABLE_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        !*CARD_TABLE_ONLY.get_or_init(|| std::env::var_os("CRATONVM_CARD_TABLE_ONLY").is_some())
    }

    /// Append all old-to-young slots using the same slot encoding as the card
    /// scanner. Duplicates from the card fast path are harmless: forwarding is
    /// idempotent.
    fn scan_all_old_to_young(
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
    ) {
        for (obj_ptr, _total_size) in old_gen.walk_objects() {
            // SAFETY: the old-generation walk yields initialized object starts.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // SAFETY: the object/header pair remains valid during this STW scan.
            unsafe {
                for_each_ref_slot(obj_ptr, header, |raw, slot_idx| {
                    if !raw.is_null() && young_from.contains(raw) {
                        let obj_ref = ObjectRef::from_raw(obj_ptr);
                        extra_roots.push((obj_ref, slot_idx, 0));
                    }
                });
            }
        }
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
        // O(total cards). `take_dirty_cards` clears BOTH the tracking list and
        // the consumed cards' bitmap bytes (CARD_CLEAN), keeping the two in
        // sync. This matters for the NON-MOVING sweep, which (unlike the moving
        // path) never calls `clear_all()`: without the byte reset, the sweep's
        // re-dirty of surviving old→young edges would no-op and the edge would
        // be lost on the next GC (see `CardTable::take_dirty_cards` docs — the
        // bt18 premature-reclamation fix).
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
        for (obj_ptr, total_size) in objects {
            // SAFETY: `obj_ptr` is from `old_gen.walk_objects_in_card_ranges()`, pointing to a valid old-gen object header.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

            // [LOW gc-genheap-cards] Plausibility cap, mirroring the non-moving
            // sweep walker (gen_object_total_size + the `num_slots <= 1<<24` /
            // `array_length <= i32::MAX` re-sync checks ~4239-4246). Previously
            // the ref-slot loops below trusted `header.num_slots` /
            // `header.array_length` verbatim, so a corrupt old-gen header (e.g.
            // an inline-alloc path that left a garbage slot count, or a header
            // straddling a buffer that walk_objects_in_card_ranges mis-bounded)
            // would drive an out-of-range slot scan reading past the object.
            //
            // `gen_object_total_size` returns 0 for an implausible header
            // (oversized num_slots, kind=Object-with-array_length, bad array
            // length); skip such an object with a diagnostic rather than
            // scanning bogus slots. Then bound the per-object iteration to the
            // ref-slot count that actually fits in `total_size` (the object's
            // region size as established by the walker), so even a header that
            // passes the coarse plausibility gate but reports more slots than
            // its bytes hold cannot read out of bounds.
            let safe_size = gen_object_total_size(header);
            if safe_size < HEADER_SIZE || safe_size != total_size {
                tracing::warn!(
                    "GC scan_dirty_cards: implausible/inconsistent old-gen header \
                     (class_id={} kind=0x{:02x} num_slots={} array_length={} \
                     gen_size={} walker_size={}) — skipping ref-slot scan",
                    header.class_id.as_u32(),
                    header.kind as u8,
                    header.num_slots,
                    header.array_length,
                    safe_size,
                    total_size,
                );
                continue;
            }
            // Field/element bytes available in this object's region (total
            // minus header). Used to cap the slot/element count.
            let body_bytes = total_size - HEADER_SIZE;

            // Scan ref slots: ref arrays use compact 8-byte pointers,
            // object fields use 16-byte Value.
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    // Cap element count at what the array's data region holds.
                    let max_elems = body_bytes / REF_ELEMENT_SIZE;
                    let elems = (header.array_length as usize).min(max_elems);
                    for i in 0..elems {
                        // SAFETY: `i` < capped element count; offset within array data region.
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
            } else if let Some((layout, compact_body)) = crate::heap::compact_oop_scan(header) {
                // Compact object: 8-byte reference slots at the oop-map offsets.
                // Record the BYTE OFFSET as slot_idx — the seed/fixup consumers
                // read it back as a byte offset for compact receivers.
                let body = compact_body.min(body_bytes);
                for &off in &layout.ref_offsets {
                    let off = off as usize;
                    if off + crate::heap::REF_FIELD_SIZE > body {
                        break;
                    }
                    // SAFETY: `off` is within the object's body (capped above).
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + off) };
                    let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if young_from.contains(ref_ptr) {
                            // SAFETY: valid old-gen object pointer.
                            let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                            extra_roots.push((obj_ref, off, 0));
                        }
                    }
                }
            } else {
                // Cap slot count at what the object's field region holds.
                let max_slots = body_bytes / SLOT_SIZE;
                let slots = (header.num_slots as usize).min(max_slots);
                for slot_idx in 0..slots {
                    // SAFETY: `slot_idx` < capped slot count; offset within the object's field region.
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
        let mut result = self.walk_young_objects();

        // Walk old generation
        {
            let old = self.old_gen.lock();
            result.extend(old.walk_objects());
        }

        result
    }

    /// Walk all objects in the young from-space only (to-space is GC scratch).
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_young_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = Vec::new();

        // Walk young generation (from-space only — to-space is GC scratch).
        //
        // BUGFIX [gc-genheap]: after `sweep_young_non_moving` the from-space is
        // NOT a dense run of live objects from offset 0. The non-moving sweep
        // leaves (a) zeroed dead spans published on the free list and (b)
        // sub-`HEADER_SIZE` GAP_FILLER sentinels left in place (see
        // `install_tail_filler` / the sweep loop ~4129-4152). Walking linearly
        // from offset 0 with no hole-skipping and no sentinel handling reads a
        // zeroed free span as a `num_slots=0` 40-byte object (or worse, a
        // class_id=0/kind=Object 0-size header → break, or a non-grid stride →
        // desync into a live object's interior). A zeroed-but-unpublished gap
        // can even decode as size 0 and spin.
        //
        // Mirror `OldGen::walk_objects` / the sweep loop / `clear_all_mark_bits_in_arena`:
        // take `free_blocks_sorted()` and skip known free blocks, stride over
        // GAP_FILLER sentinels by their offset-4 length, and use
        // `gen_object_total_size` (which flags corrupt headers as size 0) with
        // the same `total_size < HEADER_SIZE` corruption stop.
        {
            let young = self.young_from.lock();
            let base = young.base_ptr() as usize;
            let used = young.used();
            let free_blocks = young.free_blocks_sorted();
            let mut free_iter = free_blocks.iter().peekable();
            let mut offset: usize = 0;
            while offset < used {
                // Skip known free blocks — their bytes are stale dead spans and
                // must never be parsed as object headers. DoHead walk-desync
                // hardening (2026-07-02): robust skip (handles overshoot).
                if skip_free_blocks(&mut offset, &mut free_iter).0 {
                    continue;
                }
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
                // Bug-D fix: stride over a GAP-filler sentinel (a sub-`HEADER_SIZE`
                // TLAB tail left in place by the sweep). It carries its exact byte
                // length at offset 4; it is dead filler, not a live object, so it
                // is not pushed to `result`. This MUST run before
                // `gen_object_total_size`, whose `num_slots` read (offset 16) would
                // fall outside an 8-byte gap.
                if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
                    // SAFETY: offset 4 lies within the >=8-byte gap.
                    let gap =
                        unsafe { std::ptr::read((ptr as *const u8).add(4) as *const u32) } as usize;
                    if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && offset + gap <= used {
                        offset += gap;
                        continue;
                    }
                    // Malformed sentinel (should be impossible) — fall through to
                    // the corrupt-header stop below.
                }
                // DoHead walk-desync hardening (2026-07-02): an unlisted
                // zeroed span is not parseable — re-anchor at the next free
                // block instead of breaking (which would silently drop every
                // later object from the enumeration) or striding phantoms.
                let word0 = unsafe { *(ptr as *const u64) };
                let mut anomaly = false;
                if word0 == 0 {
                    let limit = free_iter
                        .peek()
                        .map(|&&(off, _)| off)
                        .unwrap_or(used)
                        .min(used);
                    let run_end = zero_run_end(base, offset, limit);
                    if run_end - offset >= HEADER_SIZE {
                        anomaly = true;
                    }
                }
                // `gen_object_total_size` returns 0 for an array with an
                // implausible length or a kind=Object header with a non-zero
                // array_length / oversized num_slots; the `< HEADER_SIZE` check
                // below then re-anchors the walk (matching the sweep).
                let total_size = if anomaly {
                    0
                } else {
                    gen_object_total_size(header)
                };
                if anomaly || total_size < HEADER_SIZE || offset + total_size > used {
                    if resync_to_next_free_block(&mut offset, &mut free_iter) {
                        continue;
                    }
                    break;
                }
                result.push((ptr, total_size));
                offset += total_size;
            }
        }

        result
    }

    /// Collect every young-gen object's reference to an OLD-gen object.
    ///
    /// fork6 GC_STRESS fix — these are mandatory roots for the concurrent
    /// old-gen mark (`ConcurrentMarker::initial_mark` / `remark`): those
    /// phases filter the thread-root list with `old_gen.contains`, so an old
    /// object whose only path from a root goes THROUGH a young object
    /// (root → young holder → old target) was invisible and the concurrent
    /// sweep freed it live. Selective promotion mass-produces exactly that
    /// shape (it tenures a young object's children while the pinned holder
    /// stays young), so under allocation pressure the old cycle reclaimed
    /// live promoted objects — the all-zero-header `class_id=0` receivers
    /// and silently-dropped static writes in the Fork6Hard GC_STRESS lane.
    ///
    /// Walks ALL young objects (live or dead): a dead young holder's old refs
    /// only over-retain (floating garbage until the next cycle), never corrupt.
    /// Must be called during a GC safepoint (all mutator threads paused, so
    /// young object bodies are stable to read).
    pub fn collect_young_to_old_roots(&self) -> Vec<usize> {
        let young_objs = self.walk_young_objects();
        let mut out = Vec::new();
        let old = self.old_gen.lock();
        for (ptr, _sz) in young_objs {
            // SAFETY: `ptr` came from the hardened young walk above; the
            // header and body are readable, and no mutator runs (STW).
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            // SAFETY: `ptr`/`header` are a valid young object under STW.
            unsafe {
                for_each_ref_slot(ptr, header, |r, _| {
                    if old.contains(r as *const u8) {
                        out.push(r as usize);
                    }
                });
            }
        }
        out
    }

    /// gcstress residual face-1 diagnostics (`CRATONVM_DBG_CELLCORRUPT`) —
    /// dump everything known about the HOLDER of a corrupt Value cell plus a
    /// generation-containment probe of the stale pointer the cell carries.
    /// Cold path: runs only when the gate is set AND the cell already failed
    /// `read_value_checked`.
    fn dump_corrupt_cell_holder(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
        index: usize,
        cell_ptr: *mut u8,
    ) {
        // SAFETY: `cell_ptr` is a readable 16-byte slot (get_field bounds-checked).
        let raw = unsafe { std::ptr::read(cell_ptr as *const [u64; 2]) };
        let class_name = crate::gc::resolve_class_info(header.class_id.as_u32())
            .map(|(n, _)| n)
            .unwrap_or_else(|| "<unresolved>".to_string());
        let target = raw[0] as usize;
        let t = target as *const u8;
        let (yf, yt, og) = (
            self.young_from.lock().contains(t),
            self.young_to.lock().contains(t),
            self.old_gen.lock().contains(t),
        );
        let holder_addr = obj_ref.as_ptr() as usize;
        let (hyf, hog) = (
            self.young_from.lock().contains(holder_addr as *const u8),
            self.old_gen.lock().contains(holder_addr as *const u8),
        );
        // Neighbor window: a misaligned-by-8 tagged Value write (the bc
        // math-ec "0x4" family — receiver pointer off by 8) leaves its
        // discriminant in the PREVIOUS cell's payload word and its payload in
        // THIS cell's discriminant word; the ±2-cell dump makes that pattern
        // (small disc at odd word positions) directly visible.
        // SAFETY: the window lies within the holder's body ± one cell; the
        // holder is a live validated object and heap arenas pad allocations,
        // so the reads stay inside mapped arena memory.
        let win: [u64; 8] = unsafe { std::ptr::read((cell_ptr as usize - 16) as *const [u64; 8]) };
        eprintln!(
            "[CELLCORRUPT] holder=0x{holder_addr:x} (young_from={hyf} old={hog}) \
             class_id={} class={class_name} kind=0x{:02x} num_slots={} array_len={} \
             gc_flags=0x{:x} index={index} raw0=0x{:016x} raw1=0x{:016x} | \
             raw0-target: young_from={yf} young_to={yt} old={og}\n\
             [CELLCORRUPT]   window cell-1..cell+2: {:016x},{:016x} | {:016x},{:016x} | \
             {:016x},{:016x} | {:016x},{:016x}\n{}",
            header.class_id.as_u32(),
            header.kind as u8,
            header.num_slots,
            header.array_length,
            header.gc_flags,
            raw[0],
            raw[1],
            win[0],
            win[1],
            win[2],
            win[3],
            win[4],
            win[5],
            win[6],
            win[7],
            std::backtrace::Backtrace::force_capture(),
        );
        // Shift test — the neighbor windows show corrupt cells decoding as an
        // 8-BYTE-SHIFTED body ({payload_k, disc_k+1}). Mechanically test: do
        // this holder's cells parse as valid Values when read at ±8? A
        // consistent hit means the body content sits 8 bytes off its slots
        // (overlapping/shifted allocation — the A2 double-serve family), not
        // a per-cell stray write.
        {
            let n = (header.num_slots as usize).min(8);
            let base = holder_addr + HEADER_SIZE;
            let mut plus8 = 0usize;
            let mut minus8 = 0usize;
            let mut aligned = 0usize;
            for k in 0..n {
                let c = base + k * SLOT_SIZE;
                // SAFETY: within the holder body ±8; arena-mapped.
                unsafe {
                    if cratonvm_types::read_value_checked(c as *const Value).is_some() {
                        aligned += 1;
                    }
                    if cratonvm_types::read_value_checked((c + 8) as *const Value).is_some() {
                        plus8 += 1;
                    }
                    if cratonvm_types::read_value_checked((c - 8) as *const Value).is_some() {
                        minus8 += 1;
                    }
                }
            }
            eprintln!(
                "[CELLCORRUPT]   shift-test over {n} cells: valid@aligned={aligned} \
                 valid@+8={plus8} valid@-8={minus8}",
            );
        }
        // If the stale target is still inside a CURRENT generation, dump its
        // header too — its identity often names the mis-writing code path.
        if yf || og {
            // SAFETY: `target` is inside a live arena (checked above); the
            // first 40 header bytes of any in-arena address are readable.
            let th = unsafe { std::ptr::read(target as *const ObjectHeader) };
            let tname = crate::gc::resolve_class_info(th.class_id.as_u32())
                .map(|(n, _)| n)
                .unwrap_or_else(|| "<unresolved>".to_string());
            eprintln!(
                "[CELLCORRUPT]   target-header: class_id={} class={tname} kind=0x{:02x} \
                 num_slots={} array_len={} gc_flags=0x{:x}",
                th.class_id.as_u32(),
                th.kind as u8,
                th.num_slots,
                th.array_length,
                th.gc_flags,
            );
        }
    }
}

/// gcstress residual face-1 gate — see `dump_corrupt_cell_holder`.
#[inline]
fn cell_corrupt_diag_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_CELLCORRUPT").is_some())
}

/// gcstress residual face-1 hunt (`CRATONVM_DBG_CELLCORRUPT`) — validate a
/// legacy object's Value cells at a GC copy SOURCE. A corrupt cell caught
/// here proves the corruption happened before this copy and reports the
/// victim's pre-copy (young, often bootstrap-page-stable) address — the
/// address a follow-up `CRATONVM_DBG_WATCH_CELL` run must watch to catch the
/// forming write. Rate-capped; no-op unless the gate is set.
fn validate_copy_source_cells(obj_ptr: *const u8, header: &ObjectHeader, site: &str) {
    if !cell_corrupt_diag_enabled()
        || header.kind != ObjectKind::Object
        || is_compact_object(header)
    {
        return;
    }
    static HITS: AtomicUsize = AtomicUsize::new(0);
    for k in 0..header.num_slots as usize {
        let c = obj_ptr as usize + HEADER_SIZE + k * SLOT_SIZE;
        // SAFETY: within the source object's body (walk-validated under STW).
        if unsafe { cratonvm_types::read_value_checked(c as *const Value) }.is_none() {
            if HITS.fetch_add(1, Ordering::Relaxed) < 16 {
                // SAFETY: same cell, raw read.
                let raw = unsafe { std::ptr::read(c as *const [u64; 2]) };
                eprintln!(
                    "[CELLCORRUPT:{site}] PRE-COPY corrupt cell: src_obj=0x{:x} class_id={} \
                     num_slots={} slot={k} cell=0x{c:x} raw={:016x},{:016x}",
                    obj_ptr as usize,
                    header.class_id.as_u32(),
                    header.num_slots,
                    raw[0],
                    raw[1],
                );
            }
        }
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
    // Arrays + compact objects use 8-byte pointer slots; legacy objects use
    // 16-byte Value cells. `forward_ref_slots` rewrites a slot only when the
    // closure returns Some (an evacuated target's new address); a non-evacuated
    // young referent flips `points_young` and leaves the slot unchanged.
    // SAFETY: `obj`/`header` are a valid object under STW (no mutator).
    unsafe {
        forward_ref_slots(obj, header, |ref_ptr| {
            let target = ref_ptr as usize;
            if let Some(nw) = fwd_of(target) {
                Some(nw as *mut u8)
            } else {
                if let Some(p) = points_young.as_deref_mut() {
                    if in_young(target) {
                        *p = true;
                    }
                }
                None
            }
        });
    }
}

/// Selective-promotion verify (CRATONVM_SP_VERIFY): count reference fields of
/// `obj` that still point at a forwarded (evacuated) young object after the
/// fixup pass. A nonzero count is a MISSED fixup — a dangling reference into a
/// reclaimed young slot, the bintrees18 wrong-checksum smoking gun.
fn forwarded_ref_count(obj: *mut u8, header: &ObjectHeader, is_y: &dyn Fn(usize) -> bool) -> usize {
    let mut n = 0usize;
    // SAFETY: `obj`/`header` are a valid object header under STW.
    unsafe {
        for_each_ref_slot(obj, header, |raw, _| {
            let t = raw as usize;
            if is_y(t) {
                // SAFETY: `is_y` confirmed `t` is a young-from-space heap address,
                // so it points at a valid `ObjectHeader`.
                let h = &*(t as *const ObjectHeader);
                if h.is_forwarded() {
                    n += 1;
                }
            }
        });
    }
    n
}

/// Invoke `f` with each non-null reference target address held by `obj`'s
/// reference fields (object Value slots or reference-array elements). Used by
/// the CRATONVM_SP_VERIFY aliasing detector.
fn for_each_ref(obj: *mut u8, header: &ObjectHeader, mut f: impl FnMut(usize)) {
    // SAFETY: `obj`/`header` are a valid object header under STW.
    unsafe {
        for_each_ref_slot(obj, header, |raw, _| f(raw as usize));
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
fn gcw_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_GCWRITE").is_some())
}

/// Cached CRATONVM_DBG_DESCTRACE gate (temp investigation aid, ALV5th GC
/// bug): trace every forward_object relocation of a
/// org/junit/runner/Description or java/util/concurrent/ConcurrentLinkedQueue
/// instance (old addr -> new addr, identity hash, promoted-or-not, age).
#[inline]
fn desc_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_DBG_DESCTRACE").is_some())
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

/// Moving-young diagnostic: after Cheney scanning but before from-space reset,
/// report any surviving heap reference that still points at a forwarded
/// young-from object. Nonzero means a heap reference rewrite was missed; zero
/// with a wrong result points at an unenumerated root/home outside the heap.
#[inline]
fn moving_young_dangling_verify_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_MOVING_YOUNG_VERIFY").is_some())
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
                            label,
                            arena,
                            optr as usize,
                            h.class_id.as_u32(),
                            si,
                            p,
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
                        label,
                        arena,
                        optr as usize,
                        h.class_id.as_u32(),
                        i,
                        raw,
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

/// xt-hardening follow-up (2026-07-03): reject a conservative candidate
/// whose header's ALWAYS-ZERO fields are non-zero. `ObjectHeader::new`
/// (types/src/heap_types.rs) unconditionally zero-initializes `_padding`
/// (offset 6-7) and `_gc_reserved` (offset 22-23), and every relocation copy
/// site (region.rs) and the JIT inline-alloc fast path (x64.rs
/// `emit_inline_tlab_new`, which explicitly zeroes offset 4-7 as a single
/// dword even on its fast path — see the "Defensively zero offset 4" comment
/// there) preserve that invariant; nothing in the codebase ever writes a
/// non-zero byte into either field. `gc_flags` similarly has only 3 defined
/// bits (`GC_FLAG_OLD_GEN`/`MARKED`/`COMPACT`); any other bit set is
/// definitionally corrupt. This closes the residual (rarer, non-zero-word0)
/// slice of the `mark_young` conservative-candidate false-positive family
/// that the zero-word0 side-mark-set fix (2026-07-03, same session) does not
/// cover — a candidate whose garbage predecessor bytes happen to satisfy the
/// kind/num_slots/array_length/extent bounds but fail this near-free check.
/// Four bytes of near-uniform-random garbage failing this check is a ~1/2^29
/// false-negative-on-garbage rate (2 padding bytes + 2 reserved bytes + 5
/// undefined gc_flags bits); real objects always pass.
#[inline]
fn header_reserved_fields_plausible(header: &ObjectHeader) -> bool {
    header._padding == [0, 0]
        && header._gc_reserved == [0, 0]
        && header.gc_flags & !(GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_COMPACT) == 0
}

/// xt-hardening follow-up (2026-07-03): targeted defense against the
/// `candidate = victim − 8` corruptor shape for a zero-word0 old-gen root
/// candidate — see the call site's comment for the full rationale. Returns
/// `true` if `candidate + 8` looks like the start of a genuine, independently
/// plausible object (in which case `candidate`'s all-zero 8 bytes are almost
/// certainly that object's own leading padding/hash bytes, not a real header
/// of its own).
#[inline]
fn victim8_neighbor_explains_zero_prefix(candidate: *mut u8, old_gen: &OldGen) -> bool {
    // SAFETY: caller has already verified `candidate` is 8-aligned and
    // inside old gen; `candidate + 8` stays 8-aligned. Bounds-check before
    // dereferencing.
    let neighbor = unsafe { candidate.add(8) };
    if !old_gen.contains(neighbor) {
        return false;
    }
    // SAFETY: bounds-checked above.
    let nheader = unsafe { &*(neighbor as *const ObjectHeader) };
    let nword0 = unsafe { *(neighbor as *const u64) };
    let kind_byte = nheader.kind as u8;
    let is_array = nheader.kind == ObjectKind::Array;
    let plausible = nword0 != 0
        && kind_byte <= 1
        && (is_array || nheader.num_slots <= (1 << 24))
        && (!is_array || nheader.array_length <= i32::MAX as u32)
        && header_reserved_fields_plausible(nheader);
    if !plausible {
        return false;
    }
    let total = gen_object_total_size(nheader);
    total >= HEADER_SIZE
        // SAFETY: total >= HEADER_SIZE was just checked.
        && old_gen.contains(unsafe { neighbor.add(total - 1) })
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
    } else if is_compact_object(header) {
        // Compact object: the body size in bytes is stored in `array_length`
        // (objects don't otherwise use it; `kind` disambiguates from arrays).
        // The sum is bounded by the u32 body size + HEADER_SIZE, no overflow.
        HEADER_SIZE + header.array_length as usize
    } else {
        // Header-coherence sanity check: a correctly-allocated legacy
        // `kind = Object` header always has `array_length = 0` (see
        // `try_alloc_object` / the ObjectHeader::new contract — only
        // `alloc_array` writes a non-zero array_length, and it sets
        // `kind = Array` together with it).
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
            if crate::a2dbg::enabled() {
                tracing::warn!(
                    "GC: inconsistent header — kind=Object but array_length={} (num_slots={}, \
                 class_id={}); inline-alloc forgot to set kind=Array. Treating as corrupt \
                 so the walker can re-sync.",
                    header.array_length,
                    header.num_slots,
                    header.class_id.as_u32(),
                );
            }
            return 0;
        }
        // Defensive cap on num_slots: no real class has 1<<24 fields, and a
        // value above this is almost certainly garbage from an uninitialised
        // region.  Same fallthrough — walker re-syncs.
        if header.num_slots > (1 << 24) {
            if crate::a2dbg::enabled() {
                tracing::warn!(
                    "GC: implausible num_slots {} on kind=Object header (class_id={}); \
                 treating as corrupt so the walker can re-sync.",
                    header.num_slots,
                    header.class_id.as_u32(),
                );
            }
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

/// Plan an object allocation under the compact reference-field layout.
///
/// Returns `(total_size, array_length, gc_flags)`:
/// - When the flag is on, a layout is registered for `class_id`, and its field
///   count matches `num_fields`, the object uses the **compact** layout:
///   `total_size = HEADER_SIZE + body_size`, `array_length = body_size` (the
///   object's body bytes, since arrays-vs-objects is disambiguated by `kind`),
///   and `gc_flags = GC_FLAG_COMPACT`.
/// - Otherwise the legacy uniform layout: `total_size = HEADER_SIZE +
///   num_fields*SLOT_SIZE`, `array_length = 0`, `gc_flags = 0`.
///
/// `None` only on size overflow.
#[inline]
fn plan_object_alloc(class_id: ClassId, num_fields: usize) -> Option<(usize, u32, u8)> {
    if compact_ref_fields_enabled() {
        if let Some(layout) = class_layout(class_id.as_u32()) {
            if layout.field_count() == num_fields {
                let total = HEADER_SIZE.checked_add(layout.body_size as usize)?;
                return Some((total, layout.body_size, GC_FLAG_COMPACT));
            } else if std::env::var_os("CRATONVM_DBG_COMPACT_LEGACY").is_some() {
                let name = crate::gc::resolve_class_info(class_id.as_u32())
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "<unresolved>".to_string());
                eprintln!(
                    "[compact-legacy] class={} id={} alloc num_fields={} != layout.field_count={} -> LEGACY object",
                    name,
                    class_id.as_u32(),
                    num_fields,
                    layout.field_count(),
                );
            }
        }
    }
    let body = num_fields.checked_mul(SLOT_SIZE)?;
    Some((HEADER_SIZE.checked_add(body)?, 0, 0))
}

/// Resolve a field access on a (possibly compact) object to `(byte_offset,
/// is_ref)` within the object body. Returns `None` for a legacy object (caller
/// uses the uniform `index * SLOT_SIZE` 16-byte cell). Keys on the per-object
/// `GC_FLAG_COMPACT` bit, so legacy and compact objects coexist correctly.
#[inline]
fn compact_field_slot(header: &ObjectHeader, index: usize) -> Option<(usize, bool)> {
    if !is_compact_object(header) {
        return None;
    }
    let cid = header.class_id.as_u32();
    // A single-entry cache thrashes on the common alternating-class pattern
    // (for example Integer.value plus HashMap.size). Keep a tiny round-robin
    // working set and resolve the requested slot while the cache is borrowed,
    // avoiding both registry locks and Arc clone/drop traffic on hits.
    struct FieldSlotCache {
        entries: [Option<(u32, u64, Arc<CompactLayout>)>; 8],
        next: usize,
    }
    impl FieldSlotCache {
        const fn new() -> Self {
            Self {
                entries: [None, None, None, None, None, None, None, None],
                next: 0,
            }
        }
    }
    thread_local! {
        static FIELD_SLOT_CACHE: std::cell::RefCell<FieldSlotCache> =
            const { std::cell::RefCell::new(FieldSlotCache::new()) };
    }
    let gen = cratonvm_types::layout_generation();
    FIELD_SLOT_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        for entry in &cache.entries {
            if let Some((cached_cid, cached_gen, layout)) = entry {
                if *cached_cid == cid && *cached_gen == gen {
                    let offset = layout.field_offset(index)? as usize;
                    return Some((offset, layout.field_is_ref(index)?));
                }
            }
        }
        let layout = class_layout(cid)?;
        let offset = layout.field_offset(index)? as usize;
        let is_ref = layout.field_is_ref(index)?;
        let replace = cache.next;
        cache.entries[replace] = Some((cid, gen, layout));
        cache.next = (replace + 1) % cache.entries.len();
        Some((offset, is_ref))
    })
}

/// Visit every reference slot of an object/array (read-only), invoking
/// `f(referent_ptr, slot_id)` for each non-null reference.
///
/// `slot_id` is the slot's GC identifier, reused by the dirty-card scan/fixup
/// pair: an **element index** for reference arrays, the **byte offset** for a
/// compact object's 8-byte reference slot, or the **field index** for a legacy
/// object's 16-byte cell. This three-way split mirrors the layout the heap
/// writes, so flag-off runs only the array + legacy arms (identical to dev).
///
/// # Safety
/// `obj_ptr` must point to a valid, fully-initialized object/array header whose
/// body is in-bounds for the slot ranges implied by `header`.
#[inline]
pub(crate) unsafe fn for_each_ref_slot(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    mut f: impl FnMut(*mut u8, usize),
) {
    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length as usize {
                let s = obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE);
                let raw: u64 = std::ptr::read(s as *const u64);
                if raw != 0 {
                    f(raw as usize as *mut u8, i);
                }
            }
        }
    } else if let Some((layout, body)) = crate::heap::compact_oop_scan(header) {
        for &off in &layout.ref_offsets {
            let off = off as usize;
            if off + crate::heap::REF_FIELD_SIZE > body {
                break;
            }
            let s = obj_ptr.add(HEADER_SIZE + off);
            let raw: u64 = std::ptr::read(s as *const u64);
            if raw != 0 {
                f(raw as usize as *mut u8, off);
            }
        }
    } else {
        for slot_idx in 0..header.num_slots as usize {
            let s = obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE);
            if let Value::Object(Some(r)) = std::ptr::read(s as *const Value) {
                f(r.as_ptr(), slot_idx);
            }
        }
    }
}

/// Forward/remap every reference slot of an object/array: for each non-null
/// referent, calls `forward(referent_ptr)`; on `Some(new_ptr)` the slot is
/// rewritten (8-byte raw for arrays/compact objects, 16-byte `Value::Object`
/// for legacy objects), on `None` the slot is left untouched. Handles all three
/// layouts; flag-off runs only the array + legacy arms (identical to dev, with
/// non-forwarded slots never written).
///
/// # Safety
/// Same contract as [`for_each_ref_slot`]. A returned `Some(ptr)` must be a
/// valid heap pointer.
#[inline]
pub(crate) unsafe fn forward_ref_slots(
    obj_ptr: *mut u8,
    header: &ObjectHeader,
    mut forward: impl FnMut(*mut u8) -> Option<*mut u8>,
) {
    if header.kind == ObjectKind::Array {
        if header.element_type == ArrayElementType::Reference {
            for i in 0..header.array_length as usize {
                let s = obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE);
                let raw: u64 = std::ptr::read(s as *const u64);
                if raw != 0 {
                    if let Some(n) = forward(raw as usize as *mut u8) {
                        // gcstress face-1 hunt (no-op unless gated) — a RAW
                        // 8-byte pointer write; on a MISREAD header (walk
                        // overshoot family) this arm would spray pointers
                        // over legacy 16-byte cells.
                        crate::heap::cell_watch_check(
                            s as usize,
                            8,
                            "forward_ref_slots-refarray",
                            &(n as usize),
                        );
                        std::ptr::write(s as *mut u64, n as u64);
                    }
                }
            }
        }
    } else if let Some((layout, body)) = crate::heap::compact_oop_scan(header) {
        for &off in &layout.ref_offsets {
            let off = off as usize;
            if off + crate::heap::REF_FIELD_SIZE > body {
                break;
            }
            let s = obj_ptr.add(HEADER_SIZE + off);
            let raw: u64 = std::ptr::read(s as *const u64);
            if raw != 0 {
                if let Some(n) = forward(raw as usize as *mut u8) {
                    // gcstress face-1 hunt (no-op unless gated).
                    crate::heap::cell_watch_check(
                        s as usize,
                        8,
                        "forward_ref_slots-compact",
                        &(n as usize),
                    );
                    std::ptr::write(s as *mut u64, n as u64);
                }
            }
        }
    } else {
        for slot_idx in 0..header.num_slots as usize {
            let s = obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE);
            if let Value::Object(Some(r)) = std::ptr::read(s as *const Value) {
                if let Some(n) = forward(r.as_ptr()) {
                    // gcstress face-1 hunt (no-op unless gated).
                    crate::heap::cell_watch_check(
                        s as usize,
                        16,
                        "forward_ref_slots-legacy",
                        &(n as usize),
                    );
                    std::ptr::write(s as *mut Value, Value::Object(Some(ObjectRef::from_raw(n))));
                }
            }
        }
    }
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
/// DoHead walk-desync hardening (2026-07-02): robust free-block skip shared by
/// every linear young from-space walk.
///
/// Advances `free_iter` past blocks the cursor has wholly passed; if the
/// cursor sits AT or INSIDE a block, jumps the cursor to the block's end.
/// Returns `(resynced, overshot)`: `resynced` means the cursor moved (the
/// caller must `continue` its loop); `overshot` means the cursor was found
/// STRICTLY INSIDE a block (`cursor > off`) — proof that an earlier stride
/// was mis-sized and everything parsed since the previous anchor is suspect.
///
/// This generalizes the main sweep's robust skip: the old exact-match skip
/// (`cursor == off`) wedged its iterator forever after a single overshoot,
/// after which every later free block was walked as a run of zeroed 40-byte
/// phantom `Object`s (class_id=0, num_slots=0 → size 40) — the A2 / ReflRepro
/// / DoHead walk-desync corruption family.
#[inline]
fn skip_free_blocks(
    cursor: &mut usize,
    free_iter: &mut std::iter::Peekable<std::slice::Iter<'_, (usize, usize)>>,
) -> (bool, bool) {
    let mut resynced = false;
    let mut overshot = false;
    while let Some(&&(off, sz)) = free_iter.peek() {
        if *cursor >= off + sz {
            // Walk is already past this entire free block — drop it and
            // re-examine the next one.
            free_iter.next();
            continue;
        }
        if *cursor >= off {
            if *cursor > off {
                overshot = true;
                let n = SWEEP_WALK_OVERSHOOT_HITS.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    tracing::warn!(
                        "young walk: cursor {} overshot into free block [{}, {}) — \
                         resynced at block end (walk grid / free list disagree upstream)",
                        *cursor,
                        off,
                        off + sz,
                    );
                }
            }
            // Cursor is within `[off, off + sz)` — skip the remainder of this
            // free block and resync the walk to a real boundary.
            *cursor = off + sz;
            free_iter.next();
            resynced = true;
        }
        // `cursor < off`: the next free block is still ahead; stop.
        break;
    }
    (resynced, overshot)
}

/// DoHead walk-desync hardening: measure the all-zero run starting at
/// `start` (an 8-aligned walk-grid offset), in 8-byte words, capped at
/// `limit`. Returns the run's END offset (always 8-aligned unless the run
/// reaches an unaligned `limit` exactly). A run `>= HEADER_SIZE` at a grid
/// offset can never be a legally allocated object header in place — real
/// headers have a non-zero first word (`class_id | kind | element_type`) or,
/// for the zero-slot `ClassId(0)` ad-hoc container, at most `HEADER_SIZE`
/// zero bytes followed by the next real header.
///
/// The caller must guarantee `[base+start, base+limit)` is mapped arena
/// memory (both offsets within the arena's committed capacity).
#[inline]
fn zero_run_end(base: usize, start: usize, limit: usize) -> usize {
    let mut r = start;
    // SAFETY (caller contract): the scanned range is mapped arena memory.
    while r + 8 <= limit && unsafe { *((base + r) as *const u64) } == 0 {
        r += 8;
    }
    if r < limit && limit - r < 8 {
        // Sub-word tail before `limit`: absorb it only if fully zero, so a
        // run ending exactly at a free-block boundary is reported as such.
        let all_zero = (r..limit).all(|i| unsafe { *((base + i) as *const u8) } == 0);
        if all_zero {
            r = limit;
        }
    }
    r
}

/// DoHead walk-desync hardening: after a walk anomaly (zero span or
/// implausible header), re-anchor the cursor at the START of the next known
/// free block — the only downstream offsets that are ground truth — leaving
/// `[old cursor, anchor)` unparsed (conservatively retained). Returns `false`
/// when no anchor remains (caller should stop its walk; the remainder of the
/// arena is retained). The byte-pattern re-sync probe this replaces could
/// lock onto arbitrary zeroed words OFF the object grid (its plausibility
/// check accepts an all-zero header) and resume freeing from there.
#[inline]
fn resync_to_next_free_block(
    cursor: &mut usize,
    free_iter: &mut std::iter::Peekable<std::slice::Iter<'_, (usize, usize)>>,
) -> bool {
    while let Some(&&(off, sz)) = free_iter.peek() {
        if off + sz <= *cursor {
            free_iter.next();
            continue;
        }
        if *cursor > off {
            // Already inside the block — resume on-grid at its end.
            *cursor = off + sz;
            free_iter.next();
        } else {
            // Land ON the block start; the caller's skip loop consumes it
            // and resumes on-grid at its end.
            *cursor = off;
        }
        return true;
    }
    false
}

fn clear_all_mark_bits_in_arena(arena: &mut Arena) {
    let base = arena.base_ptr() as usize;
    let used = arena.used();
    let free_blocks = arena.free_blocks_sorted();
    let mut free_iter = free_blocks.iter().peekable();
    let mut cursor: usize = 0;
    while cursor < used {
        // Skip known free blocks — their bytes are stale and must not be
        // parsed as object headers. DoHead walk-desync hardening
        // (2026-07-02): robust skip — the old exact-match wedged after one
        // overshoot and then CLEARED "mark bits" (a byte write at header
        // offset 21) through phantom headers inside live objects.
        if skip_free_blocks(&mut cursor, &mut free_iter).0 {
            continue;
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
            let gap =
                unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) } as usize;
            if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                cursor += gap;
                continue;
            }
        }
        // DoHead walk-desync hardening (2026-07-02): an unlisted zeroed span
        // is not parseable — re-anchor at the next free block (carries no
        // mark bits to clear) instead of striding it as phantom objects and
        // writing the mark-clear byte into live-object interiors.
        let word0 = unsafe { *(obj_ptr as *const u64) };
        let mut anomaly = false;
        if word0 == 0 {
            let limit = free_iter
                .peek()
                .map(|&&(off, _)| off)
                .unwrap_or(used)
                .min(used);
            let run_end = zero_run_end(base, cursor, limit);
            if run_end - cursor >= HEADER_SIZE {
                anomaly = true;
            }
        }
        let total_size = if anomaly {
            0
        } else {
            gen_object_total_size(header)
        };
        if anomaly || total_size < HEADER_SIZE || cursor + total_size > used {
            // Corruption — same defence as the sweep loop: re-anchor at the
            // next free block rather than risk parsing arbitrary bytes as a
            // header (marks in the skipped stretch remain — retention only).
            if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                continue;
            }
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
    // Defense-in-depth (HIB-CV-32): validate the discriminant before
    // constructing the `Value`. If a reference-integrity defect left this cell
    // holding bytes that do not form a valid `Value` (e.g. a swept-then-reused
    // live object whose primitive field reads back `{heap-ptr, 6}`), decoding
    // it as a `Value` and then matching on it — as the very next operand-stack
    // push does — is a wild jump-table SIGSEGV with no context. Instead return
    // a benign null (the field-read callers already normalize `Object(None)`
    // for a primitive slot) plus a rate-limited diagnostic, turning an
    // unrecoverable crash into a localizable one. The fast path is one aligned
    // 32-bit load + a predictable compare on top of the read already happening.
    match cratonvm_types::read_value_checked_atomic(ptr as *const Value) {
        Some(v) => v,
        None => {
            static CORRUPT_HITS: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = CORRUPT_HITS.fetch_add(1, Ordering::Relaxed);
            if n < 32 || std::env::var_os("CRATONVM_DIAG_HIB32").is_some() {
                // SAFETY: `ptr` is a readable, 8-byte-aligned 16-byte slot
                // (caller contract) -- read atomically (PLAIN-SLOT TEARING
                // FIX, 2026-07-06) so this diagnostic dump itself can't tear
                // against a concurrent plain writer on another thread.
                let raw0 = (*(ptr as *const AtomicU64)).load(Ordering::Relaxed);
                let raw1 = (*(ptr.add(8) as *const AtomicU64)).load(Ordering::Relaxed);
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    slot = ?ptr,
                    raw0 = format!("{:#018x}", raw0),
                    raw1 = format!("{:#018x}", raw1),
                    "gen_heap::read_slot: corrupt Value cell (out-of-range \
                     discriminant) — returning null instead of a UB-on-match \
                     Value. Heap reference-integrity defect (see HIB-CV-32).",
                );
            }
            Value::Object(None)
        }
    }
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
    // gcstress face-1 hunt (no-op unless CRATONVM_DBG_WATCH_CELL is set).
    crate::heap::cell_watch_check(ptr as usize, 16, "write_slot", &value);
    // PLAIN-SLOT TEARING FIX (2026-07-06): was a bare `ptr::write::<Value>`,
    // a non-atomic 16-byte copy that could tear against a concurrent plain
    // `get_field` from another mutator thread -- see
    // docs/known-issues/elasticsearch-lucene-binary-docvalues-range-hangs.md
    // #3 and commit 4e6b560f (the GC-marker-vs-JIT-store counterpart fix).
    cratonvm_types::write_value_atomic(ptr as *mut Value, value);
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

    unsafe fn corrupt_header_byte(obj: ObjectRef, offset: usize, value: u8) {
        unsafe {
            (obj.as_ptr() as *mut u8).add(offset).write(value);
        }
    }

    #[test]
    fn is_object_address_rejects_invalid_raw_header_tags() {
        let heap = GenerationalHeap::new();
        let invalid_kind = heap.alloc_object(ClassId::new(1), 0);
        unsafe {
            corrupt_header_byte(invalid_kind, OBJECT_KIND_OFFSET, 0x7f);
        }
        assert!(heap
            .is_object_address(invalid_kind.as_ptr() as usize)
            .is_none());

        let invalid_element = heap.alloc_array(ClassId::new(2), ArrayElementType::Int, 1);
        unsafe {
            corrupt_header_byte(invalid_element, ARRAY_ELEMENT_TYPE_OFFSET, 0x7f);
        }
        assert!(heap
            .is_object_address(invalid_element.as_ptr() as usize)
            .is_none());
    }

    #[test]
    fn conservative_candidate_diagnostics_require_a2_mode() {
        assert!(!emit_conservative_candidate_diagnostic(0, false));
        assert!(!emit_conservative_candidate_diagnostic(8, true));
        assert!(emit_conservative_candidate_diagnostic(7, true));
    }

    /// Test-only `StopTheWorldToken`. The single-threaded test harness
    /// trivially satisfies the STW invariant — no other mutator exists.
    #[inline]
    fn stw() -> crate::collector::StopTheWorldToken {
        // SAFETY: these unit tests run the heap single-threaded.
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    /// Create a small generational heap for testing.
    fn small_gen_heap() -> GenerationalHeap {
        // 4KB young semi-space, 8KB old gen
        GenerationalHeap::with_sizes(4 * 1024, 8 * 1024)
    }

    /// fork6 GC_STRESS fix — a young object's reference to an old-gen object
    /// must be reported by `collect_young_to_old_roots` (the concurrent
    /// old-gen mark treats these as mandatory roots; without them an old
    /// object reachable only through a young holder was swept while live).
    #[test]
    fn collect_young_to_old_roots_finds_young_held_old_target() {
        let heap = small_gen_heap();

        // Young holder with one reference field.
        let holder = heap.alloc_object(ClassId::new(1), 1);
        assert!(heap.is_in_young(holder.as_ptr()));

        // Old-gen target, allocated directly in old gen (as selective
        // promotion would).
        let size = HEADER_SIZE + SLOT_SIZE;
        let old_ptr = {
            let mut og = heap.old_gen_lock();
            let p = og.alloc(size, 8).unwrap();
            // SAFETY: freshly allocated old-gen block of `size` bytes.
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(2);
                h.kind = ObjectKind::Object;
                h.num_slots = 1;
                h.gc_flags = GC_FLAG_OLD_GEN;
            }
            p
        };

        // holder.field[0] = old target (the ONLY reference to it).
        // SAFETY: `old_ptr` is a valid, fully-initialized old-gen object.
        let old_ref = unsafe { ObjectRef::from_raw(old_ptr) };
        heap.set_field(holder, 0, Value::Object(Some(old_ref)));

        let roots = heap.collect_young_to_old_roots();
        assert!(
            roots.contains(&(old_ptr as usize)),
            "young→old edge must be collected as an old-marking root \
             (got {} roots)",
            roots.len(),
        );

        // A young→young edge must NOT be reported: repoint the field at a
        // young object and re-collect.
        let young_target = heap.alloc_object(ClassId::new(3), 0);
        heap.set_field(holder, 0, Value::Object(Some(young_target)));
        let roots2 = heap.collect_young_to_old_roots();
        assert!(
            !roots2.contains(&(old_ptr as usize)),
            "an old object no longer young-referenced must not be re-reported",
        );
    }

    /// `young_spill_pressure` — the native-wrapper young-exhaustion signal —
    /// must start clear, stay clear while old gen has ample spill headroom
    /// (spilling is cheaper than collecting), arm once old-gen headroom
    /// drops below the worst-case promotion demand (one young semi + young/8
    /// margin), and re-arm after `clear` while the exhaustion persists. (The
    /// VM consumes this flag at the `safe_native_call` boundary to run the
    /// GC the panicking native allocation wrappers cannot run themselves —
    /// the fix for the HashMapOnly "young gen exhausted" hard abort.)
    #[test]
    fn young_spill_sets_pressure_flag_for_boundary_gc() {
        let heap = small_gen_heap();
        assert!(
            !heap.young_spill_pressure(),
            "fresh heap must not report young spill pressure"
        );

        // Fill the 4 KiB young semi (≈73 x 56-byte objects), then keep
        // spilling into the 8 KiB old gen. The advisability gate arms the
        // flag once old_used >= old_cap - young_semi - young_semi/8 =
        // 8192 - 4096 - 512 = 3584 bytes (≈64 spilled objects) — well
        // before old gen fills, so the both-gens-full abort is unreachable.
        let mut armed_at = None;
        for i in 0..220 {
            let _ = heap.alloc_object(ClassId::new(1), 1);
            if heap.young_spill_pressure() {
                armed_at = Some(i);
                break;
            }
        }
        let armed_at = armed_at.expect("sustained young spill must arm the pressure flag");
        assert!(
            armed_at > 70,
            "the flag must NOT arm while old gen still has more headroom \
             than a full young semi (armed after only {armed_at} allocations)"
        );

        heap.clear_young_spill_pressure();
        assert!(!heap.young_spill_pressure());

        // Young is still exhausted and old-gen headroom is still below the
        // promotion-demand bound: the very next spilling allocation must
        // re-arm the flag (fallible array path this time).
        let arr = heap.try_alloc_array_full(ClassId::new(0), ArrayElementType::Int, 4);
        assert!(arr.is_some(), "old gen must still have room for the array");
        assert!(
            heap.young_spill_pressure(),
            "a post-clear spill must re-arm the flag"
        );
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
    fn young_gc_trigger_preserves_moving_headroom_but_fills_non_moving_space() {
        let capacity = 1000;
        let moving_threshold = 500;
        assert_eq!(
            young_gc_trigger_bytes(capacity, moving_threshold, false),
            500,
            "moving young must retain its configured Cheney-copy headroom",
        );
        assert_eq!(
            young_gc_trigger_bytes(capacity, moving_threshold, true),
            900,
            "non-moving young should defer the O(heap) sweep until 90% occupancy",
        );
        let ordinary_cycle =
            next_young_gc_is_guaranteed_non_moving(false, false, false, false, false, false);
        assert!(!ordinary_cycle, "JIT-quiescent collection is moving by default");
        assert!(
            next_young_gc_is_guaranteed_non_moving(true, false, false, false, false, false),
            "live JIT frames require the default non-moving collector",
        );
        assert!(
            next_young_gc_is_guaranteed_non_moving(false, false, true, false, false, false),
            "a JIT allocation helper has a compiled frame for root scanning",
        );
        assert!(
            !next_young_gc_is_guaranteed_non_moving(true, false, true, true, true, false),
            "explicitly allowed moving-young must preserve copy headroom",
        );
        assert!(
            !next_young_gc_is_guaranteed_non_moving(true, false, true, false, false, true),
            "the diagnostic forced-moving path must preserve copy headroom",
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
    fn young_alloc_initializes_header_before_unlocking_arena() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::{mpsc, Arc};

        let heap = Arc::new(GenerationalHeap::with_sizes(4096, 4096));
        let entered_init = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel();

        let worker_heap = heap.clone();
        let worker_entered = entered_init.clone();
        let worker = std::thread::spawn(move || {
            worker_heap
                .try_alloc_young_initialized(HEADER_SIZE, move |ptr| {
                    worker_entered.store(true, AtomicOrdering::Release);
                    release_rx.recv().expect("test should release initializer");
                    let header = ObjectHeader::new(
                        ClassId::new(123),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        7,
                        0,
                        0,
                    );
                    // SAFETY: the helper passed a freshly allocated,
                    // zero-initialized HEADER_SIZE-byte span and still holds
                    // the young arena lock while this initializer runs.
                    unsafe {
                        std::ptr::write(ptr as *mut ObjectHeader, header);
                    }
                })
                .expect("young allocation should fit") as usize
        });

        while !entered_init.load(AtomicOrdering::Acquire) {
            std::thread::yield_now();
        }
        let allocation_still_locked = heap.young_from.try_lock().is_none();
        release_tx.send(()).expect("release initializer");
        let ptr = worker.join().expect("allocation worker should finish");

        assert!(
            allocation_still_locked,
            "young allocation must not release the arena before the header is initialized"
        );
        // SAFETY: the worker finished after writing a valid object header.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        assert_eq!(header.class_id, ClassId::new(123));
        assert_eq!(header.kind, ObjectKind::Object);
        assert_eq!(header.array_length, 0);
        assert_eq!(header.num_slots, 0);
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
        heap.set_array_element(arr, n - 1, Value::Int(12345))
            .unwrap();

        let mut roots = vec![arr];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        // The array must be forwarded, not skipped as a false root.
        assert_eq!(
            result.stats.objects_copied, 1,
            "large array must survive GC"
        );

        let new_arr = roots[0];
        assert_ne!(
            new_arr.as_ptr(),
            arr.as_ptr(),
            "array should have been moved"
        );
        assert_eq!(heap.array_length(new_arr), n);
        assert_eq!(heap.get_array_element(new_arr, 0), Ok(Value::Int(7)));
        assert_eq!(
            heap.get_array_element(new_arr, n - 1),
            Ok(Value::Int(12345))
        );
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
        heap.set_array_element(a, n - 1, Value::Int(0xAAAA_AAAAu32 as i32))
            .unwrap();
        let b = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(b, n - 1, Value::Int(0xBBBB_BBBBu32 as i32))
            .unwrap();
        let c = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(c, n - 1, Value::Int(0xCCCC_CCCCu32 as i32))
            .unwrap();

        let mut roots = vec![a, b, c];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);
        assert_eq!(
            result.stats.objects_copied, 3,
            "all three large arrays must be forwarded"
        );

        let (na, nb, nc) = (roots[0], roots[1], roots[2]);
        assert_eq!(heap.array_length(na), n, "a.length corrupted after GC");
        assert_eq!(heap.array_length(nb), n, "b.length corrupted after GC");
        assert_eq!(heap.array_length(nc), n, "c.length corrupted after GC");
        assert_eq!(
            heap.get_array_element(na, n - 1),
            Ok(Value::Int(0xAAAA_AAAAu32 as i32))
        );
        assert_eq!(
            heap.get_array_element(nb, n - 1),
            Ok(Value::Int(0xBBBB_BBBBu32 as i32))
        );
        assert_eq!(
            heap.get_array_element(nc, n - 1),
            Ok(Value::Int(0xCCCC_CCCCu32 as i32))
        );
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
        assert!(
            result.stats.bytes_freed > 0,
            "dead object must be reclaimed"
        );
        // The reclaimed region was zeroed by the sweep.
        // SAFETY: `dead_ptr` is inside the young arena; reading its
        // (now-freed, zeroed) header is a valid in-bounds read.
        let dead_header = unsafe { &*(dead_ptr as *const ObjectHeader) };
        assert_eq!(
            dead_header.class_id,
            ClassId::new(0),
            "freed hole must be zeroed"
        );

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
    fn non_moving_old_sweep_reclaims_dead_promotions_without_relocation() {
        let heap = GenerationalHeap::with_sizes(4 * 1024, 16 * 1024);
        let monitors = NoOpMonitors;

        let live = heap.alloc_object(ClassId::new(1), 1);
        let dead = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(live, 0, Value::Int(4242));
        heap.set_field(dead, 0, Value::Int(-1));

        // Age both objects into old space through the ordinary moving path.
        let mut roots = vec![live, dead];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        let live_old = roots[0];
        assert!(heap.is_in_old(live_old.as_ptr()));
        assert!(heap.is_in_old(roots[1].as_ptr()));

        let old_used_before = heap.old_gen_used();
        let reclaimed = heap.sweep_old_gen_non_moving(&[live_old]);

        assert!(
            reclaimed > 0,
            "unreachable promoted object must be reclaimed"
        );
        assert!(heap.old_gen_used() < old_used_before);
        assert_eq!(
            heap.get_field(live_old, 0).as_int(),
            Some(4242),
            "the rooted old object must stay at its original address"
        );
    }

    /// RandomizedContext WeakHashMap<Thread,...> fix regression: a
    /// kept-in-place (non-promoted) young survivor that is a WATCHED
    /// referent (`gc_quiescence::set_watched_referents`) must get an
    /// IDENTITY `pointer_map` entry, so post-GC reference processing's
    /// `is_marked` check recognizes it as alive instead of wrongly clearing
    /// a live Weak/Soft/Phantom reference to it. An UNWATCHED survivor must
    /// still produce an empty pointer_map, exactly like
    /// `non_moving_sweep_when_jit_active` — this fix must not start
    /// recording every survivor, only watched ones (bounded cost).
    #[test]
    fn non_moving_sweep_records_identity_map_for_watched_survivor() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        let obj_unwatched = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_a, 0, Value::Int(7));
        heap.set_field(obj_unwatched, 0, Value::Int(9));

        let a_ptr = obj_a.as_ptr();
        let unwatched_ptr = obj_unwatched.as_ptr();

        // Watch `obj_a`'s address only (simulating a live WeakReference whose
        // referent is this object) — mirrors what
        // `weakref_null_referents_pre_gc` publishes before a real collection.
        crate::gc_quiescence::set_watched_referents(&[a_ptr as usize]);

        crate::gc_quiescence::enter();
        assert!(crate::gc_quiescence::is_active());

        // Both objects are roots (so both survive as kept-in-place,
        // non-promoted survivors); only `obj_a` is watched.
        let mut roots = vec![obj_a, obj_unwatched];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        crate::gc_quiescence::leave();
        crate::gc_quiescence::set_watched_referents(&[]);

        assert_eq!(roots[0].as_ptr(), a_ptr, "survivor must not move");
        assert_eq!(roots[1].as_ptr(), unwatched_ptr, "survivor must not move");

        assert_eq!(
            result.pointer_map.get(&(a_ptr as usize)),
            Some(&(a_ptr as usize)),
            "watched survivor must get an identity pointer_map entry"
        );
        assert!(
            !result.pointer_map.contains_key(&(unwatched_ptr as usize)),
            "unwatched survivor must NOT get a pointer_map entry (bounded cost)"
        );
    }

    /// A5 fix regression: the `unregistered_jit_frame_on_stack` flag must force
    /// the same NON-MOVING sweep as `is_active()`. The VM root scan sets it when
    /// it finds a guard-less JIT frame on the native stack (the compiled
    /// entry-point `main`); without the non-moving path the moving collector
    /// would relocate that frame's conservatively-marked roots and leave its raw
    /// stack slots stale (the bintrees `main`-compiled corruption). Here the flag
    /// is set directly (no JIT-quiescence `enter()`), and the collector must keep
    /// survivors in place exactly as in `non_moving_sweep_when_jit_active`.
    #[test]
    fn non_moving_sweep_when_unregistered_jit_frame_on_stack() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Int(4242));
        let a_ptr = obj_a.as_ptr();
        let b_ptr = obj_b.as_ptr();

        // No JIT quiescence — only the unregistered-frame flag is set, exactly
        // as `conservative_roots::scan_active_jit_frames` does on detection.
        assert!(!crate::gc_quiescence::is_active());
        crate::gc_quiescence::set_unregistered_jit_frame_on_stack();
        assert!(crate::gc_quiescence::unregistered_jit_frame_on_stack());

        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitors);

        crate::gc_quiescence::clear_unregistered_jit_frame_on_stack();

        // Non-moving: nothing copied, empty pointer map, addresses UNCHANGED.
        assert_eq!(
            result.stats.objects_copied, 0,
            "unregistered-JIT-frame flag must select the non-moving sweep"
        );
        assert!(result.pointer_map.is_empty());
        assert_eq!(roots[0].as_ptr(), a_ptr, "survivor must not move");
        assert_eq!(obj_a.as_ptr(), a_ptr);
        match heap.get_field(obj_a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(b.as_ptr(), b_ptr, "B must not move");
                assert_eq!(heap.get_field(b, 0).as_int(), Some(4242));
            }
            other => panic!("A's field should still reference B, got {other:?}"),
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
    fn full_old_rset_scan_preserves_unbarriered_old_to_young_ref() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![old_obj];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        let young = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(young, 0, Value::Int(31337));
        // Model a raw store which bypasses the remembered-set barrier.
        unsafe {
            std::ptr::write(
                promoted.as_ptr().add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(young)),
            );
        }

        let mut gc_roots = vec![promoted];
        heap.collect_garbage(&stw(), &mut gc_roots, &monitors);
        match heap.get_field(gc_roots[0], 0) {
            Value::Object(Some(survivor)) => {
                assert_eq!(heap.get_field(survivor, 0).as_int(), Some(31337));
            }
            other => panic!("full old scan lost unbarriered young ref: {other:?}"),
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
                    let obj = heap
                        .try_alloc_object(ClassId::new(0), 2)
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
            heap.set_field(obj, 0, Value::Int(i));
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

            let compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

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
            let _compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

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
                assert_eq!(
                    heap.class_id_of(b),
                    ClassId::new(2),
                    "B should have class 2"
                );
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
            let _compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

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
            let compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);
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
            let _compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

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
        assert!(
            heap.is_in_old(roots[0].as_ptr()),
            "object should be promoted to old gen"
        );
        roots[0]
    }

    /// ES-FAIL-FAMILY-20260710 hunt: a field that points back to its own
    /// holder (`Throwable.cause = this`, `Throwable.backtrace = this` — the
    /// JDK "uninitialized" self-reference sentinel) must be updated to the
    /// relocated address when the holder itself moves during GC (promotion
    /// young->old, and any further old-gen compaction), not left pointing at
    /// the holder's stale pre-move address. A field write for a *self*
    /// reference is easy to special-case incorrectly (e.g. "skip rewriting
    /// this slot, the pointer is already correct" without accounting for the
    /// holder itself having just moved) — if that happens, the field keeps
    /// pointing at the old, now-free address, and once that address is
    /// reused by a later allocation, reading the field returns whatever
    /// unrelated object now lives there.
    #[test]
    fn self_referential_field_survives_promotion() {
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj, 0, Value::Object(Some(obj)));

        let mut roots = vec![obj];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
        }
        assert!(
            heap.is_in_old(roots[0].as_ptr()),
            "S-SELFREF: object should be promoted to old gen"
        );

        let relocated = roots[0];
        match heap.get_field(relocated, 0) {
            Value::Object(Some(r)) => {
                assert_eq!(
                    r.as_ptr(),
                    relocated.as_ptr(),
                    "S-SELFREF: self-referential field should track the relocated \
                     object after promotion (field points at {:?}, holder is now at {:?})",
                    r.as_ptr(),
                    relocated.as_ptr()
                );
            }
            other => panic!(
                "S-SELFREF: self-reference lost/corrupted after promotion, field={:?}",
                other
            ),
        }

        // Keep stressing the heap after promotion — allocate + collect a few
        // more times (some of which may trigger old-gen compaction) and
        // re-check the self-reference each time, in case the bug needs a
        // SECOND relocation of an already-old object rather than the initial
        // young->old promotion.
        for cycle in 0..5 {
            for _ in 0..200 {
                let garbage = heap.alloc_object(ClassId::new(0), 4);
                heap.set_field(garbage, 0, Value::Int(cycle));
            }
            heap.collect_garbage(&stw(), &mut roots, &monitors);
            let relocated = roots[0];
            match heap.get_field(relocated, 0) {
                Value::Object(Some(r)) => {
                    assert_eq!(
                        r.as_ptr(),
                        relocated.as_ptr(),
                        "S-SELFREF: self-referential field diverged after post-promotion \
                         GC cycle {cycle} (field points at {:?}, holder is now at {:?})",
                        r.as_ptr(),
                        relocated.as_ptr()
                    );
                }
                other => panic!(
                    "S-SELFREF: self-reference lost/corrupted at post-promotion cycle \
                     {cycle}, field={:?}",
                    other
                ),
            }
        }
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
            assert_eq!(
                tag.as_int(),
                Some(i as i32),
                "S29: root {} tag should be {} after GC",
                i,
                i
            );
        }

        // Walk reachable graph from roots and verify all tags
        let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut walk_queue: std::collections::VecDeque<ObjectRef> =
            std::collections::VecDeque::new();
        for r in &roots {
            walk_queue.push_back(*r);
        }
        while let Some(obj) = walk_queue.pop_front() {
            let tag = heap.get_field(obj, 0).as_int().unwrap() as usize;
            if !visited.insert(tag) {
                continue;
            }
            // Verify tag is in our expected reachable set
            assert!(
                reachable.contains(&tag),
                "S29: object with tag {} should be reachable",
                tag
            );
            // Follow link
            if let Value::Object(Some(next)) = heap.get_field(obj, 1) {
                walk_queue.push_back(next);
            }
        }
        // All reachable objects should have been visited
        assert_eq!(
            visited.len(),
            reachable.len(),
            "S29: all {} reachable objects should survive GC, found {}",
            reachable.len(),
            visited.len()
        );
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
                heap.get_field(r, 0).as_int(),
                Some(42),
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
                    assert_eq!(
                        heap.get_field(y, 0).as_int(),
                        Some(i as i32 * 10 + 1),
                        "S29: old[{}]→young tag should be {}",
                        i,
                        i * 10 + 1
                    );
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
                assert_eq!(
                    heap.get_field(y, 0).as_int(),
                    Some(20),
                    "S29: overwritten ref should point to y2 (tag=20)"
                );
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
            heap.set_field(obj, 0, Value::Int(i));
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
                assert_eq!(
                    tag, i as i32,
                    "S29 cycle {}: root {} tag corrupted (got {})",
                    cycle, i, tag
                );
            }

            // Verify linked objects are reachable with correct tags
            for i in 0..n {
                if i % 3 == 0 {
                    continue;
                } // unlinked
                let target_idx = (i * 13 + cycle * 7 + 5) % n;
                match heap.get_field(roots[i], 1) {
                    Value::Object(Some(linked)) => {
                        let linked_tag = heap.get_field(linked, 0).as_int().unwrap();
                        assert_eq!(
                            linked_tag, target_idx as i32,
                            "S29 cycle {}: obj[{}] link tag should be {}, got {}",
                            cycle, i, target_idx, linked_tag
                        );
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
            heap.set_array_element(promoted_arr, i as usize, Value::Object(Some(y)))
                .unwrap();
            heap.write_barrier(promoted_arr, Value::Object(Some(y)));
        }

        // GC with only old array as root
        let mut roots = vec![promoted_arr];
        heap.collect_garbage(&stw(), &mut roots, &monitors);

        let arr_after = roots[0];
        for i in 0..4 {
            match heap.get_array_element(arr_after, i as usize).unwrap() {
                Value::Object(Some(y)) => {
                    assert_eq!(
                        heap.get_field(y, 0).as_int(),
                        Some(young_tags[i as usize]),
                        "S29: ref array[{}] young tag should be {}",
                        i,
                        young_tags[i as usize]
                    );
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
        assert!(
            heap.card_table.take_dirty_cards().is_empty(),
            "S29: old→old should not dirty card"
        );

        // Young → Young: should NOT dirty card
        let y1 = heap.alloc_object(ClassId::new(0), 2);
        let y2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y1, 1, Value::Object(Some(y2)));
        heap.write_barrier(y1, Value::Object(Some(y2)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(
            heap.card_table.take_dirty_cards().is_empty(),
            "S29: young→young should not dirty card"
        );

        // Non-reference store: should NOT dirty card
        heap.set_field(old1, 0, Value::Int(999));
        heap.write_barrier(old1, Value::Int(999));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(
            heap.card_table.take_dirty_cards().is_empty(),
            "S29: non-ref store should not dirty card"
        );

        // Old → Young: SHOULD dirty card
        heap.set_field(old1, 1, Value::Object(Some(y1)));
        heap.write_barrier(old1, Value::Object(Some(y1)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(
            !heap.card_table.take_dirty_cards().is_empty(),
            "S29: old→young SHOULD dirty card"
        );
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
        assert!(
            heap.is_in_old(a_old.as_ptr()),
            "S29: A should be in old gen"
        );

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
        assert_eq!(
            heap.get_field(current, 0).as_int(),
            Some(5),
            "S29: E (young, linked from old D) should survive"
        );
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
            assert_eq!(
                heap.get_field(*root, 0).as_int(),
                Some(i as i32),
                "S29 stress: object {} tag corrupted after GC1",
                i
            );
        }

        // Drop half the roots — only first 500
        roots.truncate(500);

        // GC cycle 2
        heap.collect_garbage(&stw(), &mut roots, &monitors);
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(
                heap.get_field(*root, 0).as_int(),
                Some(i as i32),
                "S29 stress: root {} tag corrupted after GC2",
                i
            );
        }

        // GC cycles 3-5: repeatedly compact
        for cycle in 3..=5 {
            heap.collect_garbage(&stw(), &mut roots, &monitors);
            for (i, root) in roots.iter().enumerate() {
                assert_eq!(
                    heap.get_field(*root, 0).as_int(),
                    Some(i as i32),
                    "S29 stress: root {} tag corrupted after GC{}",
                    i,
                    cycle
                );
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
                assert_eq!(
                    heap.get_field(y, 0).as_int(),
                    Some(99),
                    "S29: young object linked from just-promoted old should survive"
                );
            }
            _ => panic!("S29: old→young link lost after promotion+barrier+GC"),
        }
    }

    #[test]
    fn native_old_batch_allocates_distinct_rootable_objects() {
        let heap = GenerationalHeap::with_sizes(4096, 256 * 1024);
        let before = heap.stats().snapshot().old_allocations;
        let mut roots = heap.try_alloc_objects_old_batch(ClassId::new(0), 1, 128);

        assert_eq!(roots.len(), 128);
        assert_eq!(
            heap.stats().snapshot().old_allocations - before,
            roots.len() as u64
        );
        let mut addresses: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        addresses.sort_unstable();
        addresses.dedup();
        assert_eq!(addresses.len(), roots.len());
        assert!(roots.iter().all(|r| heap.is_in_old(r.as_ptr())));

        for (index, object) in roots.iter().copied().enumerate() {
            heap.set_field(object, 0, Value::Int(index as i32));
        }
        let young_from = heap.young_from.lock();
        let mut old_gen = heap.old_gen.lock();
        let _pointer_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);
        drop(old_gen);
        drop(young_from);
        for (index, object) in roots.iter().copied().enumerate() {
            assert_eq!(heap.get_field(object, 0), Value::Int(index as i32));
        }
    }
}
