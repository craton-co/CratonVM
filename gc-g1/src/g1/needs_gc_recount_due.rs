#[allow(unused_imports)]
use super::*;

/// Return whether this mutator should validate G1's maintained free-region
/// census on this query. Kept outside [`G1Collector`] so the common path is a
/// TLS load/store rather than a shared atomic read-modify-write.
#[inline]
pub fn needs_gc_recount_due() -> bool {
    NEEDS_GC_RECOUNT_TICK.with(|tick| {
        let next = tick.get() + 1;
        if next >= NEEDS_GC_RECOUNT_INTERVAL {
            tick.set(0);
            true
        } else {
            tick.set(next);
            false
        }
    })
}

/// F-13 — evacuation worker count for this collector's configuration.
///
/// Free-standing because F-12 sizes the concurrent-mark deques from it in
/// `G1Collector::new`, before there is a `&self` to ask.
///
/// Precedence: the `CRATONVM_G1_WORKERS` diagnostic override, then an explicit
/// `gc_worker_threads` (including requests above the CPU count), then the machine-derived
/// ergonomic. See [`ergonomic_gc_worker_threads`].
pub fn gc_worker_threads_for(config: &G1CollectorConfig) -> usize {
    // Diagnostic override: `CRATONVM_G1_WORKERS=N` forces the worker count
    // (e.g. =1 to drain the parallel path serially and isolate concurrency
    // races from logic divergences). Falls back to the config otherwise.
    if let Some(n) = gc_flags().g1_workers {
        return n.max(1);
    }
    // The machine count, NOT the affinity mask: HotSpot 25 on Windows sizes
    // `Parallel Workers` from all CPUs too (measured 2026-10-22 under a
    // 0xFFFF mask on the 32-CPU host: `CPUs: 32 total, 32 available`,
    // `Parallel Workers: 23`, `availableProcessors()` 32), so an
    // affinity-aware count here would break the `gc+init` parity.
    let avail = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);
    match config.gc_worker_threads {
        0 => ergonomic_gc_worker_threads(avail),
        cfg => cfg,
    }
}

/// F-12 — concurrent-mark worker count for this collector's configuration.
///
/// A quarter of the evacuation worker count, rounded up, which is HotSpot's
/// `ConcGCThreads` ergonomic. The division is the whole point and is not a
/// hedge: evacuation workers run inside a stop-the-world pause where the
/// application is stopped and every core is free, while marking workers run
/// BESIDE the application. Sizing the marker at the pause's width would hand a
/// quarter to a third of the machine to the collector for the length of a
/// concurrent cycle, which is how you shorten a mark and lengthen the program.
///
/// `CRATONVM_G1_PARALLEL_MARK=0` pins it to 1 — the single worker
/// `ConcurrentMarkController::spawn` used to start unconditionally — and is the
/// bisection lever for F-12. `CRATONVM_G1_WORKERS=N` still reaches this through
/// [`gc_worker_threads_for`], so `=1` gives a single marker too.
pub fn concurrent_mark_worker_count(config: &G1CollectorConfig) -> usize {
    gc_worker_threads_for(config).div_ceil(4).max(1)
}

/// F-13 — evacuation worker count for a machine with `cpus` hardware threads.
///
/// HotSpot's `ParallelGCThreads` ergonomic: one worker per CPU up to 8, then
/// five eighths of the rest. The taper is not arbitrary — evacuation workers
/// contend on the free-region pool and on to-space TLAB claims, so the marginal
/// worker is worth progressively less, and on a large machine a GC that used
/// every core would evict the application's caches during a pause it is
/// supposed to be shortening.
///
/// | cpus | workers |
/// |------|---------|
/// | 1    | 1       |
/// | 4    | 4       |
/// | 8    | 8       |
/// | 16   | 13      |
/// | 32   | 23      |
/// | 64   | 43      |
pub fn ergonomic_gc_worker_threads(cpus: usize) -> usize {
    let cpus = cpus.max(1);
    if cpus <= 8 {
        cpus
    } else {
        8 + (cpus - 8) * 5 / 8
    }
}

/// F-06 — the TAMS rule as `cleanup` used to state it, against the global
/// mark-start snapshot.
///
/// Retained only as the oracle for the `debug_assert_eq!` in `cleanup`: the
/// authority is now `G1Region::tams`, and this exists so that a divergence
/// between the per-region mirror and the snapshot it was copied from is a
/// failing test rather than a silently wrong liveness verdict.
pub fn cleanup_tams_from_snapshot(
    snapshot: &[(u64, usize, RegionType)],
    region_idx: usize,
    region: &G1Region,
) -> usize {
    if snapshot.is_empty() {
        return region.cursor();
    }
    match snapshot.get(region_idx) {
        Some(&(epoch, snap_cursor, snap_type))
            if epoch == region.reuse_epoch && snap_type == region.region_type() =>
        {
            snap_cursor.min(region.cursor())
        }
        _ => 0,
    }
}

/// Dead objects `cleanup` scrubbed in regions it kept ([`scrub_dead_refs_below_tams`]).
pub static CLEANUP_SCRUBBED_OBJECTS: AtomicUsize = AtomicUsize::new(0);
/// Their total size in bytes.
pub static CLEANUP_SCRUBBED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// `CRATONVM_G1_CLEANUP_SCRUB=0` -- leave dead objects in regions `cleanup`
/// keeps exactly as they were (see [`scrub_dead_refs_below_tams`]), the
/// behaviour before 2026-09-30. Default on.
pub fn g1_cleanup_scrub() -> bool {
    None::<std::ffi::OsString>
        .and_then(|v| v.into_string().ok())
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        })
        .unwrap_or(true)
}

/// Null every reference slot of every dead object below `tams` in an Old
/// region `cleanup` keeps -- the reference half of HotSpot G1's scrubbing of
/// dead objects after a mark.
///
/// # Why
///
/// Cleanup frees an Old region whose every object is dead, and eager reclaim
/// frees a dead humongous span; a dead object in a region that ALSO holds a
/// live one stays where it is. Its slots still name whatever it named when it
/// died, and cleanup has just freed some of those targets. Every linear walker
/// then meets a slot that names freed or reused memory: the remembered-set
/// source walk follows it (`parallel-seed[array]: REJECTED a non-object
/// candidate`, or worse, an unrelated object that now starts there), and the
/// post-evacuation verifier reports it as a use-after-free (`SECURITY V7b` on
/// `NativeFactoryReclaimProbe`, whose dropped `byte[][]` shares an Old region
/// with boot objects). A dead object that holds no reference is inert.
///
/// Only the references are cleared: the header, the size and every primitive
/// stay, so a walker, a heap census or a weak side table that still reads the
/// dead object sees the same object with null fields.
///
/// Sound for the reason cleanup's own in-place free is: below TAMS the bitmap
/// is authoritative (`live_bytes` is computed from it), and the caller only
/// reaches this when cleanup trusted the closure. JNI-pinned regions are
/// scrubbed too since gci i15/ra F7 (a pinned array is live and primitive).
/// JIT pins are traced by final remark before a pinned region may be
/// scrubbed: pinning prevents relocation, but does not make every other
/// object sharing that region live.
///
/// What this cannot reach (gci i16/nc): a dead object AT OR ABOVE TAMS (a
/// copy an evacuation made of an unreachable object during the cycle -- the
/// region-granular source walk copies whatever a dead holder's slot names --
/// or anything in a region retyped during the cycle, whose TAMS is 0), and
/// the tail of a region behind the `break` below. Their slots into regions
/// this cleanup frees are nulled by [`scrub_slots_into_freed_regions`].
///
/// # Safety
/// Stop-the-world, regions write guard held; `base + tams` inside `region`'s
/// allocated prefix.
pub unsafe fn scrub_dead_refs_below_tams(
    region: &G1Region,
    base: usize,
    tams: usize,
    jit_skips: &[(usize, usize)],
) -> (usize, usize) {
    let mut objs = 0usize;
    let mut bytes = 0usize;
    let mut offset = 0usize;
    // JIT round 23 wave 8, lane L9: `CRATONVM_G1_WALK_SIZE_MEMO`.
    let mut sizes = WalkSizeMemo::new();
    while offset < tams {
        let addr = base + offset;
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, addr) {
            offset += skip;
            continue;
        }
        if let Some(gap) = gap_filler_len(addr as *const u8) {
            offset += gap;
            continue;
        }
        // SAFETY: `offset < tams <= cursor`, inside the allocated prefix.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if is_humongous_filler(header) {
            break;
        }
        let size = sizes.size(header);
        if size < cratonvm_types::MIN_OBJECT_SIZE || offset + size > tams {
            // An extent this walk cannot trust ends it: the rest of the region
            // keeps its slots, which is what it did before this scrub existed.
            break;
        }
        if !region.mark_bitmap.is_marked(addr) && !is_tlab_filler_header(header) {
            let nulled = unsafe { clear_dead_object_reference_edges(addr, header, size) };
            if nulled {
                objs += 1;
                bytes += size;
            }
        }
        offset += size;
    }
    (objs, bytes)
}

/// Slots in a [`WalkSizeMemo`] (a power of two).
pub const WALK_SIZE_MEMO_SLOTS: usize = 16;

/// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_WALK_SIZE_MEMO`, default ON,
/// `=0` kills): [`object_total_size`] for a linear region walk, with the
/// compact-object answer memoised per walk.
///
/// A compact object's size is its class layout's `total_size`, looked up per
/// object through `with_class_layout` (a layout-generation load, a flag read,
/// the thread-local `ScanCache` and its `RefCell` borrow). The cleanup scrubs
/// walk every object below TAMS of every region, so on `KeepLong -Xmx1g` that
/// lookup was 15-31% of the pausing thread (all-thread RIP sampler,
/// n23-w8c). A heap of a few classes asks the same `(class_id, num_slots)`
/// question millions of times.
///
/// The answer is IDENTICAL by construction:
/// - only compact objects are memoised (arrays and legacy objects are pure
///   header arithmetic and go straight to `object_total_size`);
/// - the key is the whole lookup key, `(class_id, num_slots)`;
/// - every lookup compares the current `layout_generation()` with the
///   generation the memo was filled under, and clears it on a change: the
///   registry bumps the generation on every register and unregister, which
///   is the same invalidation rule `ScanCache` itself relies on;
/// - a miss in the registry (`IMPLAUSIBLE_BODY_SIZE`, 1 TiB) is never
///   memoised, so a later registration is seen.
///
/// Per walk (a stack local), so no process or thread global is added.
pub struct WalkSizeMemo {
    pub enabled: bool,
    pub generation: u64,
    pub keys: [u64; WALK_SIZE_MEMO_SLOTS],
    pub sizes: [usize; WALK_SIZE_MEMO_SLOTS],
}

impl WalkSizeMemo {
    #[inline]
    pub fn new() -> Self {
        Self::with_enabled(g1_walk_size_memo_enabled())
    }

    #[inline]
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            generation: cratonvm_types::layout_generation(),
            keys: [u64::MAX; WALK_SIZE_MEMO_SLOTS],
            sizes: [0; WALK_SIZE_MEMO_SLOTS],
        }
    }

    /// `object_total_size(header)`, memoised for compact objects.
    #[inline]
    pub fn size(&mut self, header: &ObjectHeader) -> usize {
        if !self.enabled
            || header.kind() == ObjectKind::Array
            || !cratonvm_types::is_compact_object(header)
        {
            return object_total_size(header);
        }
        let class_id = header.class_id.as_u32();
        // A SHORT (8-byte) compact header has no shape word: its `num_slots`
        // is itself a registry lookup, `compact_field_count(class_id)`, a
        // function of the class and the layout generation alone. Key it by
        // the class with the reserved field word `u32::MAX - 1`, without
        // asking. A long compact header keys by its shape word; the two
        // reserved words fall back to the plain lookup.
        let field_word = if header.is_short() {
            u32::MAX - 1
        } else {
            let n = header.num_slots();
            if n >= u32::MAX - 1 {
                return object_total_size(header);
            }
            n
        };
        let num_slots = field_word;
        let key = (u64::from(class_id) << 32) | u64::from(field_word);
        let generation = cratonvm_types::layout_generation();
        if generation != self.generation {
            self.keys = [u64::MAX; WALK_SIZE_MEMO_SLOTS];
            self.generation = generation;
        }
        let slot = ((class_id ^ num_slots.rotate_left(5)) as usize) & (WALK_SIZE_MEMO_SLOTS - 1);
        if self.keys[slot] == key {
            return self.sizes[slot];
        }
        let size = object_total_size(header);
        // A real size; the registry-miss sentinel is 1 TiB and is not kept.
        if size > 0 && size < (1usize << 32) {
            self.keys[slot] = key;
            self.sizes[slot] = size;
        }
        size
    }
}

/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_WALK_SIZE_MEMO` (default ON,
/// `=0` kills). See [`WalkSizeMemo`]. Read once per region walk.
pub fn g1_walk_size_memo_enabled() -> bool {
    true
}

/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_MARK_SIZE_MEMO` (OPT-IN, `=1`).
/// The concurrent marker's per-step [`WalkSizeMemo`] (see
/// `concurrent_mark_step_worker`); a separate switch from the cleanup walks'
/// so each A/Bs on its own. Read once per mark step.
///
/// Opt-in on its A/B (n23-w8g, pinned, 3 reps, `-XX:+UseG1GC -Xmx1g`): with
/// it ON, `R23W3Keep obj` ran 58.6/75.5/83.8 ns/op and `KeepLong 30`
/// 19.2/19.2/19.2 M ops; with `=0`, 65.1/70.3/72.3 ns/op and 19.2/19.2/16.8 M.
/// Neutral.
pub fn g1_mark_size_memo_enabled() -> bool {
    false
}

/// JIT round 23 wave 8, lane L9: the serial evacuator's per-pause knobs,
/// built once before a Cheney loop and handed by `&mut` to every
/// `evacuate_object_tuned` the loop makes (a stack local of the pause: no
/// thread-local, no global). [`SerialEvacKnobs::off`] is the plain
/// `evacuate_object` path.
///
/// Sampled on n23-w8g (`KeepLong 25 -XX:+UseG1GC -Xmx1g`, busiest thread, 18 s,
/// all inside serial evacuation): `evacuate_object` 59.7% inclusive; within it
/// the class-layout lookups -- `object_instance_size` 11.9% (the size) and
/// `ObjectHeader::num_slots` 8.5% (the two shape reads around the copy, each a
/// `compact_field_count` probe for a short header) -- are about a third.
pub struct SerialEvacKnobs {
    /// `CRATONVM_G1_SERIAL_EVAC_SIZE_MEMO`.
    pub sizes: WalkSizeMemo,
    /// `CRATONVM_G1_EVAC_SHAPE_KEY`.
    pub shape_key: bool,
    /// `CRATONVM_G1_REEVAC_PROBE_BEFORE_RETIRE_SKIP`: the loop these knobs serve
    /// runs before its pause retires any forward, so `evacuate_object_tuned`
    /// may skip the re-evacuation guard's `pointer_map` probe (see there).
    /// Only [`Self::new_before_retire`] sets it.
    pub before_retire: bool,
    /// `CRATONVM_G1_SERIAL_EVAC_SCREEN_ONCE` (JIT round 23 wave 9): a slot
    /// candidate the scan proved skips the supply screen's repeat
    /// `classify_candidate_header_view` (see `evacuate_object_tuned`).
    pub screen_once: bool,
    /// `CRATONVM_G1_EVAC_ONE_STORE_MARK`: the copy's mark word in one plain
    /// store (`destination_mark`).
    pub one_store_mark: bool,
}

impl SerialEvacKnobs {
    /// The switches as the environment sets them, read once per pause, for a
    /// Cheney loop that runs BEFORE its pause's `retire_forwards` (the young
    /// and mixed drivers' Phase 3). Never use it for a loop after a
    /// retirement.
    pub fn new_before_retire() -> Self {
        Self {
            sizes: WalkSizeMemo::with_enabled(g1_serial_evac_size_memo_enabled()),
            shape_key: true,
            before_retire: true,
            screen_once: g1_serial_evac_screen_once_enabled(),
            one_store_mark: g1_evac_one_store_mark_enabled(),
        }
    }

    /// All cuts off: exactly the pre-wave-8 `evacuate_object`.
    pub fn off() -> Self {
        Self {
            sizes: WalkSizeMemo::with_enabled(false),
            shape_key: false,
            before_retire: false,
            screen_once: false,
            one_store_mark: false,
        }
    }
}

/// JIT round 23 wave 9, lane L9: `CRATONVM_G1_SERIAL_EVAC_SCREEN_ONCE` (opt-in
/// `=1`: neutral on the n23-w9a A/B, keep-obj 72.5/68.6/71.4 ns/op on vs
/// 72.3/59.3/85.8 off). See `G1Collector::evacuate_object_tuned`. Read once per
/// pause.
pub fn g1_serial_evac_screen_once_enabled() -> bool {
    false
}



/// JIT round 23 wave 9, lane L9: `CRATONVM_G1_CLEANUP_PARALLEL_SCRUB` (OPT-IN,
/// `=1`). See `G1Collector::cleanup_scrub_regions_parallel`. Read once per
/// cleanup.
///
/// Opt-in on n23-w9c (`KeepLong 30 -Xmx1g --real-jdk`, `CRATONVM_GC_STATS=1`,
/// two runs each): it halves the cleanup pause, `cleanup_us` 127839 / 172058
/// against 250131 / 364638 at `=0`, but the row got worse: ops 16.8 / 19.2 M
/// against 21.6 / 21.6 M. Pinned A/B: keep-obj 78.2/65.9/69.4 against
/// 69.8/78.4/62.9 ns/op, `KeepLong` 19.2/21.6/19.2 M against 19.2 M x3.
pub fn g1_cleanup_parallel_scrub_enabled() -> bool {
    false
}



/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_SERIAL_FORWARD_PRESIZE`:
/// default ON since the round-23 in-session triage (R23-TRI-FLIP, `=0` kills;
/// with its two serial siblings, n23-w13m 5 pinned reps rotated: `R23W3Keep`
/// keep-obj median 76.8 vs 91.2 ns/op, `KeepLong 30` 19.2 vs 16.8 M ops). It was
/// (opt-in `=1`: mixed on the n23-w8h `KeepLong` A/B, keep-obj 92.7/60.3/83.7 ns/op
/// default vs 79.2/395.9/80.8 with it off). See
/// `G1Collector::serial_pointer_map`. Read once per pause.
pub fn g1_serial_forward_presize_enabled() -> bool {
    true
}

/// The largest forwarding map a serial pause pre-sizes for (entries): about
/// 17 MB of table (16 bytes an entry plus a control byte). A pause that copies
/// more still grows the map, from there (two or three doublings instead of
/// twenty); the cap bounds what a small pause after a huge one allocates.
pub const SERIAL_FORWARD_PRESIZE_CAP: usize = 1 << 20;

/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_SERIAL_EVAC_SIZE_MEMO` (default
/// ON, `=0` kills). The serial evacuator sizes each copy through the pause's
/// [`WalkSizeMemo`] (the same answer as `object_total_size`). Read once per
/// pause.
pub fn g1_serial_evac_size_memo_enabled() -> bool {
    true
}



/// The serial evacuator's copy-drift check key: `(class id, field word,
/// kind)`.
///
/// `keyed == false` is the original `(class_id, num_slots(), kind)`. With
/// `keyed`, a SHORT compact header contributes `u32::MAX` instead of
/// `num_slots()`: its field count is not stored in the object at all but
/// looked up as `compact_field_count(class_id)`, a function of the class id
/// already in the key (and of the layout generation, which no pause moves
/// between the two reads of one copy). So two keyed shapes are equal exactly
/// when the decoded shapes are, and the check skips two registry probes per
/// copied object. A long header (legacy instance, array) still reads its
/// stored field word. The check only feeds the `EVAC_COPY_SHAPE_DRIFT`
/// diagnostic; it never changes a copy.
#[inline]
pub fn evac_shape_key(header: &ObjectHeader, keyed: bool) -> (u32, u32, ObjectKind) {
    let slots = if keyed && header.is_short() {
        u32::MAX
    } else {
        header.num_slots()
    };
    (header.class_id.as_u32(), slots, header.kind())
}

/// Clear reference slots of a dead object without changing its walkable shape.
/// The caller supplies the completed mark or evacuation reachability proof.
pub unsafe fn clear_dead_object_reference_edges(
    addr: usize,
    header: &ObjectHeader,
    size: usize,
) -> bool {
    let mut nulled = false;
    if header.kind() == ObjectKind::Array {
        if header.element_type() == ArrayElementType::Reference {
            let len =
                (header.array_length() as usize).min(size.saturating_sub(ARRAY_DATA_OFFSET) / 8);
            // SAFETY: `len` elements lie inside the object's extent.
            unsafe { std::ptr::write_bytes((addr + ARRAY_DATA_OFFSET) as *mut u8, 0, len * 8) };
            nulled = len > 0;
        }
    } else {
        let max_slots = if cratonvm_types::is_compact_object(header) {
            header.num_slots() as usize
        } else {
            (header.num_slots() as usize).min(size.saturating_sub(HEADER_SIZE) / SLOT_SIZE)
        };
        for_each_flat_object_reference_capped(
            addr as *const u8,
            header,
            0,
            max_slots,
            |slot, _raw, compact| {
                // SAFETY: the walk hands out slots inside this
                // object's body.
                unsafe {
                    if compact {
                        std::ptr::write(slot as *mut u64, 0);
                    } else {
                        cratonvm_types::write_value_atomic(slot as *mut Value, Value::Object(None));
                    }
                }
                nulled = true;
            },
        );
    }
    nulled
}



/// gci i16/nc / i17/nr counters of the cleanup freed-target scrub and its
/// sibling screens, PER HEAP (gci i18/nr, gci i17/rv finding 11: they were
/// process statics, so two VMs in one process summed into one set). Read
/// through [`G1Collector::cleanup_scrub_counters`].
#[derive(Debug, Default)]
pub struct CleanupScrubCounters {
    /// gci i16/nc -- cleanups that ran the freed-target scrub.
    pub runs: AtomicU64,
    /// gci i16/nc -- source regions the freed-target scrub walked.
    pub sources: AtomicU64,
    /// gci i16/nc -- objects whose slots it nulled (one or more each).
    pub holders: AtomicU64,
    /// gci i16/nc -- reference slots it nulled.
    pub slots: AtomicU64,
    /// gci i16/nc -- source-region walks that stopped at an extent they could
    /// not trust (the rest of that region was not examined).
    pub walk_breaks: AtomicU64,
    /// gci i17/nr (R2) -- regions `cleanup` was about to free (in place, or as
    /// part of a dead humongous span) and KEPT for one more cycle because a
    /// source region that may name them could not be scrubbed to its end.
    pub retained: AtomicU64,
    /// gci i16/nc -- evacuation candidates (reference-slot supply route,
    /// `evacuation_candidate_is_an_object_view`) that passed every header
    /// screen but carry no `GC_FLAG_HEADER`. Expected ZERO on a sound heap.
    pub evac_candidate_flagless: AtomicU64,
    /// gci i17/nr (R1) -- reference slots the remembered-set source walk
    /// nulled in holders it skipped as proven dead (`MarkProvenDead`).
    pub rset_dead_holder_slots_nulled: AtomicU64,
    /// gci i18/nr (vi17-nr R1 fallout) -- holders the freed-target scrub
    /// nulled although their mark bit is set (an under-mark lead; expected 0).
    pub marked_holders: AtomicU64,
    /// gci i18/nr (vi17-nr R1 fallout) -- cleanups that took the
    /// retain-everything fail-safe (`saw_implausible` or an undrained gray
    /// set): no in-place free, no humongous reclaim, no scrub.
    pub retain_all_cycles: AtomicU64,
    /// gci i30/hz -- flagless evacuation candidates (the
    /// `evac_candidate_flagless` population) REFUSED because they lie inside
    /// the extent of the object before them ([`flagless_candidate_cover`]):
    /// interior addresses whose evacuation would have installed a forwarding
    /// state in that object's body. Expected ZERO on a sound heap.
    pub evac_candidate_flagless_covered_refused: AtomicU64,
    /// gci i39/cv -- free events `CRATONVM_G1_DBG_FREED_SLOT_AUDIT` audited.
    pub freed_slot_audits: AtomicU64,
    /// gci i39/cv -- reference slots that named a region the free just reset
    /// (the producer of gci-i29cv2). Expected ZERO on a sound heap.
    pub freed_slot_survivors: AtomicU64,
    /// gci i39/cv -- `[g1][FREED-SLOT]` detail lines printed (the per-heap cap).
    pub freed_slot_lines: AtomicU64,
}

/// `CRATONVM_G1_EVAC_CANDIDATE_REQUIRES_HEADER_FLAG=1` -- refuse such a
/// candidate (leave the slot as it is) instead of following it. Default OFF
/// (`runtime_flag_on`); the counter above runs either way.
pub fn g1_evac_candidate_requires_header_flag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        false
    })
}



/// How far back, in 8-byte words, [`flagless_candidate_cover`] looks for the
/// header of the object that may contain a flagless candidate.
pub const FLAGLESS_COVER_SCAN_WORDS: usize = 32;

/// gci i30/hz -- where a flagless candidate sits relative to the nearest
/// published header before it ([`flagless_candidate_cover`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlaglessCover {
    /// The header at `start` (class `class_id`, `size` bytes) extends past the
    /// candidate: the candidate is an INTERIOR address of that object.
    Covered {
        start: usize,
        class_id: u32,
        size: usize,
    },
    /// The nearest header before the candidate ends at or before it: the
    /// candidate may be an object start whose own header lost its flag.
    NotCovered {
        start: usize,
        class_id: u32,
        size: usize,
    },
    /// No verdict (region base reached, no header in the window, or a
    /// forwarded long header whose size word is the forwarding target).
    Undecided(&'static str),
}

impl FlaglessCover {
    pub fn is_covered(&self) -> bool {
        matches!(self, FlaglessCover::Covered { .. })
    }

    /// One `cover=...` field for the candidate at `raw`.
    pub fn describe(&self, raw: usize) -> String {
        match *self {
            FlaglessCover::Covered {
                start,
                class_id,
                size,
            } => format!(
                "cover=INTERIOR of=0x{start:x} delta=0x{:x} size=0x{size:x} cid={class_id}",
                raw.wrapping_sub(start)
            ),
            FlaglessCover::NotCovered {
                start,
                class_id,
                size,
            } => format!(
                "cover=NOT-COVERED prev=0x{start:x} prev_size=0x{size:x} prev_cid={class_id}"
            ),
            FlaglessCover::Undecided(why) => format!("cover=UNDECIDED({why})"),
        }
    }
}

/// gci i30/hz -- is the flagless candidate `raw` inside the extent of the
/// object before it? Scans back at most [`FLAGLESS_COVER_SCAN_WORDS`] words
/// (never below `base`) for the NEAREST word that reads as a published header
/// (`GC_FLAG_HEADER`, sound kind and element tags, a size of at least
/// `MIN_OBJECT_SIZE` that ends inside `[base, base + cursor)`), and answers
/// whether that object's extent passes `raw`.
///
/// Why it exists (gci-i29cv2): r19's "live object with a zeroed first header
/// word" reads, word for word, as the bytes at `HashMap + 0x20` of a compact
/// `java.util.HashMap` (keySet@8 values@16 table@24 entrySet@32, then size,
/// modCount, threshold, loadFactor@40..56): `entrySet == null` is the
/// "header" (class 0, flags 0), `0x00000001_00000001` is size 1 / modCount 1,
/// `0x3f400000_0000000c` is threshold 12 / loadFactor 0.75f, and `raw1` at
/// +0x18 is the header of the object after the 0x38-byte map. A slot naming
/// that address is an interior pointer, not an object whose header was lost.
///
/// Errs toward NOT covered: a body word that mimics a header with a short
/// extent makes the answer `NotCovered`, never a refusal.
///
/// # Safety
/// `[base, base + cursor)` is a region's allocated prefix (mapped for the
/// collector's lifetime); racy reads of it are tolerated (a pause, or a
/// parallel worker whose peers only CAS mark words and write forwarding
/// targets into objects they have claimed).
pub unsafe fn flagless_candidate_cover(base: usize, cursor: usize, raw: usize) -> FlaglessCover {
    let Some(end) = base.checked_add(cursor) else {
        return FlaglessCover::Undecided("prefix-overflow");
    };
    if raw < base || raw >= end || raw & 0x7 != 0 {
        return FlaglessCover::Undecided("outside-prefix");
    }
    for k in 1..=FLAGLESS_COVER_SCAN_WORDS {
        let Some(h) = raw.checked_sub(8 * k) else {
            break;
        };
        if h < base {
            return FlaglessCover::Undecided("region-base");
        }
        let ptr = h as *const u8;
        // SAFETY: `h` is 8-aligned inside `[base, raw)`, and `h + 16 <= raw + 8
        // <= end` because `raw` is 8-aligned below `end`.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        if header.gc_flags() & cratonvm_types::GC_FLAG_HEADER == 0 {
            continue;
        }
        // SAFETY: as above, the tag bytes lie inside the header just read.
        let kind_ok = unsafe { object_kind_from_tag(cratonvm_types::kind_tag_at(ptr)) }.is_some();
        // SAFETY: as above.
        let elem_ok = unsafe { cratonvm_types::element_type_tag_ok_at(ptr) };
        if !kind_ok || !elem_ok {
            continue;
        }
        // A forwarded LONG header's second word is the forwarding target, not
        // its shape, so its size is unknown here. A compact instance sizes
        // from its class layout and keeps its class id and flags across the
        // forward, so it is still decidable.
        if header.is_forwarded() && !cratonvm_types::is_compact_object(header) {
            return FlaglessCover::Undecided("prev-forwarded-long-header");
        }
        let size = object_total_size(header);
        if size < cratonvm_types::MIN_OBJECT_SIZE || size > end - h {
            continue;
        }
        let class_id = header.class_id.as_u32();
        return if h + size > raw {
            FlaglessCover::Covered {
                start: h,
                class_id,
                size,
            }
        } else {
            FlaglessCover::NotCovered {
                start: h,
                class_id,
                size,
            }
        };
    }
    FlaglessCover::Undecided("no-header-in-window")
}

/// gci i17/nr (R2) -- how many keep-and-rewalk rounds one `cleanup` runs before
/// it keeps every candidate (see the freed-target scrub in `cleanup`).
pub const CLEANUP_RETAIN_ROUNDS: usize = 4;

/// gci i16/nc -- the regions one `cleanup` frees, and the remembered-set
/// sources that named them, captured BEFORE `G1Region::reset` clears each
/// freed region's rset.
#[derive(Default)]
pub struct CleanupFreedTargets {
    /// Regions this cleanup freed: in place (Old, zero live) or as part of a
    /// reclaimed dead humongous span.
    pub freed: RegionSet,
    /// Every source region a freed region's remembered set named.
    pub sources: RegionSet,
    /// A freed region's rset was coarsened ("any region may point into me"):
    /// every region is a source.
    pub coarsened: bool,
}

impl CleanupFreedTargets {
    /// Record `idx` as freed and take its rset's sources. Call BEFORE the
    /// region's `reset`.
    pub fn note(&mut self, idx: usize, rset: &RememberedSet) {
        self.freed.insert(idx);
        if rset.is_coarsened() {
            self.coarsened = true;
        }
        for s in rset.sources() {
            self.sources.insert(s);
        }
    }
}

/// gci i16/nc -- what [`scrub_slots_into_freed_regions`] did.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreedTargetScrub {
    /// Source regions offered to the walk (since gci i18/nr a region with no
    /// dirty card is counted here but returns before walking).
    pub sources: usize,
    /// Objects whose slots were examined (card-screened objects excluded).
    pub objects: usize,
    /// Their bytes.
    pub bytes: usize,
    /// Objects with at least one slot nulled.
    pub holders: usize,
    /// Slots nulled.
    pub slots: usize,
    /// Walks that stopped at an untrusted extent.
    pub breaks: usize,
    /// gci i18/nr (vi17-nr R1 fallout, diagnostic): holders with a nulled
    /// slot whose mark bit is SET. The marker reached such a holder and
    /// marked what its slots named at scan time; SATB lets a later store
    /// install only a referent reachable at the snapshot (marked) or newer
    /// (above TAMS, so its region is never freed in place). A marked holder
    /// naming a region cleanup frees is therefore evidence of an under-mark:
    /// the scrub turned a would-be use-after-free into a null, i.e. a wrong
    /// Java result (vi17-nr `nr17_jt_np_r5`). Expected 0.
    pub marked_holders: usize,
}

impl FreedTargetScrub {
    pub fn add(&mut self, o: FreedTargetScrub) {
        self.sources += o.sources;
        self.objects += o.objects;
        self.bytes += o.bytes;
        self.holders += o.holders;
        self.slots += o.slots;
        self.breaks += o.breaks;
        self.marked_holders += o.marked_holders;
    }
}

/// JIT round 23 wave 3, lane L4: young regions re-walked by
/// [`G1Collector::record_excluded_young_sources`] (a counter, read by tests and
/// the census).
pub static G1_YOUNG_SOURCES_REWALKED: AtomicU64 = AtomicU64::new(0);

/// JIT round 23 wave 3, lane L4: the object starts of `region`'s allocated
/// prefix, parsed exactly as [`scrub_slots_into_freed_regions`] parses a
/// source region (JIT TLAB skip spans, gap fillers, TLAB fillers skipped; a
/// humongous filler or an implausible size ends the walk -- the source walks
/// that consume what is recorded stop at the same place).
///
/// # Safety
/// Stop-the-world; the caller holds the regions guard `region` was borrowed
/// from.
pub unsafe fn young_region_object_starts(region: &G1Region, jit_skips: &[(usize, usize)]) -> Vec<usize> {
    let base = region.data.as_ptr() as usize;
    let limit = region.cursor();
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < limit {
        let addr = base + offset;
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, addr) {
            offset += skip;
            continue;
        }
        if let Some(gap) = gap_filler_len(addr as *const u8) {
            offset += gap;
            continue;
        }
        // SAFETY: `offset < limit <= cursor`, inside the allocated prefix.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if is_humongous_filler(header) {
            break;
        }
        let size = object_total_size(header);
        if size < cratonvm_types::MIN_OBJECT_SIZE || offset + size > limit {
            break;
        }
        if !is_tlab_filler_header(header) {
            out.push(addr);
        }
        offset += size;
    }
    out
}

/// Null every reference slot of every object in `region` (below `limit`)
/// that names an address inside a region of `freed` -- the regions this
/// `cleanup` is about to free (since gci i17/nr the scrub runs before their
/// reset, so `cleanup` can still keep one whose sources it could not finish).
///
/// # Why (gci i16/nc, the vi7-g1y net-of-pins wrong result and crash)
///
/// [`scrub_dead_refs_below_tams`] nulls the slots of dead objects the bitmap
/// can prove dead. It cannot see a dead object at or above TAMS, and the
/// collector makes those itself: the remembered-set source walk is
/// REGION-granular, so a young pause during the cycle walks a dead Old
/// holder's slots and copies whatever young object they name -- an object
/// that was already unreachable at the snapshot -- into a Survivor or Old
/// region above TAMS (or into a region retyped during the cycle, TAMS 0).
/// Its slots still name the dead Old objects it named, cleanup frees their
/// regions in place, the mutator reuses those regions straight away, and the
/// FIRST pause after cleanup walks the copy wholesale (its region is a source
/// of every young region) and follows the slot into the middle of whatever was
/// allocated there. The header screens accept body words as a header (an
/// `int[]` of ascending positions reads as class id ~5800 with ~5800 legacy
/// slots and no `GC_FLAG_HEADER`), `evacuate` installs a forwarding mark word
/// inside the live object and copies its body, and the Cheney scan of the copy
/// reports the `g1::for_each_object_reference: corrupt Value cell` burst.
/// Every np-arm javac row of vi7-g1y (i10a, i13) has that burst, each one in
/// the pause right after a `g1-remark` (cleanup); the default rows never run a
/// cycle and have none. HotSpot G1 never meets the shape: its concurrent-start
/// pause evacuates the whole young generation and root-region scanning marks
/// through every survivor, so nothing young at the snapshot points at an
/// unmarked Old object, and it scans dirty CARDS, not whole source regions.
///
/// # Why nulling is sound
///
/// A region is freed by cleanup only when it holds no marked object below TAMS
/// and nothing above it (`live_bytes == 0`), or as a humongous span whose start
/// is unmarked. Under SATB nothing reachable at the snapshot lives there, and
/// nothing allocated after it either, so no reachable object can hold a slot
/// into it: every slot that does is a dead object's (or the freed region was
/// already a use-after-free, which a null turns into a null). Only reference
/// slots are written; headers, sizes and primitives stay.
///
/// # Which regions
///
/// Every producer of a cross-region slot records the holder's region in the
/// target region's remembered set (the mutator barrier, and the Phase-4
/// rebuild for every object a pause copies), so the freed regions' rsets,
/// captured before `reset` cleared them, name every region that can hold
/// such a slot; a coarsened rset widens the walk to every region. The
/// per-object card screen is Phase 2's (`CRATONVM_G1_CARD_RSET`), on the same
/// premise: every producer dirties the holder's start card. A JNI-pinned
/// region is walked without it.
///
/// # Safety
/// Stop-the-world cleanup, regions write guard held; `limit <= region.cursor()`
/// (a `HumongousStart`'s cursor is its object's whole size, which the arena
/// lays out contiguously over its continuations).
pub unsafe fn scrub_slots_into_freed_regions(
    gc: &G1Collector,
    region: &G1Region,
    limit: usize,
    freed: &RegionSet,
    jit_skips: &[(usize, usize)],
    card_screen: bool,
) -> FreedTargetScrub {
    let mut out = FreedTargetScrub {
        sources: 1,
        ..FreedTargetScrub::default()
    };
    let base = region.data.as_ptr() as usize;
    let cards = gc.cards();
    let card_screen = card_screen && !region.pinned;
    // gci i18/nr (i17/rv finding 3): no dirty card anywhere in `[base, base +
    // limit)` -> every per-object screen below would skip its object (each
    // object's extent lies inside that range), so nothing would be nulled:
    // return before the linear walk. One summary-indexed query instead of an
    // O(objects) step per clean source region -- the whole heap when one rset
    // is coarsened. Also no walk break is reported for such a region: an
    // object behind a break starts inside the same clean range, so on the
    // per-object screen's own premise (every producer of a cross-region slot
    // dirties its holder's start card; cards go clean only at `reset`, or
    // under the opt-in card-cleaning contract) it holds no slot into a freed
    // region, and `cleanup` need not keep a candidate for it.
    if card_screen && !cards.any_dirty_in(base, limit) {
        return out;
    }
    let names_freed =
        |raw: usize| raw != 0 && gc.lookup_region_for_addr(raw).is_some_and(|i| freed.contains(&i));
    let mut offset = 0usize;
    // JIT round 23 wave 8, lane L9: `CRATONVM_G1_WALK_SIZE_MEMO`.
    let mut sizes = WalkSizeMemo::new();
    while offset < limit {
        let addr = base + offset;
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, addr) {
            offset += skip;
            continue;
        }
        if let Some(gap) = gap_filler_len(addr as *const u8) {
            offset += gap;
            continue;
        }
        // SAFETY: `offset < limit <= cursor`, inside the allocated prefix.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if is_humongous_filler(header) {
            break;
        }
        let size = sizes.size(header);
        if size < cratonvm_types::MIN_OBJECT_SIZE || offset + size > limit {
            out.breaks += 1;
            break;
        }
        if is_tlab_filler_header(header) || (card_screen && !cards.any_dirty_in(addr, size)) {
            offset += size;
            continue;
        }
        out.objects += 1;
        out.bytes += size;
        // SAFETY: `[addr, addr + size)` is one object inside the prefix.
        let n = unsafe { null_reference_slots_naming(addr, header, size, &names_freed) };
        if n > 0 {
            out.holders += 1;
            out.slots += n;
            if region.mark_bitmap.is_marked(addr) {
                out.marked_holders += 1;
            }
        }
        offset += size;
    }
    out
}

/// Null the reference slots of the object at `addr` whose value `names`
/// accepts; answers how many it nulled. Same slot walk and the same writes
/// as [`clear_dead_object_reference_edges`], filtered by value.
///
/// # Safety
/// `[addr, addr + size)` is one object whose header is `header`; stop-the-world.
pub unsafe fn null_reference_slots_naming(
    addr: usize,
    header: &ObjectHeader,
    size: usize,
    names: &impl Fn(usize) -> bool,
) -> usize {
    let mut nulled = 0usize;
    if header.kind() == ObjectKind::Array {
        if header.element_type() == ArrayElementType::Reference {
            let len =
                (header.array_length() as usize).min(size.saturating_sub(ARRAY_DATA_OFFSET) / 8);
            for i in 0..len {
                let slot = (addr + ARRAY_DATA_OFFSET + i * 8) as *mut u64;
                // SAFETY: element `i < len` lies inside the object's extent.
                let raw = unsafe { std::ptr::read(slot) } as usize;
                if names(raw) {
                    // SAFETY: as above.
                    unsafe { std::ptr::write(slot, 0) };
                    nulled += 1;
                }
            }
        }
    } else {
        let max_slots = if cratonvm_types::is_compact_object(header) {
            header.num_slots() as usize
        } else {
            (header.num_slots() as usize).min(size.saturating_sub(HEADER_SIZE) / SLOT_SIZE)
        };
        for_each_flat_object_reference_capped(
            addr as *const u8,
            header,
            0,
            max_slots,
            |slot, raw, compact| {
                if !names(raw) {
                    return;
                }
                // SAFETY: the walk hands out slots inside this object's body.
                unsafe {
                    if compact {
                        std::ptr::write(slot as *mut u64, 0);
                    } else {
                        cratonvm_types::write_value_atomic(slot as *mut Value, Value::Object(None));
                    }
                }
                nulled += 1;
            },
        );
    }
    nulled
}

/// gci i39/cv (page `gci-i29cv2-g1-live-object-with-a-zeroed-first-header-word-RETIRED-UNFIXED-20261018`,
/// the producer of the `HashMap + 0x20` holder slot) -- `CRATONVM_G1_DBG_FREED_SLOT_AUDIT=1`
/// (default OFF, `runtime_flag_on`, read once per free event: no `OnceLock`,
/// no process static).
///
/// # What it answers
///
/// gci i30/hz narrowed the page to "a holder slot outlived the free of the
/// region its target lived in, and the region was then reused". The shape the
/// page records is that slot met AFTER the reuse, when the address has become
/// the inside of a promoted map, i.e. one free and an unknown number of pauses
/// after the producer. It occurred in 1 of 20 javac rows on i26a and in 0 of 42
/// on i30a, so waiting for the shape does not decide anything.
///
/// The invariant the design rests on is stronger and checkable at the moment
/// it breaks: after every free, NO reference slot of any object in a surviving
/// region names a region that free just reset. Every path is meant to keep it
/// -- the evacuation rewrites (Phase 2 / 3 / 4) or, for an unreached target,
/// the region-granular source walk would have reached it; the kept-region
/// dead-edge clearing (`free_or_keep_cset`); the MarkProvenDead nulling of a
/// skipped holder; cleanup's below-TAMS scrub and freed-target scrub. A slot
/// that survives a free is therefore the producer itself, caught in the event
/// that made it, whether or not its region is later reused at an address that
/// lands inside another object.
///
/// After each audited free (`free_or_keep_cset`: every young, mixed and drain
/// pass; `cleanup`: the in-place Old frees and the dead humongous spans) it
/// walks every surviving region linearly (the same walk discipline as
/// [`scrub_slots_into_freed_regions`]) and prints one
/// `[g1][FREED-SLOT] event=..` line per surviving slot (at most
/// [`FREED_SLOT_LINES_PER_EVENT`] per event and [`FREED_SLOT_LINES_TOTAL`] per
/// heap) and one `[g1][FREED-SLOT] summary ..` line per event, with `why=`
/// splitting the producer ([`FreedSlotWhy`]). O(heap) per pause: a diagnostic,
/// never a default. Not covered: the eager humongous reclaim after Phase 5 (the
/// page's address is not a region base, so its target was not a humongous
/// start).
pub fn g1_dbg_freed_slot_audit() -> bool {
    false}

/// gci i39/cv -- per-event cap on `[g1][FREED-SLOT]` detail lines.
pub const FREED_SLOT_LINES_PER_EVENT: u64 = 32;
/// gci i39/cv -- per-heap cap on `[g1][FREED-SLOT]` detail lines (the summary
/// line of every event is printed regardless).
pub const FREED_SLOT_LINES_TOTAL: u64 = 4096;

/// gci i39/cv -- what the freed region's remembered set said about the
/// holder's region before the free reset it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreedSlotRset {
    /// The holder's region is a source, recorded in a generation its region
    /// has not been recycled since.
    Named,
    /// The rset was coarsened (every region is a nominal source).
    Coarsened,
    /// The holder's region is not a source at all.
    Missing,
    /// It is a source, but recorded BEFORE the holder's region was last
    /// recycled, so `live_rset_sources` drops it as dead (`rset_entry_is_stale`).
    Stale,
    /// No snapshot was taken for the target region (e.g. a humongous
    /// continuation, whose edges are recorded on the span start).
    NoSnapshot,
}

impl FreedSlotRset {
    pub fn as_str(self) -> &'static str {
        match self {
            FreedSlotRset::Named => "yes",
            FreedSlotRset::Coarsened => "coarsened",
            FreedSlotRset::Missing => "no",
            FreedSlotRset::Stale => "stale",
            FreedSlotRset::NoSnapshot => "no-snapshot",
        }
    }
}

/// gci i39/cv -- the pre-registered reading of one surviving slot. The
/// variants are tested in this order (first match wins).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreedSlotWhy {
    /// `cleanup` only: the holder lies below its region's TAMS and is MARKED --
    /// a live holder names a region cleanup freed: an under-mark.
    MarkedHolder = 0,
    /// Evacuation only: the target WAS copied (a forwarding entry to another
    /// address exists) and this slot was not rewritten -- the fix-up did not
    /// walk or did not resolve the holder (`phase4_regions_to_walk`).
    FixupMiss = 1,
    /// The freed region's rset did not name the holder's region: no producer
    /// recorded the edge (a store or copy that skipped the barrier / rebuild).
    RsetHole = 2,
    /// The rset named it only with an entry older than the holder region's
    /// last recycle, which the scans drop as dead.
    RsetStale = 3,
    /// The rset named the holder's region but no card of the holder is dirty:
    /// the card screens step over it.
    CardHole = 4,
    /// Named and dirty, yet neither rewritten nor nulled: the walk that should
    /// have handled it skipped it (a refused candidate, a walk break, a
    /// filtered holder, a region the walk did not take).
    WalkSkip = 5,
}

/// gci i39/cv -- number of [`FreedSlotWhy`] variants.
pub const FREED_SLOT_WHY_COUNT: usize = 6;

impl FreedSlotWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            FreedSlotWhy::MarkedHolder => "marked-holder",
            FreedSlotWhy::FixupMiss => "fixup-miss",
            FreedSlotWhy::RsetHole => "rset-hole",
            FreedSlotWhy::RsetStale => "rset-stale",
            FreedSlotWhy::CardHole => "card-hole",
            FreedSlotWhy::WalkSkip => "walk-skip",
        }
    }

    pub fn classify(
        cleanup_marked_holder: bool,
        forwarded_elsewhere: bool,
        rset: FreedSlotRset,
        card_dirty: bool,
    ) -> Self {
        if cleanup_marked_holder {
            return FreedSlotWhy::MarkedHolder;
        }
        if forwarded_elsewhere {
            return FreedSlotWhy::FixupMiss;
        }
        match rset {
            FreedSlotRset::Missing => FreedSlotWhy::RsetHole,
            FreedSlotRset::Stale => FreedSlotWhy::RsetStale,
            FreedSlotRset::Named | FreedSlotRset::Coarsened | FreedSlotRset::NoSnapshot => {
                if card_dirty {
                    FreedSlotWhy::WalkSkip
                } else {
                    FreedSlotWhy::CardHole
                }
            }
        }
    }
}

/// gci i39/cv -- the state [`freed_slot_audit`] compares against, taken
/// BEFORE the free: every region's type and incarnation, and the remembered
/// set (sources with their generations) of every region the free may reset
/// (the reset clears it).
pub struct FreedSlotAuditPre {
    pub before: Vec<(RegionType, u64)>,
    pub rsets: rustc_hash::FxHashMap<usize, (bool, Vec<(usize, u64)>)>,
    pub cset: RegionSet,
}

impl FreedSlotAuditPre {
    pub fn take(
        regions: &[G1Region],
        candidates: impl IntoIterator<Item = usize>,
        cset: &[usize],
    ) -> Self {
        let before = regions
            .iter()
            .map(|r| (r.region_type(), r.reuse_epoch))
            .collect();
        let mut rsets = rustc_hash::FxHashMap::default();
        for i in candidates {
            if let Some(r) = regions.get(i) {
                let coarsened = r.rset.is_coarsened();
                let sources = if coarsened {
                    Vec::new()
                } else {
                    r.rset.sources_with_generations()
                };
                rsets.insert(i, (coarsened, sources));
            }
        }
        FreedSlotAuditPre {
            before,
            rsets,
            cset: cset.iter().copied().collect(),
        }
    }
}

/// gci i39/cv -- one audit's census (also what the unit tests read).
#[derive(Debug, Default)]
pub struct FreedSlotAuditOut {
    pub freed_regions: usize,
    pub regions_walked: usize,
    pub objects: usize,
    pub survivors: usize,
    pub walk_breaks: usize,
    pub by_why: [usize; FREED_SLOT_WHY_COUNT],
}

/// gci i39/cv -- the audit itself; see [`g1_dbg_freed_slot_audit`]. `event`
/// is `"evac"` (from `free_or_keep_cset`, with that pass's `pointer_map`) or
/// `"cleanup"`. Reads only: it writes no slot, card, rset or mark bit, so the
/// run it watches takes the same path as without it (bar the time it costs).
///
/// Called with the regions guard held (stop-the-world), after the free.
pub fn freed_slot_audit(
    gc: &G1Collector,
    regions: &[G1Region],
    pre: &FreedSlotAuditPre,
    event: &'static str,
    pointer_map: Option<&cratonvm_types::PointerMap>,
) -> FreedSlotAuditOut {
    let mut out = FreedSlotAuditOut::default();
    if pre.before.len() != regions.len() {
        return out;
    }
    let mut freed = RegionSet::with_region_capacity(regions.len());
    let (mut freed_young, mut freed_old, mut freed_humongous) = (0usize, 0usize, 0usize);
    for (i, r) in regions.iter().enumerate() {
        let (was_type, was_epoch) = pre.before[i];
        if was_type != RegionType::Free
            && (r.region_type() == RegionType::Free || r.reuse_epoch != was_epoch)
        {
            freed.insert(i);
            match was_type {
                RegionType::Eden | RegionType::Survivor => freed_young += 1,
                RegionType::Old => freed_old += 1,
                _ => freed_humongous += 1,
            }
        }
    }
    out.freed_regions = freed.len();
    if freed.is_empty() {
        return out;
    }
    let cleanup_event = event == "cleanup";
    let jit_skips = gc.jit_tlab_skip_spans();
    let cards = gc.cards();
    let gc_no = gc.collection_count.load(Ordering::Relaxed);
    let marking = gc.gc_state.is_marking_active();
    let counters = &gc.scrub_counters;
    let mut lines_here = 0u64;
    let names_freed = |raw: usize| {
        raw != 0 && gc.lookup_region_for_addr(raw).is_some_and(|i| freed.contains(&i))
    };
    for (idx, region) in regions.iter().enumerate() {
        if freed.contains(&idx)
            || matches!(
                region.region_type(),
                RegionType::Free | RegionType::HumongousContinuation
            )
        {
            continue;
        }
        out.regions_walked += 1;
        let base = region.data.as_ptr() as usize;
        let limit = region.cursor();
        let tams = region.tams();
        // Edges from a humongous holder are recorded under its span start.
        let holder_src = humongous_span_start(regions, idx).unwrap_or(idx);
        let holder_recycled = regions[holder_src].recycled_in_generation();
        let mut offset = 0usize;
        while offset < limit {
            let addr = base + offset;
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, addr) {
                offset += skip;
                continue;
            }
            if let Some(gap) = gap_filler_len(addr as *const u8) {
                offset += gap;
                continue;
            }
            // SAFETY: `offset < limit <= cursor`, inside the allocated prefix;
            // stop-the-world, the caller holds the regions guard.
            let header = unsafe { &*(addr as *const ObjectHeader) };
            if is_humongous_filler(header) {
                break;
            }
            let size = object_total_size(header);
            if size < cratonvm_types::MIN_OBJECT_SIZE || offset + size > limit {
                out.walk_breaks += 1;
                break;
            }
            if is_tlab_filler_header(header) {
                offset += size;
                continue;
            }
            out.objects += 1;
            let mut hits: Vec<(usize, usize)> = Vec::new();
            if header.kind() == ObjectKind::Array {
                if header.element_type() == ArrayElementType::Reference {
                    let len = (header.array_length() as usize)
                        .min(size.saturating_sub(ARRAY_DATA_OFFSET) / 8);
                    for i in 0..len {
                        let slot = addr + ARRAY_DATA_OFFSET + i * 8;
                        // SAFETY: element `i < len` lies inside the object.
                        let raw = unsafe { std::ptr::read(slot as *const u64) } as usize;
                        if names_freed(raw) {
                            hits.push((slot, raw));
                        }
                    }
                }
            } else {
                let max_slots = if cratonvm_types::is_compact_object(header) {
                    header.num_slots() as usize
                } else {
                    (header.num_slots() as usize).min(size.saturating_sub(HEADER_SIZE) / SLOT_SIZE)
                };
                for_each_flat_object_reference_capped(
                    addr as *const u8,
                    header,
                    0,
                    max_slots,
                    |slot, raw, _compact| {
                        if names_freed(raw) {
                            hits.push((slot as usize, raw));
                        }
                    },
                );
            }
            for (slot, raw) in hits {
                out.survivors += 1;
                let t_idx = gc.lookup_region_for_addr(raw).unwrap_or(usize::MAX);
                let rset = match pre.rsets.get(&t_idx) {
                    None => FreedSlotRset::NoSnapshot,
                    Some((true, _)) => FreedSlotRset::Coarsened,
                    Some((false, sources)) => {
                        match sources.iter().find(|(s, _)| *s == holder_src) {
                            None => FreedSlotRset::Missing,
                            Some(&(_, generation)) if generation < holder_recycled => {
                                FreedSlotRset::Stale
                            }
                            Some(_) => FreedSlotRset::Named,
                        }
                    }
                };
                let forwarded = pointer_map.and_then(|m| m.get(&raw).copied());
                let card_dirty = cards.any_dirty_in(addr, size);
                let marked = region.mark_bitmap.is_marked(addr);
                let below_tams = offset < tams;
                let why = FreedSlotWhy::classify(
                    cleanup_event && marked && below_tams,
                    forwarded.is_some_and(|to| to != raw),
                    rset,
                    card_dirty,
                );
                out.by_why[why as usize] += 1;
                if lines_here >= FREED_SLOT_LINES_PER_EVENT {
                    continue;
                }
                let total = counters.freed_slot_lines.fetch_add(1, Ordering::Relaxed) + 1;
                if total > FREED_SLOT_LINES_TOTAL {
                    continue;
                }
                lines_here += 1;
                let forwarded_desc = match forwarded {
                    None => "none".to_string(),
                    Some(to) if to == raw => "self".to_string(),
                    Some(to) => format!("to=0x{to:x}"),
                };
                let (t_was, t_epoch_before, t_off) =
                    match (pre.before.get(t_idx), regions.get(t_idx)) {
                        (Some(&(ty, ep)), Some(t)) => {
                            (ty, ep, raw.wrapping_sub(t.data.as_ptr() as usize))
                        }
                        _ => (RegionType::Free, 0, 0),
                    };
                eprintln!(
                    "[g1][FREED-SLOT] event={event} gc={gc_no} marking={marking} why={} \
                     holder=0x{addr:x} holder_at=r{idx}/{:?}/off=0x{offset:x}/epoch={}/\
                     recycled_gen={}/marked={marked}/below_tams={below_tams}/pinned={}/\
                     in_cset={} holder_cid={} holder_kind={} holder_size=0x{size:x} \
                     slot_off=0x{:x} value=0x{raw:x} target_at=r{t_idx}/was={t_was:?}/\
                     off=0x{t_off:x}/epoch_before={t_epoch_before} in_rset={} card={} \
                     forwarded={forwarded_desc}",
                    why.as_str(),
                    region.region_type(),
                    region.reuse_epoch,
                    region.recycled_in_generation(),
                    region.pinned,
                    pre.cset.contains(&idx),
                    header.class_id.as_u32(),
                    if header.kind() == ObjectKind::Array { "array" } else { "object" },
                    slot.wrapping_sub(addr),
                    rset.as_str(),
                    if card_dirty { "dirty" } else { "clean" },
                );
            }
            offset += size;
        }
    }
    counters.freed_slot_audits.fetch_add(1, Ordering::Relaxed);
    let survivors_total = counters
        .freed_slot_survivors
        .fetch_add(out.survivors as u64, Ordering::Relaxed)
        + out.survivors as u64;
    let w = &out.by_why;
    eprintln!(
        "[g1][FREED-SLOT] summary event={event} gc={gc_no} marking={marking} \
         freed_regions={} freed_young={freed_young} freed_old={freed_old} \
         freed_humongous={freed_humongous} regions_walked={} objects={} survivors={} \
         marked_holder={} fixup_miss={} rset_hole={} rset_stale={} card_hole={} walk_skip={} \
         walk_breaks={} survivors_total={survivors_total}",
        out.freed_regions,
        out.regions_walked,
        out.objects,
        out.survivors,
        w[FreedSlotWhy::MarkedHolder as usize],
        w[FreedSlotWhy::FixupMiss as usize],
        w[FreedSlotWhy::RsetHole as usize],
        w[FreedSlotWhy::RsetStale as usize],
        w[FreedSlotWhy::CardHole as usize],
        w[FreedSlotWhy::WalkSkip as usize],
        out.walk_breaks,
    );
    out
}

/// F-06 — the walk `cleanup` used to do: sum the sizes of marked objects below
/// `tams`.
///
/// No longer on the pause path by default. It is the `CRATONVM_G1_CLEANUP_WALK`
/// arm and the debug-build oracle for the accumulator — see the call site.
pub fn walk_marked_bytes_below_tams(
    region: &G1Region,
    base: usize,
    tams: usize,
    jit_skips: &[(usize, usize)],
) -> (usize, bool) {
    let mut live = 0usize;
    let mut offset = 0usize;
    // Did the walk reach `tams`, or did it stop at an object it could not size?
    // The caller's assertion depends on the difference: a broken walk is
    // legitimately SMALLER than the accumulator, a complete one must match.
    let mut complete = true;
    while offset < tams {
        let obj_addr = base + offset;
        // INT-3 — frozen-peer TLAB tail: skip before interpreting.
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, obj_addr) {
            offset += skip;
            continue;
        }
        // TLAB-retire gap sentinel: skip its exact span.
        if let Some(gap) = gap_filler_len(obj_addr as *const u8) {
            offset += gap;
            continue;
        }
        // SAFETY: `offset < tams <= cursor`, so this is inside the region's
        // live extent and the regions guard is held by the caller.
        let header = unsafe { &*(obj_addr as *const ObjectHeader) };
        // Round-9 gc CRIT-1: humongous continuation filler covers the entire
        // region with no live objects of its own; skip.
        if is_humongous_filler(header) {
            complete = false;
            break;
        }
        let obj_size = object_total_size(header);
        if obj_size < HEADER_SIZE || offset + obj_size > region.cursor() {
            complete = false;
            break;
        }
        if region.mark_bitmap.is_marked(obj_addr) {
            live += obj_size;
        }
        offset += obj_size;
    }
    (live, complete)
}

/// G1's to-space reserve in regions for a heap of `total` regions:
/// `G1ReservePercent`'s 10 %, at least one region. Kept Free for the next
/// pause's survivors by the evacuation-headroom trigger
/// (`G1Collector::young_exceeds_evac_headroom`), so the mutator never
/// allocates into it ([`G1Collector::old_absorbable_bytes`]).
pub fn to_space_reserve_regions(total: usize) -> usize {
    (total / 10).max(1)
}

/// gci i18/mx -- the Free-fraction arm of `G1Collector::needs_gc` (and of the
/// native-pressure latch, `note_region_consumed_locked`), before the young
/// floor (`low_free_trigger_allowed`) is applied.
///
/// Default: the Free pool is under `pct` % of `total` (`needs_gc_free_percent`,
/// 25 % and +8 per evacuation failure up to 50 %). On a heap filling with Old
/// data that keeps every young generation a few regions big (oo i16 findings 1
/// and 2: at 71 of 96 MB each young pause collected ~6 regions and Old filled
/// faster). Under `CRATONVM_G1_YOUNG_SIZES_TO_RESERVE` (`reserve_sizing`):
/// HotSpot's shape -- the young generation may use the Free pool down to the
/// `G1ReservePercent` to-space reserve ([`to_space_reserve_regions`]); the
/// evacuation-headroom arm (`young_exceeds_evac_headroom`, unchanged) still
/// bounds young by the predicted survivors plus that reserve.
#[inline]
pub fn free_fraction_trigger_fires(
    free: usize,
    total: usize,
    pct: usize,
    reserve_sizing: bool,
) -> bool {
    if reserve_sizing {
        free <= to_space_reserve_regions(total)
    } else {
        free * 100 < total * pct
    }
}

/// F-09 — round a requested region size up to a power of two.
///
/// # Why this is arithmetic, not policy
///
/// Region `i` starts at `arena_base + i * region_size`, so "which region owns
/// this address" is `(addr - arena_base) / region_size`. With a power-of-two
/// size that is a shift; without one it is a 64-bit hardware divide on the
/// hottest read in the collector — one the write barrier performs twice per
/// compiled reference store. HotSpot's `-XX:G1HeapRegionSize` carries the same
/// requirement and rounds the same way.
///
/// The rounding is applied in [`G1Collector::new`] before `num_regions`, the
/// arena length, the region bases and `region_shift` are derived from it, so
/// there is exactly one geometry rather than one for the allocator and another
/// for the lookup.
///
/// # What this deliberately does NOT do
///
/// It does not clamp to `[MIN_REGION_SIZE, 32 MiB]`. That range is an OPERATOR
/// ergonomic and belongs with the rest of the `-XX:` handling, in
/// `vm_heap::new_with_overrides`, which is where it now lives. Clamping here
/// would silently rewrite the 4 KiB and 8 KiB region sizes several unit tests
/// use to build a many-region heap cheaply — turning a 512-region fixture into
/// a 16-region one and quietly changing what those tests cover.
///
/// A zero request is returned unchanged: `G1Collector::new` already asserts
/// "the heap must fit at least one region", and that assertion is a better
/// place to report it than an arithmetic surprise here.
pub fn normalize_region_size(requested: usize) -> usize {
    if requested == 0 || requested.is_power_of_two() {
        return requested;
    }
    // Checked rather than the panicking form: a request within a factor of two
    // of `usize::MAX` is not a heap anyone can allocate, and `new`'s assertion
    // says so better than an overflow panic in a helper.
    requested.checked_next_power_of_two().unwrap_or(requested)
}

/// F-18 — the number of distinct object ages the tenuring histogram tracks.
///
/// `ObjectHeader::gc_age` is 4 bits, and `promotion_age` is capped at 15 by the
/// same encoding, so 16 buckets covers every age an object can hold.
pub const G1_MAX_TENURING_AGE: usize = 16;

/// F-18 — the share of the young generation survivors should occupy, as a
/// divisor.
///
/// 8, matching HotSpot's default `SurvivorRatio`: survivor space is about an
/// eighth of the young generation. It is a TARGET, not a limit — exceeding it
/// does not fail an allocation, it just means the adaptive threshold will
/// tenure earlier next pause so the copies stop being made.
///
/// gci i22/sz: HotSpot's `max_survivor_regions_d = young_list_target_length /
/// SurvivorRatio`, rounded UP to whole regions
/// (`G1Policy::update_survivors_policy`); see [`g1_desired_survivor_bytes`].
pub const G1_SURVIVOR_TARGET_DIVISOR: usize = 8;

/// gci i22/sz -- HotSpot's default `-XX:TargetSurvivorRatio` (percent): the
/// share of the survivor CAPACITY the age table may fill before the threshold
/// drops (`G1Policy::desired_survivor_size`). The default of
/// `G1Collector::target_survivor_ratio`; a typed `-XX:TargetSurvivorRatio`
/// replaces it (gci i23/tn, `G1Collector::set_tenuring_flags`), as a typed
/// `-XX:MaxTenuringThreshold` replaces `config.promotion_age`.
pub const G1_TARGET_SURVIVOR_RATIO: usize = 50;

/// gci i22/sz (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`) --
/// HotSpot 25's G1 desired survivor size, in bytes. Pure.
///
/// `G1Policy::update_survivors_policy`:
/// `desired_max_survivor_regions = ceil(young_list_target_length /
/// SurvivorRatio)`, and `desired_survivor_size(regions) = (size_t)(GrainWords
/// * regions * TargetSurvivorRatio / 100.0)` in WORDS, which
/// `AgeTable::compute_tenuring_threshold` compares the age table's word
/// counts against. Every survivor size is a whole number of words, so the
/// word target times 8 compared in bytes decides exactly as HotSpot does
/// (the Generational twin is `gen_heap_tenuring::desired_survivor_bytes`).
///
/// The old figure, `young_target * region / 8`, had neither the ceiling nor
/// the `TargetSurvivorRatio` term: twice HotSpot's for a young target of
/// eight regions or more (the threshold dropped later than HotSpot's), and a
/// fraction of it below four (a one-region young target aimed at an eighth of
/// a region where HotSpot aims at half of one).
pub fn g1_desired_survivor_bytes(young_target_regions: usize, region_size: usize) -> usize {
    g1_desired_survivor_bytes_at(young_target_regions, region_size, G1_TARGET_SURVIVOR_RATIO)
}

/// gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018` item
/// 5) -- [`g1_desired_survivor_bytes`] at an explicit `-XX:TargetSurvivorRatio`
/// (`0..=100`, clamped), the value `G1Collector::set_tenuring_flags` applies.
/// Pure. HotSpot's `desired_survivor_size(max_regions)`:
/// `(size_t)(GrainWords * max_regions * TargetSurvivorRatio / 100.0)` words.
pub fn g1_desired_survivor_bytes_at(
    young_target_regions: usize,
    region_size: usize,
    target_survivor_ratio: usize,
) -> usize {
    g1_desired_survivor_bytes_with(
        young_target_regions,
        region_size,
        target_survivor_ratio,
        G1_SURVIVOR_TARGET_DIVISOR,
    )
}

/// gci i43/gy (lane lf's hunk H-gy1) -- [`g1_desired_survivor_bytes_at`] at an
/// explicit `-XX:SurvivorRatio` (`survivor_ratio`, floor 1): the capacity is
/// [`g1_max_survivor_regions_at`] of it. Pure.
pub fn g1_desired_survivor_bytes_with(
    young_target_regions: usize,
    region_size: usize,
    target_survivor_ratio: usize,
    survivor_ratio: usize,
) -> usize {
    const WORD: usize = 8;
    let regions = g1_max_survivor_regions_at(young_target_regions, survivor_ratio);
    let capacity_words = regions.saturating_mul(region_size / WORD);
    (capacity_words.saturating_mul(target_survivor_ratio.min(100)) / 100).saturating_mul(WORD)
}

/// gci i23/tn (item 4 of the i22/sz page) -- HotSpot 25's desired maximum
/// survivor regions of a pause, `ceil(young_list_target_length /
/// SurvivorRatio)` (`G1Policy::update_survivors_policy`). Pure. The pause's
/// Survivor region cap ([`G1Collector::arm_survivor_region_cap`]) and the
/// survivor capacity of [`g1_desired_survivor_bytes_at`]; also the `(c)` of
/// the VM's `Survivor regions: a->b(c)` line (`gc_events.rs`,
/// `G1_SURVIVOR_RATIO`). A zero target reads as one.
pub fn g1_max_survivor_regions(young_target_regions: usize) -> usize {
    g1_max_survivor_regions_at(young_target_regions, G1_SURVIVOR_TARGET_DIVISOR)
}

/// gci i43/gy (lane lf's hunk H-gy1) -- [`g1_max_survivor_regions`] at an
/// explicit `-XX:SurvivorRatio` (HotSpot's range is `>= 1`; 0 reads as 1).
/// Pure.
pub fn g1_max_survivor_regions_at(young_target_regions: usize, survivor_ratio: usize) -> usize {
    young_target_regions.max(1).div_ceil(survivor_ratio.max(1))
}

/// g1ep6 -- HotSpot 25's young target (`G1Policy::calculate_young_target_length`)
/// for a heap of `total` regions: the `desired` length, but no more than the
/// `young` regions already allocated plus the `free` regions outside the
/// `G1ReservePercent` reserve ([`to_space_reserve_regions`]), and never less
/// than `young`. The survivor policy divides this, not the desired length: as
/// Old fills the Free pool, HotSpot's maximum Survivor regions fall with it.
/// Pure.
pub fn g1_young_target_within_free(desired: usize, young: usize, free: usize, total: usize) -> usize {
    let outside_reserve = free.saturating_sub(to_space_reserve_regions(total));
    young.saturating_add(desired.saturating_sub(young).min(outside_reserve))
}

/// gci i23/tn -- HotSpot's ceiling for `-XX:MaxTenuringThreshold` (and
/// `InitialTenuringThreshold`): `range(0, markWord::max_age + 1)`. 16 is
/// HotSpot's `NeverTenure` value: no age reaches it (an object's age stops
/// at 15), so nothing is tenured by age.
pub const G1_MAX_TENURING_THRESHOLD_LIMIT: u8 = 16;



/// gci i23/tn (`gci-i20ys-g1-young-model-and-mixed-futile-reset-as-default-review-findings-RETIRED-UNFIXED-20261018`
/// item 6) -- `CRATONVM_G1_MIXED_WAIVER_BASE_TRIGGER=1` (default OFF): the
/// mixed gate's `tight` waiver is measured against twice the BASE free
/// trigger ([`G1_BASE_FREE_TRIGGER_PCT`]) instead of twice the live
/// `needs_gc_free_percent`, which evacuation failures ratchet to its 50 %
/// cap -- where the waiver reads `tight` at any occupancy and the
/// `G1HeapWastePercent` floor never applies. Read once per mixed-gate poll.
pub fn g1_mixed_waiver_base_trigger() -> bool {
    false
}

/// gci i23/tn -- the free-pool trigger's base and floor (percent):
/// `needs_gc_free_percent` starts here and every clean pause decays it back
/// toward here (`retry_after_evacuation_failure`). Named for
/// [`g1_mixed_waiver_tight`]; the two literal sites keep their own `25`.
pub const G1_BASE_FREE_TRIGGER_PCT: usize = 25;

/// gci i23/tn -- the mixed gate's `tight` waiver. Pure. Default arm: the
/// Free pool is under twice the LIVE free trigger (capped at 100 %); under
/// [`g1_mixed_waiver_base_trigger`]: under twice the BASE trigger (50 %), so
/// a ratcheted trigger no longer turns the waiver on at every occupancy.
pub fn g1_mixed_waiver_tight(free: usize, total: usize, trigger_pct: usize, base_trigger: bool) -> bool {
    let pct = if base_trigger {
        G1_BASE_FREE_TRIGGER_PCT * 2
    } else {
        trigger_pct.max(1) * 2
    }
    .min(100);
    free.saturating_mul(100) < total.max(1).saturating_mul(pct)
}

/// LANE W5-A — the smallest `bytes_copied` a pause may have and still teach a
/// per-byte copy-cost calibration.
///
/// A ratio whose denominator is small is not a cheap measurement, it is noise:
/// `closure_ns / bytes` over a pause that copied 300 bytes is one cache miss
/// divided by nothing, and under the first-sample-adopted-whole rule
/// ([`G1Collector::fold_evac_cost`]) such a point would become the calibration
/// outright. 64 KiB is an eighth of the smallest region this collector ships
/// (512 KiB), i.e. "the pause copied at least a recognisable fraction of one
/// region's worth of live data".
///
/// A pause below the gate is REFUSED and counted
/// (`evac_cost_samples_refused`), not clamped: a clamp would still move the
/// estimate, and the honest statement about a denominator this small is that
/// the pause carries no information about cost per byte. The counter is what
/// makes "the calibration never moved" distinguishable from "every sample was
/// refused" — the distinction `evac_ns_per_byte=4` could not make for four
/// waves.
pub const G1_EVAC_COST_MIN_SAMPLE_BYTES: usize = 64 * 1024;

/// LANE W5-A — the fallback nanoseconds-per-live-byte used when NOTHING has
/// been measured yet, and the constructor value of `evac_ns_per_byte`.
///
/// 4 ns/byte (~250 MB/s). This is the number `w4b-a-drain-pass-is-not-a-pause.md`
/// §6 found the whole mixed copy budget was resting on for the life of every
/// process that never ran a mixed collection. It survives as the LAST resort —
/// the price used when neither the old-copy nor the young-copy calibration has
/// a sample — and it is now named, so a reader of the source can see that it is
/// a guess rather than a measurement.
pub const G1_EVAC_NS_PER_BYTE_DEFAULT: u64 = 4;

/// Floor for the adaptive young-generation size, as a percentage of the region
/// count (HotSpot's `G1NewSizePercent`). Below this the collector would pay a
/// full pause's fixed cost — root scan, remembered-set walk, whole-heap
/// reference fix-up — for a collection set too small to amortise it.
pub const G1_MIN_NEW_PERCENT: usize = 5;

/// Ceiling for the adaptive young-generation size, as a percentage of the
/// region count (HotSpot's `G1MaxNewSizePercent`), and the value it starts at.
/// See [`G1Collector::young_target_regions`] for why it starts at the ceiling
/// rather than the floor.
pub const G1_MAX_NEW_PERCENT: usize = 60;

/// LANE W7-L — what a mixed pause must RECLAIM, as a percentage of what it
/// COPIED, to count as productive.
///
/// The bar is set from both tails of the only measurement there is, W6-M's
/// 257 healthy mixed pauses against the ~5 000 pathological ones:
///
/// | arm | `bytes_freed / bytes_copied` |
/// |---|---|
/// | the working recipe, pooled over 257 pauses | median **3.69**, **minimum 2.06** |
/// | the futile loop, 5 000 consecutive pauses | median **1.00**, exact equality on 615 |
///
/// 150% sits in the empty middle of that gap, a factor of 1.37 below the
/// worst healthy pause ever recorded and 1.5x above the whole pathological
/// distribution. It is deliberately NOT 100%: a pause that reclaims one byte
/// more than it copies is not doing useful work either, and a screen placed
/// exactly at the fixed point would be decided by rounding.
///
/// The ratio is over the WHOLE pause, young half included, because that is
/// what the collector can measure — see [`G1Collector::note_mixed_productivity`]
/// for what that costs in sharpness and which way the error runs.
///
/// gci i17/mx: no longer the only bar. Both tails above came from workloads
/// whose healthy mixed pauses evacuated mostly-dead regions; a workload whose
/// Old regions die slowly (a table a quarter replaced per round) evacuates
/// candidates 67-85 % live, legitimately under `G1MixedGCLiveThresholdPercent`,
/// at 118-150 %. `mixed_choice::mixed_pause_productive` also counts a pause
/// productive when `freed * live_threshold > copied * 100`, which still puts
/// the whole pathological distribution (at or under 100 %) on the futile side.
pub const MIXED_PRODUCTIVE_RATIO_PCT: u64 = 150;

/// LANE W7-L — how many CONSECUTIVE futile mixed pauses trip the breaker.
///
/// Not one: a single unproductive mixed pause is ordinary. The phase's last
/// candidate can turn out to be JIT-pinned (`mixed_phase_has_work`'s own doc
/// calls that case out), a humongous reclaim can land in the pause before the
/// one that earned it, and a mark cycle that finished during heavy promotion
/// leaves one stale estimate behind. The defect this trips on is a FIXED
/// POINT — five thousand pauses, not five — so four is chosen to be far
/// outside ordinary noise and still 1 250x below the observed loop length.
pub const MIXED_FUTILE_STREAK_TRIP: u64 = 4;

/// LANE W7-L — with the breaker tripped, let one poll in this many through.
///
/// A breaker with no probe is a latch: once it refuses, no further mixed pause
/// can be measured, so the evidence that would close it can never be
/// collected, and the collector would have lost old-generation reclamation for
/// the life of the process on four pauses' evidence. That is a worse failure
/// than the one being fixed, and it is the failure mode a `debug_assert!` or
/// an unconditional refusal would have shipped.
///
/// So the refusal is a RATE, not a stop: the tripped breaker admits one mixed
/// pause every 32 polls, which either reclaims (streak resets, breaker closes,
/// full mixed service resumes) or does not (streak grows, nothing changes).
/// The cost of being wrong is therefore bounded at 1/32 of the pathological
/// pause count — 5 397 becomes ~170 — and the cost of being right is one
/// mixed pause in 32 on a heap that has started reclaiming again.
///
/// gci i20/ys (i18/mx finding 3): "polls" are polls INSIDE a phase. A refused
/// poll ends the phase, so with `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE=0`
/// the breaker is consulted about once per marking cycle and the cadence is
/// one admitted phase per ~32 CYCLES, not one pause per 32 polls. On the
/// shipped default (that switch ON since 2026-10-15) the first poll of every
/// phase restarts the streak, so a tripped breaker costs at most
/// [`MIXED_FUTILE_STREAK_TRIP`] futile pauses per phase and this probe matters
/// only within one long phase.
pub const MIXED_FUTILE_PROBE_EVERY: u64 = 32;

/// Slots in the per-thread remembered-set edge memo — see the
/// `RSET_EDGE_MEMO` thread-local in `post_write_barrier_rset`.
///
/// Four, because a reference-store loop into one destination array reaches
/// objects spread over several SOURCE regions, and a one-slot memo alternates
/// between them and misses every time. Measured (`BarrierProbe refstore`, 8
/// threads, 2M stores, ordinary arrays): one slot left the mode scaling 4.9x
/// from one thread to eight while the barrier-free modes went flat.
pub const RSET_EDGE_MEMO_SLOTS: usize = 4;

// JIT round 21, lane vhcae (VHCAE21-1; page
// `docs/feature-designs/jit-barrier-lowering.md`):
// a per-collector, JIT-addressable form of the remembered-set edge memo, so a
// compiled cross-region reference store whose barrier would be a no-op takes
// no call (`cratonvm_jit::ir_g1_edge_filter`, `CRATONVM_JIT_G1_EDGE_FILTER`).
//
// # What a hit proves
//
// `entries[dst]` holds `epoch << 32 | (src + 1)` (the low 32 bits of the
// epoch) for the LAST `(src, dst)` edge some thread recorded into region
// `dst`'s remembered set, written by [`G1Collector::post_write_barrier_rset`]
// only after its insert landed (`remember_edge`, with the epoch the insert was
// validated under) or on a per-thread memo hit (with the epoch that memo was
// keyed on). Compiled code compares it against `epoch_mirror << 32 |
// (src + 1)`: equal means the edge was recorded in the CURRENT
// reclassification epoch, which is exactly the claim the per-thread memos'
// epoch keys make -- an rset is a set, and only a recycle/retype phase (which
// bumps the epoch first, under the regions write lock,
// `G1Collector::bump_rset_cache_epoch`) clears or prunes it. A stale entry can
// only be from an older epoch and so never compares equal; the low 32 bits
// wrap only after 2^32 bumps, and the bump that wraps them clears every entry.
//
// The compiled reader ALSO requires the source card to be dirty already and
// its block offset to be at or below the store's object offset -- the two
// things the barrier's card half writes (`dirty_addr_at_object_start`; a dirty
// card implies a dirty summary byte, which is stored first and re-derived from
// the cards by every cleaner). Every card cleaner runs at a safepoint
// (`G1CardTable::clean_epoch`), and the compiled sequence has none, so a
// skipped card store would have been a no-op. Everything else (a clean card,
// a block offset to lower, a different edge, another epoch, an address
// outside the regions) is a miss: the unchanged barrier call.
//
// # Layout
//
// `#[repr(C)]`; the compiled reader bakes words 1..=10 at compile time (they
// never change for the collector's life) and loads word 0 and the entries at
// run time. The word offsets are pinned below and mirrored by
// `cratonvm_jit::ir_g1_edge_filter::W_*`; the VM hands the address over through
// the well-known resolver (`WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG`). Owned
// by the collector (a `Box`, so its address survives the collector moving):
// per VM, never a process static.
// Words 1..=10 are read only by compiled code, through the pinned offsets.
#[allow(dead_code)]
#[repr(C)]
pub struct G1JitEdgeFilter {
    /// Word 0: a mirror of `rset_cache_epoch`, stored (`Release`) by every
    /// bump right after the bump itself.
    pub epoch_mirror: AtomicU64,
    /// Word 1: the arena base region indices (and card indices) are relative to.
    pub arena_base: usize,
    /// Word 2: `region_count << region_shift` -- the span holding regions (an
    /// address at or above it has no region and no entry).
    pub regions_len: usize,
    /// Word 3: log2 of the region size.
    pub region_shift: usize,
    /// Word 4: address of the F-05 card table's card 0.
    pub cards_base: usize,
    /// Word 5: address of the block-offset table's entry 0.
    pub starts_base: usize,
    /// Word 6: address of `entries[0]`.
    pub entries_addr: usize,
    /// Word 7: number of entries (regions).
    pub region_count: usize,
    /// Word 8: [`crate::g1_cards::G1_CARD_SHIFT`].
    pub card_shift: usize,
    /// Word 9: [`crate::g1_cards::G1_BLOCK_GRAIN_SHIFT`].
    pub block_grain_shift: usize,
    /// Word 10: [`crate::g1_cards::G1_CARD_DIRTY`].
    pub card_dirty: usize,
    /// One packed key per destination region (0 = empty).
    pub entries: Box<[AtomicU64]>,
    /// vhc21 census (VHCAE21-7), counted only under `CRATONVM_DBG_G1ACCESSOR`
    /// (see [`Self::census_overwrite`]): `note_edge` calls that found the entry
    /// already holding the edge.
    pub census_unchanged: AtomicU64,
    /// ... calls that filled an empty entry or replaced one of an older epoch.
    pub census_first: AtomicU64,
    /// ... calls that replaced a live-epoch entry naming ANOTHER source region
    /// (the alternation a 2-way entry would absorb).
    pub census_alternating: AtomicU64,
}

const _: () = {
    assert!(std::mem::offset_of!(G1JitEdgeFilter, epoch_mirror) == 0);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, arena_base) == 8);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, regions_len) == 16);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, region_shift) == 24);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, cards_base) == 32);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, starts_base) == 40);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, entries_addr) == 48);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, region_count) == 56);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, card_shift) == 64);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, block_grain_shift) == 72);
    assert!(std::mem::offset_of!(G1JitEdgeFilter, card_dirty) == 80);
};

impl G1JitEdgeFilter {
    pub fn new(
        arena_base: usize,
        region_count: usize,
        region_shift: u32,
        cards: &crate::g1_cards::G1CardTable,
    ) -> Box<Self> {
        let entries: Box<[AtomicU64]> = (0..region_count).map(|_| AtomicU64::new(0)).collect();
        let entries_addr = entries.as_ptr() as usize;
        // The card index the compiled reader computes is relative to the
        // arena base; it is the card table's own index only while both share
        // that origin (they do: one table for the whole reservation). A table
        // with another origin gets an all-zero geometry, which the JIT refuses.
        let shared_origin = cards.arena_base() == arena_base;
        Box::new(Self {
            epoch_mirror: AtomicU64::new(0),
            arena_base: if shared_origin { arena_base } else { 0 },
            regions_len: region_count << region_shift,
            region_shift: region_shift as usize, // Cast: a shift below 64
            cards_base: cards.cards_base_addr(),
            starts_base: cards.starts_base_addr(),
            entries_addr,
            region_count,
            card_shift: crate::g1_cards::G1_CARD_SHIFT as usize, // Cast: a small shift
            block_grain_shift: crate::g1_cards::G1_BLOCK_GRAIN_SHIFT as usize, // Cast: as above
            card_dirty: usize::from(crate::g1_cards::G1_CARD_DIRTY),
            entries,
            census_unchanged: AtomicU64::new(0),
            census_first: AtomicU64::new(0),
            census_alternating: AtomicU64::new(0),
        })
    }

    /// The packed key compiled code compares: the epoch's low 32 bits above
    /// `src + 1` (never 0, the empty entry).
    #[inline]
    pub fn key(epoch: u64, src_idx: usize) -> u64 {
        (epoch << 32) | ((src_idx as u64) + 1) // Cast: a region index, far below 2^32
    }

    /// Record that `(src_idx -> dst_idx)` is in `dst_idx`'s remembered set in
    /// `epoch`. Only when it differs: a hot edge is a load, not a store into a
    /// line other threads read.
    #[inline]
    pub fn note_edge(&self, dst_idx: usize, src_idx: usize, epoch: u64) {
        let Some(slot) = self.entries.get(dst_idx) else {
            return;
        };
        let key = Self::key(epoch, src_idx);
        let prev = slot.load(Ordering::Relaxed);
        if prev != key {
            if gc_flags().g1_dbg_accessor {
                self.census_overwrite(prev, key);
            }
            slot.store(key, Ordering::Relaxed);
        } else if gc_flags().g1_dbg_accessor {
            self.census_unchanged.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// vhc21 (VHCAE21-7): does replacing `prev` by `key` (`prev != key`) drop a
    /// live-epoch edge from ANOTHER source region of the same destination? Both
    /// keys carry the epoch's low 32 bits above `src + 1`; an empty entry (`0`)
    /// or an older epoch is a plain fill, not an alternation.
    #[inline]
    pub fn overwrite_alternates(prev: u64, key: u64) -> bool {
        prev != 0 && (prev >> 32) == (key >> 32)
    }

    /// vhc21 (VHCAE21-7): book one overwrite for the
    /// `CRATONVM_DBG_G1ACCESSOR` census. The entry holds ONE source per
    /// destination region, so a destination written from two holder regions in
    /// turn misses in compiled code every time; `census_alternating` counts
    /// exactly those replacements.
    pub fn census_overwrite(&self, prev: u64, key: u64) {
        let counter = if Self::overwrite_alternates(prev, key) {
            &self.census_alternating
        } else {
            &self.census_first
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// vhc21: `(unchanged, first_or_new_epoch, alternating)` so far.
    pub fn census(&self) -> (u64, u64, u64) {
        (
            self.census_unchanged.load(Ordering::Relaxed),
            self.census_first.load(Ordering::Relaxed),
            self.census_alternating.load(Ordering::Relaxed),
        )
    }

    /// The bump's half: publish the new epoch; on a wrap of its low 32 bits,
    /// first retire every entry (a key from 2^32 epochs ago would compare
    /// equal again).
    pub fn publish_epoch(&self, epoch: u64) {
        if epoch & 0xFFFF_FFFF == 0 {
            for e in self.entries.iter() {
                e.store(0, Ordering::Relaxed);
            }
        }
        self.epoch_mirror.store(epoch, Ordering::Release);
    }

    /// The address the VM hands to the JIT.
    pub fn addr(&self) -> usize {
        self as *const Self as usize
    }
}

// ---------------------------------------------------------------------------
// Collection type
// ---------------------------------------------------------------------------

/// Type of G1 collection to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G1CollectionType {
    /// Evacuate all Eden + Survivor regions.
    YoungOnly,
    /// Evacuate young + selected old regions.
    Mixed,
    /// Full heap compaction (fallback when evacuation fails).
    Full,
}

/// One STW collection's pause record (§7 item 6 — the structured pause sink).
///
/// Pause is recorded in **microseconds** (not milliseconds): a young G1 pause
/// is routinely sub-millisecond, so the old `Duration::as_millis()` rounded the
/// whole young series to `0` and made p50/p99 reporting (§5) impossible. The
/// collector keeps a bounded ring of the most recent records (see
/// `G1Collector::pause_history`) which `pause_summary()` reduces to percentiles.
#[derive(Debug, Clone, Copy)]
pub struct G1PauseRecord {
    /// Young / mixed / full.
    pub collection_type: G1CollectionType,
    /// STW pause duration in microseconds.
    pub pause_us: u64,
    /// Live objects copied.
    pub objects_copied: usize,
    /// Bytes copied (headers + slot data).
    pub bytes_copied: usize,
    /// Bytes reclaimed.
    pub bytes_freed: usize,
    /// Where the pause went. See [`G1PausePhases`].
    pub phases: G1PausePhases,
}

/// G1AUD-10 — per-phase breakdown of one evacuation pause, in microseconds.
///
/// `pause_us` alone cannot answer the question every G1 pause investigation
/// starts with: WHICH phase is long. The collector's phases have very
/// different scaling — the root scan is O(roots), the Cheney closure is
/// O(young live), and the reference fix-up is O(WHOLE HEAP) — so a single
/// total cannot even distinguish "the young set grew" from "the old
/// generation grew", which call for opposite responses.
///
/// This is not a debug-flag feature. It is filled unconditionally (four
/// `Instant::elapsed` calls on a path that already takes one) and rendered on
/// every `--verbose:gc` `[GC-STAT]` line, because the phase whose cost is
/// unmeasured is the phase that stays unfixed: the whole-heap fix-up walk had
/// been described in the audit as a structural limit for months with no number
/// attached to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct G1PausePhases {
    /// Phase 1 + 1b — root evacuation and marking keep-alive.
    pub roots_us: u64,
    /// Phase 2 — remembered-set source region walks.
    pub rset_us: u64,
    /// Phase 3 (+3.5) — the Cheney closure over evacuated objects.
    pub closure_us: u64,
    /// gci i14/yt — on the PARALLEL drivers, the part of the evacuation
    /// interval `parallel_evacuate` spent seeding (Phases 1-1c roots and
    /// Phase 2 rset sources), now reported in `roots_us` / `rset_us` instead
    /// of inside `closure_us`. Always `roots_us + rset_us` there; 0 on the
    /// serial drivers, whose closure never contained the seed. The adaptive
    /// controllers read [`Self::fused_closure_us`], the pre-split figure, so
    /// the split is reporting only.
    pub parallel_seed_us: u64,
    /// Phase 4 — the whole-heap reference fix-up, remembered-set rebuild and
    /// humongous census. The one phase that scales with the LIVE HEAP rather
    /// than with the collection set.
    pub fixup_us: u64,
    /// Phase 5 + eager humongous reclaim — freeing the collection set.
    pub free_us: u64,
    /// The post-pause verifiers: [`G1Collector::verify_no_dangling_into_cset`]
    /// (budgeted, and therefore running in RELEASE builds since audit §9 item
    /// 2) plus the env-gated `dbg_verify_*` family.
    ///
    /// It has its own field because it is not free and it was not attributable.
    /// The verifier runs AFTER `free_us` is recorded and BEFORE `pause_us` is
    /// taken, so its cost landed in the pause total and in none of the five
    /// phases: the breakdown did not sum to the pause, and the difference was
    /// invisible. A budget nobody can see the cost of is a budget nobody can
    /// argue about — `CRATONVM_G1_VERIFY_BUDGET` is the A/B, and this is its
    /// readout.
    pub verify_us: u64,
    /// Everything in the pause that is not one of the six phases above: the
    /// CSet construction and pin census before Phase 1, the monitor /
    /// skip-set / dedup-table / mark-worklist remaps after Phase 5, and the
    /// bookkeeping around them.
    ///
    /// Derived, not measured (`pause_us` minus the six), and present so that
    /// the breakdown is a PARTITION of the pause rather than a sample of it.
    /// A phase table whose rows do not sum to the total cannot be used to
    /// argue that a cost has been removed rather than moved.
    pub other_us: u64,
    /// Non-CSet regions the fix-up walked, and bytes it walked over them.
    /// The denominator for `fixup_us`: without it a long fix-up cannot be
    /// told from a large old generation.
    pub fixup_regions: u32,
    pub fixup_bytes: u64,
    /// F-05 — how many remembered-set source regions this pause was asked to
    /// walk, and how many of them the card screen let it skip outright because
    /// no card of the region was dirty.
    ///
    /// The denominator for `rset_us`, and the engagement readout for the card
    /// table. `rset_regions_offered == 0` means Phase 2 had nothing to do at
    /// all — a different fact from `rset_regions_skipped == 0`, which means the
    /// screen ran and refused every region, which is in turn different from
    /// "the feature is off" (`CRATONVM_G1_CARD_RSET=0`, under which
    /// `rset_regions_skipped` and `rset_bytes_skipped` are both zero while
    /// `rset_bytes_scanned` accounts for everything).
    pub rset_regions_offered: u32,
    pub rset_regions_skipped: u32,
    /// F-05 — bytes of objects whose reference slots Phase 2 actually visited,
    /// and bytes it stepped over because the object touched no dirty card.
    ///
    /// Their sum is what the pre-F-05 walk would have visited, so
    /// `skipped / (scanned + skipped)` is the fraction of the source walk the
    /// card table removed.
    pub rset_bytes_scanned: u64,
    pub rset_bytes_skipped: u64,
    /// The region census AFTER this pause: Free / Eden / Survivor / Old /
    /// humongous.
    ///
    /// `needs_gc` triggers on the FREE FRACTION, so "how much did this pause
    /// buy" is not answerable from `bytes_freed` alone -- it is answerable
    /// from how many regions came back. A pause that frees bytes without
    /// returning regions to the Free pool leaves the trigger latched and the
    /// next pause starts immediately, which is the storm shape
    /// `young_target_regions` documents as "a trigger the collector cannot
    /// satisfy".
    ///
    /// Filled inside the pause, where the regions guard is already held. The
    /// census this replaces was written as a `try_lock` inside the `[GC-STAT]`
    /// emitter -- and every collection path calls that emitter while holding
    /// the guard, so the `try_lock` ALWAYS failed and the counts were never
    /// once printed. An instrument armed where it cannot fire.
    pub free_regions: u32,
    pub eden_regions: u32,
    pub surv_regions: u32,
    pub old_regions: u32,
    pub hum_regions: u32,
    /// gci i14/yt — bytes in use (`cursor()`) summed over the Survivor regions
    /// of the same census. `surv_regions` counts regions, and on G1 4-thread
    /// javac each of ~21 copying workers claims its own whole Survivor region
    /// per pause (`TlabSet::survivor`), so ~2 MB of survivors occupied ~20
    /// regions; HotSpot packs them through PLABs into shared regions. The
    /// HotSpot-shaped young model (`CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`)
    /// charges survivors to the young target by this byte figure. Filled by
    /// `record_region_census`; 0 on a record without a census.
    pub surv_bytes: u64,
    /// How many regions this pause actually collected, and how many young
    /// regions each pin vocabulary kept OUT of that set.
    ///
    /// `bytes_freed` cannot separate "there was little garbage" from "almost
    /// nothing was collectable": measured on the H2 class, 19 268 young pauses
    /// in 240 s freeing a mean of 356 KB each -- about a third of a region per
    /// pause, on a heap whose trigger is a 25% free fraction. Either the
    /// collection set is tiny or it is nearly all pinned out, and only these
    /// three numbers say which.
    pub cset_regions: u32,
    pub jni_pinned_out: u32,
    pub jit_pinned_out: u32,
    /// LANE E wave 2 — the index of the highest NON-Free region after this
    /// pause, plus one; i.e. the number of regions the heap still has to keep
    /// committed. `0` means the whole table is Free.
    ///
    /// Carried on the phase record rather than recomputed because the one place
    /// that wants it — [`G1Collector::record_collection_with_phases`], which
    /// runs the heap-resize policy — reaches that function holding the regions
    /// guard as a WRITER and `parking_lot`'s `RwLock` is not reentrant, so it
    /// cannot take a read guard to ask. This is the same constraint that makes
    /// the `[GC-STAT]` `try_read` census structurally dead, answered the same
    /// way the region census answered it: take the number inside the pause,
    /// where the table is already borrowed.
    ///
    /// Also the whole of the shrink's safety argument in numeric form — the
    /// trailing run `[highest_used_region, len)` is Free by construction, so
    /// releasing its pages cannot unmap a byte any walker reads.
    pub highest_used_region: u32,
    /// Total regions in the table this census walked. The denominator for
    /// `highest_used_region`, and for the occupancy the resize policy decides
    /// on; carried rather than re-derived so the policy and the census can
    /// never disagree about how big the heap is.
    pub total_regions: u32,
    /// LANE W3-C — is this record a DRAIN PASS rather than a stop-the-world
    /// pause?
    ///
    /// `drain_kept_self_forwards` calls
    /// [`G1Collector::record_collection_with_phases`] with
    /// `G1CollectionType::YoungOnly`, and `retry_after_evacuation_failure`
    /// calls it up to eight times INSIDE one young pause that has already
    /// recorded itself. So `[GC-SUMMARY] young count=` was not a count of
    /// young pauses: an evacuation-failure pause contributed up to nine
    /// entries to a population of "pauses", the extra eight being nested
    /// intervals of the first. The percentiles that population produces are
    /// not percentiles over pauses — a 6 ms drain pass and a 94 ms pause sit
    /// in the same ranking.
    ///
    /// Two separate defects, and this flag is how both are told:
    ///
    ///  * the pause percentiles now cover only true pauses
    ///    ([`G1Collector::pause_summary`] reports drain passes as their own
    ///    figure, so nothing is hidden — the record is still in the ring);
    ///  * the `[GC-STAT]` line says `drain_pass=true`, so a reader scraping
    ///    stderr is not counting them as pauses either.
    ///
    /// It is also how the third instance of the all-zero-census bug was found.
    /// `record_region_census` was called on four of the FIVE paths that record
    /// a collection; the drain was the fifth, so every drain pass printed
    /// `free_regions=0 eden_regions=0 surv_regions=0 old_regions=0
    /// hum_regions=0 cset_regions=0` beside `objects_copied=112601` — the
    /// exact wording of the PARITY note in `young_collection_parallel`, which
    /// says of the previous incarnation that it "reads as a fact about the
    /// heap and is a fact about the instrument".
    pub drain_pass: bool,
    /// gci i18/mx -- this mixed-driver pause self-forwarded at least one
    /// object (an evacuation failure: its pointer map holds an identity
    /// entry). Its `bytes_freed` leaves out every region it KEPT, so its
    /// `freed / copied` reads futile by construction; it is not a sample for
    /// the futile-copy breaker (`G1Collector::note_mixed_productivity`).
    /// Set by both mixed drivers; `false` elsewhere.
    pub evacuation_failed: bool,
    /// gci i18/mx -- this mixed-driver pause was a full compaction
    /// (`full_compaction_this_pause`) or ran an allocation-failure compaction
    /// plan. Its old half is not the mixed choice (it moves whatever the
    /// window / the Free pool allows, live or not), so it is not a sample for
    /// the futile-copy breaker either: the last-ditch `92M->92M` compaction
    /// used to count as one more futile mixed pause.
    pub compaction: bool,
}

impl G1PausePhases {
    /// Fill the region census from a borrowed region table.
    pub fn record_region_census(&mut self, regions: &[G1Region]) {
        let (mut f, mut e, mut s, mut o, mut h) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let mut highest_used = 0usize;
        // gci i14/yt: Survivor bytes in use, for the HotSpot-shaped young
        // model (see `surv_bytes`).
        let mut sb = 0u64;
        for (i, r) in regions.iter().enumerate() {
            match r.region_type() {
                RegionType::Free => f += 1,
                RegionType::Eden => e += 1,
                RegionType::Survivor => {
                    s += 1;
                    sb = sb.saturating_add(r.cursor() as u64);
                }
                RegionType::Old => o += 1,
                _ => h += 1,
            }
            if r.region_type() != RegionType::Free {
                highest_used = i + 1;
            }
        }
        self.free_regions = f;
        self.eden_regions = e;
        self.surv_regions = s;
        self.old_regions = o;
        self.hum_regions = h;
        self.surv_bytes = sb;
        self.highest_used_region = highest_used as u32;
        self.total_regions = regions.len() as u32;
    }

    /// gci i14/yt — `closure_us` as the controllers measured it before the
    /// parallel seed split: the closure plus whatever seeding the parallel
    /// driver split out of it ([`Self::parallel_seed_us`]). On the serial
    /// drivers this is `closure_us`.
    #[inline]
    pub fn fused_closure_us(&self) -> u64 {
        self.closure_us.saturating_add(self.parallel_seed_us)
    }

    /// gci i14/yt — record a parallel driver's evacuation interval `total_us`
    /// (seed + closure + Phase 3.5 / overlay) split by the seed times
    /// `parallel_evacuate` measured: `roots_us` / `rset_us` get the seed,
    /// `closure_us` the rest, and `parallel_seed_us` exactly
    /// `total_us - closure_us`, so [`Self::fused_closure_us`] is `total_us`
    /// whatever the micro-second rounding did (the controllers see the
    /// pre-split number byte for byte) and the phases still partition the
    /// pause.
    pub fn set_parallel_evacuation_us(&mut self, total_us: u64, seed: (u64, u64)) {
        let (roots_us, rset_us) = seed;
        let rset_us = rset_us.min(total_us);
        let roots_us = roots_us.min(total_us - rset_us);
        self.roots_us = roots_us;
        self.rset_us = rset_us;
        self.closure_us = total_us - rset_us - roots_us;
        self.parallel_seed_us = total_us - self.closure_us;
    }

    /// Sum of the six measured phases.
    #[inline]
    pub fn measured_us(&self) -> u64 {
        self.roots_us
            .saturating_add(self.rset_us)
            .saturating_add(self.closure_us)
            .saturating_add(self.fixup_us)
            .saturating_add(self.free_us)
            .saturating_add(self.verify_us)
    }

    /// Fill [`Self::other_us`] from the pause total, so the six measured
    /// phases plus `other_us` partition `pause_us` exactly.
    ///
    /// Saturating: the phase clocks and the pause clock are separate
    /// `Instant`s and each phase truncates to whole microseconds, so on a
    /// short pause the parts can round to slightly more than the whole. That
    /// reports `other_us = 0` rather than underflowing.
    #[inline]
    pub fn close(&mut self, pause_us: u64) {
        self.other_us = pause_us.saturating_sub(self.measured_us());
    }
}

/// gci i9/fa (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`, G1 half)
/// -- the run's [`G1PausePhases`], summed, for the `[GC] g1 pause-phases:`
/// line of the end-of-run summary.
///
/// The per-pause breakdown is printed only on `--verbose:gc`'s `[GC-STAT]`
/// lines, and the foreign-attach rows run with `CRATONVM_GC_STATS=1` alone:
/// their G1 pause averaged 1.2-1.7 ms against HotSpot G1's 0.23 ms on the same
/// probe and heap, and the only phase figures they printed were two decaying
/// estimates (`fixup_ns_ema`, `young_copy_ns_ema`), which cannot say what
/// share of the pause the seed walk, the closure or the fix-up took. These
/// sums can, and the denominators beside them (`fixup_bytes`, the pinned-out
/// regions) say whether a long phase is long because of what it walked.
///
/// Fed from `record_collection_with_phases_within` with every instrumented
/// pause (a pause whose breakdown is all zero is "not instrumented" and is
/// skipped); a drain pass is counted but not summed, because its interval is
/// nested in a young pause that recorded itself (`G1PausePhases::drain_pass`).
/// Instance state, not a process static: relaxed adds, one set per pause.
#[derive(Debug, Default)]
pub struct G1PhaseTotals {
    pub pauses: AtomicU64,
    pub drain_passes: AtomicU64,
    pub pause_us: AtomicU64,
    pub roots_us: AtomicU64,
    pub rset_us: AtomicU64,
    pub closure_us: AtomicU64,
    pub fixup_us: AtomicU64,
    pub free_us: AtomicU64,
    pub verify_us: AtomicU64,
    pub other_us: AtomicU64,
    pub fixup_regions: AtomicU64,
    pub fixup_bytes: AtomicU64,
    pub rset_bytes_scanned: AtomicU64,
    pub rset_bytes_skipped: AtomicU64,
    pub cset_regions: AtomicU64,
    pub jit_pinned_out: AtomicU64,
    pub jni_pinned_out: AtomicU64,
    /// Pinned regions (and their bytes) `CRATONVM_G1_FIXUP_SKIP_CLEAN_PINNED`
    /// took out of the Phase-4 walk ([`G1Collector::drop_clean_pinned_from_fixup`]).
    /// Zero with the switch off.
    pub fixup_skipped_regions: AtomicU64,
    pub fixup_skipped_bytes: AtomicU64,
    /// gci i10/pp: Phase-4 walks that ran on the pool
    /// ([`G1Collector::update_references_in_regions_parallel`],
    /// `CRATONVM_G1_PARALLEL_FIXUP=1`). Counts WALKS, drain passes included,
    /// so it can exceed `pauses`. Zero with the switch off.
    pub fixup_parallel_walks: AtomicU64,
    /// gci i10/pp: pin words `CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR=1` declined to
    /// turn into a region pin because they name no allocated byte
    /// ([`pin_word_names_no_allocation`]): JIT snapshot words plus non-object
    /// roots, summed over every pin-set construction. Zero with the switch off.
    pub pin_words_above_cursor: AtomicU64,
    /// g1sz: fresh to-space regions (and their bytes) Phase 4 did not walk
    /// because the closure settled them (`CRATONVM_G1_FIXUP_SKIP_CLOSURE_TOSPACE`,
    /// [`G1Collector::drop_closure_settled_tospace_from_fixup`]).
    pub fixup_closure_skipped_regions: AtomicU64,
    pub fixup_closure_skipped_bytes: AtomicU64,
    /// g1sz: parallel pauses with the switch on whose closure could not vouch
    /// for its to-space (see [`ClosureTospaceEdges`]), so Phase 4 walked it.
    pub fixup_closure_refused: AtomicU64,
    /// g1sz: wall time the parallel drivers spent building the pause's
    /// `PointerMap` from the workers' forward lists (a part of `closure_us`).
    pub pointer_map_us: AtomicU64,
}

impl G1PhaseTotals {
    /// g1sz: one pause's closure-settled to-space skip (`regions` 0 and
    /// `refused` true when the closure could not vouch for it).
    pub fn note_pointer_map_build(&self, us: u64) {
        self.pointer_map_us.fetch_add(us, Ordering::Relaxed);
    }

    pub fn note_fixup_closure_skip(&self, regions: u64, bytes: u64, refused: bool) {
        self.fixup_closure_skipped_regions.fetch_add(regions, Ordering::Relaxed);
        self.fixup_closure_skipped_bytes.fetch_add(bytes, Ordering::Relaxed);
        if refused {
            self.fixup_closure_refused.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// gci i9/fa: one pause's Phase-4 skip, as counted by
    /// [`G1Collector::drop_clean_pinned_from_fixup`].
    pub fn note_fixup_skip(&self, regions: u64, bytes: u64) {
        self.fixup_skipped_regions.fetch_add(regions, Ordering::Relaxed);
        self.fixup_skipped_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// gci i10/pp: one Phase-4 walk ran on the pool.
    pub fn note_fixup_parallel(&self) {
        self.fixup_parallel_walks.fetch_add(1, Ordering::Relaxed);
    }

    /// gci i10/pp: pin words the above-cursor screen declined.
    pub fn note_pin_words_above_cursor(&self, words: u64) {
        self.pin_words_above_cursor.fetch_add(words, Ordering::Relaxed);
    }

    /// Add one pause. `pause_us` is the pause total `phases` partitions;
    /// `other` is recomputed from it rather than read from `phases.other_us`,
    /// so a caller that never ran [`G1PausePhases::close`] still sums to it.
    pub fn note(&self, phases: &G1PausePhases, pause_us: u64) {
        if *phases == G1PausePhases::default() {
            return;
        }
        if phases.drain_pass {
            self.drain_passes.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let add = |c: &AtomicU64, v: u64| {
            c.fetch_add(v, Ordering::Relaxed);
        };
        add(&self.pauses, 1);
        add(&self.pause_us, pause_us);
        add(&self.roots_us, phases.roots_us);
        add(&self.rset_us, phases.rset_us);
        add(&self.closure_us, phases.closure_us);
        add(&self.fixup_us, phases.fixup_us);
        add(&self.free_us, phases.free_us);
        add(&self.verify_us, phases.verify_us);
        add(&self.other_us, pause_us.saturating_sub(phases.measured_us()));
        add(&self.fixup_regions, u64::from(phases.fixup_regions));
        add(&self.fixup_bytes, phases.fixup_bytes);
        add(&self.rset_bytes_scanned, phases.rset_bytes_scanned);
        add(&self.rset_bytes_skipped, phases.rset_bytes_skipped);
        add(&self.cset_regions, u64::from(phases.cset_regions));
        add(&self.jit_pinned_out, u64::from(phases.jit_pinned_out));
        add(&self.jni_pinned_out, u64::from(phases.jni_pinned_out));
    }

    /// The `[GC] g1 pause-phases:` line. Sums in microseconds, bytes and
    /// region-pauses; divide by `pauses` for the per-pause mean.
    pub fn line(&self) -> String {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        format!(
            "[GC] g1 pause-phases: pauses={} drain_passes={} pause_us={} roots_us={} rset_us={} closure_us={} fixup_us={} free_us={} verify_us={} other_us={} fixup_regions={} fixup_bytes={} rset_bytes_scanned={} rset_bytes_skipped={} cset_regions={} jit_pinned_out={} jni_pinned_out={} fixup_skipped_regions={} fixup_skipped_bytes={} fixup_parallel_walks={} pin_words_above_cursor={} fixup_closure_skipped_regions={} fixup_closure_skipped_bytes={} fixup_closure_refused={} pointer_map_us={} (sums over the instrumented young and mixed pauses; drain passes counted, not summed)",
            g(&self.pauses),
            g(&self.drain_passes),
            g(&self.pause_us),
            g(&self.roots_us),
            g(&self.rset_us),
            g(&self.closure_us),
            g(&self.fixup_us),
            g(&self.free_us),
            g(&self.verify_us),
            g(&self.other_us),
            g(&self.fixup_regions),
            g(&self.fixup_bytes),
            g(&self.rset_bytes_scanned),
            g(&self.rset_bytes_skipped),
            g(&self.cset_regions),
            g(&self.jit_pinned_out),
            g(&self.jni_pinned_out),
            g(&self.fixup_skipped_regions),
            g(&self.fixup_skipped_bytes),
            g(&self.fixup_parallel_walks),
            g(&self.pin_words_above_cursor),
            g(&self.fixup_closure_skipped_regions),
            g(&self.fixup_closure_skipped_bytes),
            g(&self.fixup_closure_refused),
            g(&self.pointer_map_us),
        )
    }
}

/// gci i9/fa -- one young pause's pin set, attributed to the vocabularies that
/// published it (`CRATONVM_G1_DBG_PINS=1` only; see
/// [`G1Collector::note_pin_attribution`]). Region counts, except `bytes`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct G1PinAttributionRow {
    /// Regions in the pin set, how many of them young, and their occupancy.
    pub regions: u64,
    pub young: u64,
    pub bytes: u64,
    /// Regions holding a kept (unmovable) word the COLLECTING thread
    /// published, any other thread or this cycle's helper windows published,
    /// or a frozen peer's un-retired TLAB tail.
    pub own: u64,
    pub others: u64,
    pub tlab: u64,
    /// Regions only one vocabulary claims: removing that publisher's words
    /// would have returned them to the collection set.
    pub own_only: u64,
    pub others_only: u64,
    pub others_only_young: u64,
    /// Regions none of the three claims: pinned by a root that is not an
    /// object start (`pinned_region_set_including_non_object_roots`).
    pub nonobj_only: u64,
    /// This cycle's helper-window words (part of `others`).
    pub xt_words: u64,
    /// gci i24/jp (i20/vh review item 4): regions holding a kept word of THIS
    /// cycle's cross-thread pins alone (`xt_pins`: the take-over, the helper
    /// windows, the deferred blocked chains). `own` and `others` both carry
    /// those words (the initiator republishes them), so neither column can
    /// attribute a helper-window pin; this one can.
    pub xt: u64,
}

/// gci i9/fa -- [`G1PinAttributionRow`]s summed over the run, for the
/// `[GC] g1 pin-attribution:` line (printed only when a pause was attributed).
#[derive(Debug, Default)]
pub struct G1PinAttribution {
    pub pauses: AtomicU64,
    pub regions: AtomicU64,
    pub young: AtomicU64,
    pub bytes: AtomicU64,
    pub own: AtomicU64,
    pub others: AtomicU64,
    pub tlab: AtomicU64,
    pub own_only: AtomicU64,
    pub others_only: AtomicU64,
    pub others_only_young: AtomicU64,
    pub nonobj_only: AtomicU64,
    pub xt_words: AtomicU64,
    pub xt: AtomicU64,
}

impl G1PinAttribution {
    pub fn add(&self, row: &G1PinAttributionRow) {
        let add = |c: &AtomicU64, v: u64| {
            c.fetch_add(v, Ordering::Relaxed);
        };
        add(&self.pauses, 1);
        add(&self.regions, row.regions);
        add(&self.young, row.young);
        add(&self.bytes, row.bytes);
        add(&self.own, row.own);
        add(&self.others, row.others);
        add(&self.tlab, row.tlab);
        add(&self.own_only, row.own_only);
        add(&self.others_only, row.others_only);
        add(&self.others_only_young, row.others_only_young);
        add(&self.nonobj_only, row.nonobj_only);
        add(&self.xt_words, row.xt_words);
        add(&self.xt, row.xt);
    }

    pub fn line(&self) -> Option<String> {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        if g(&self.pauses) == 0 {
            return None;
        }
        Some(format!(
            "[GC] g1 pin-attribution: pauses={} pinned_regions={} pinned_young={} pinned_bytes={} by_own={} by_others={} by_tlab={} own_only={} others_only={} others_only_young={} nonobj_only={} xt_words={} by_xt={} (region-pauses summed over the attributed young pauses; own = the collecting thread's published words, others = other threads' and this cycle's helper windows', xt = this cycle's cross-thread pins alone)",
            g(&self.pauses),
            g(&self.regions),
            g(&self.young),
            g(&self.bytes),
            g(&self.own),
            g(&self.others),
            g(&self.tlab),
            g(&self.own_only),
            g(&self.others_only),
            g(&self.others_only_young),
            g(&self.nonobj_only),
            g(&self.xt_words),
            g(&self.xt),
        ))
    }
}

/// gci i9/fa -- may [`G1Collector::drop_clean_pinned_from_fixup`] run at all?
/// Only with its switch (`CRATONVM_G1_FIXUP_SKIP_CLEAN_PINNED=1`) AND on the
/// premise its soundness argument rests on: Phase 2's card screen armed for
/// pinned regions (`CRATONVM_G1_CARD_RSET`, `CRATONVM_G1_CARD_SCREEN_JIT_PINNED`,
/// both default ON) and cards cleared only on recycle (`CRATONVM_G1_CARD_CLEAN`
/// off, its default). Pure, so both arms are testable.
pub fn fixup_skip_clean_pinned_armed(
    switch_on: bool,
    card_rset: bool,
    screen_jit_pinned: bool,
    card_clean: bool,
) -> bool {
    switch_on && card_rset && screen_jit_pinned && !card_clean
}

/// gci i9/fa -- the per-region half of the same rule, before the card read:
/// not JNI-pinned, non-empty, unchanged by this pause (type and cursor equal
/// to the pre-evacuation snapshot), and an ordinary Eden / Survivor / Old
/// region.
pub fn fixup_skip_region_eligible(
    ty: RegionType,
    was_type: RegionType,
    cursor: usize,
    was_cursor: usize,
    jni_pinned: bool,
) -> bool {
    !jni_pinned
        && cursor > 0
        && ty == was_type
        && cursor == was_cursor
        && matches!(ty, RegionType::Eden | RegionType::Survivor | RegionType::Old)
}

/// gci i15/rs -- `CRATONVM_G1_YOUNG_CARD_SCREEN=1` (default on): the young
/// pause's card-granular remembered-set scan and fix-up. Read per source walk
/// and per fix-up (`runtime_flag_on`), never cached in a static.
pub fn young_card_screen_switch() -> bool {
    true
}

/// gci i15/rs -- may the young card screen run at all? Only with its switch
/// AND on the premise it shares with Phase 2's card screen: the card READ side
/// armed (`CRATONVM_G1_CARD_RSET`, default ON) and cards never cleaned by
/// Phase 2 (`CRATONVM_G1_CARD_CLEAN` off, its default -- the cleaner writes
/// only CLEAN / DIRTY and owns the same bytes). Pure, so both arms are
/// testable.
pub fn young_card_screen_armed(switch_on: bool, card_rset: bool, card_clean: bool) -> bool {
    switch_on && card_rset && !card_clean
}

/// gci i15/rs -- does a reference into a region of type `ty` (`in_cset`: one
/// of this pause's collection-set regions) keep its holder's card YOUNG-dirty?
/// Yes for every region a later young pause may collect or reach through the
/// young table: Eden, Survivor, a Free region (a stale reference into a region
/// that may become Eden; conservative) and any collection-set member. Old and
/// humongous targets do not: the mixed screen, eager humongous reclaim and
/// the census read the sticky `!= CLEAN` table, which demotion never cleans.
/// Pure.
pub fn young_card_target(ty: RegionType, in_cset: bool) -> bool {
    in_cset || matches!(ty, RegionType::Eden | RegionType::Survivor | RegionType::Free)
}

/// gci i15/rs -- is every member of `cset` a young (Eden / Survivor) region?
/// Only then does "no young-dirty card" mean "no reference into the
/// collection set". `type_of` answers a region's type (`None`: out of range,
/// which fails the test). An empty set passes (there is nothing to find).
pub fn cset_is_young_only(
    cset: &RegionSet,
    mut type_of: impl FnMut(usize) -> Option<RegionType>,
) -> bool {
    cset.iter()
        .all(|i| matches!(type_of(i), Some(RegionType::Eden | RegionType::Survivor)))
}

/// gci i10/pp -- what one region of the Phase-4 walk covered
/// ([`G1Collector::fixup_walk_region`]): whether it was walked at all (a
/// Free or CSet index is not), its cursor in bytes, and whether the walk broke
/// off before the cursor.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FixupRegionWalk {
    pub walked: bool,
    pub bytes: u64,
    pub aborted: bool,
}

/// gci i10/pp -- the pool helpers a parallel Phase-4 walk dispatches: the
/// collector's workers minus the driver, clamped to the helpers the pool has
/// and to one fewer than the regions to walk (the driver takes one). Pure.
pub fn phase4_fixup_helpers(workers: usize, pool_helpers: usize, regions_to_walk: usize) -> usize {
    workers
        .saturating_sub(1)
        .min(pool_helpers)
        .min(regions_to_walk.saturating_sub(1))
}

/// gci i10/pp (`CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR=1`) -- does a pin word at
/// `addr` name no byte ever allocated in its region (`base`, `cursor`, type
/// `ty`)? Then no object lives there for the word to hold, and its region need
/// not leave the collection set for it.
///
/// Strictly ABOVE `base + cursor`: the one-past-the-end address of the
/// region's last object equals `base + cursor`, and a derived end pointer of
/// that object is exactly the word a pin must keep. Ordinary Eden / Survivor /
/// Old regions only: a humongous start's cursor is the whole span and a
/// continuation's bytes are all live; a Free region is never in a collection
/// set, so its pin changes nothing this rule needs to argue about. Pure.
pub fn pin_word_names_no_allocation(ty: RegionType, base: usize, cursor: usize, addr: usize) -> bool {
    matches!(ty, RegionType::Eden | RegionType::Survivor | RegionType::Old)
        && addr > base.saturating_add(cursor)
}

/// gci i9/fa -- attribute each region of `pinned` to the three publishers,
/// given the region of each kept word of each publisher. Pure, so the
/// counting rule is testable without a collector.
pub fn attribute_pin_regions(
    pinned: &RegionSet,
    young_of: impl Fn(usize) -> Option<(bool, u64)>,
    own: &RegionSet,
    others: &RegionSet,
    tlab: &RegionSet,
) -> G1PinAttributionRow {
    let mut row = G1PinAttributionRow::default();
    for idx in pinned.iter() {
        let Some((young, occupancy)) = young_of(idx) else {
            continue;
        };
        row.regions += 1;
        row.bytes += occupancy;
        if young {
            row.young += 1;
        }
        let (o, t, x) = (own.contains(&idx), others.contains(&idx), tlab.contains(&idx));
        row.own += u64::from(o);
        row.others += u64::from(t);
        row.tlab += u64::from(x);
        match (o, t, x) {
            (true, false, false) => row.own_only += 1,
            (false, true, false) => {
                row.others_only += 1;
                if young {
                    row.others_only_young += 1;
                }
            }
            (false, false, false) => row.nonobj_only += 1,
            _ => {}
        }
    }
    row
}

/// gci i24/jp -- how many regions of `pinned` a publisher's region set
/// claims (the `by_xt` column). Pure.
pub fn pinned_regions_claimed_by(pinned: &RegionSet, claims: &RegionSet) -> u64 {
    pinned.iter().filter(|idx| claims.contains(idx)).count() as u64
}


/// Percentile reduction of the recorded pauses, split by collection type
/// (the §5 acceptance metric). All durations are microseconds.
#[derive(Debug, Clone, Default)]
pub struct G1PausePercentiles {
    /// Number of collections of this type in the (bounded) history.
    pub count: u64,
    /// Total pause across the recorded collections.
    pub total_us: u64,
    /// Median (50th percentile) pause.
    pub p50_us: u64,
    /// 99th percentile pause.
    pub p99_us: u64,
    /// Worst observed pause.
    pub max_us: u64,
}

/// Aggregate pause summary for a run: young + mixed percentiles plus the
/// number of records dropped from the bounded ring (so a long soak's summary
/// never silently under-reports — see `PAUSE_HISTORY_CAP`).
#[derive(Debug, Clone, Default)]
pub struct G1PauseSummary {
    /// Young-only collection percentiles.
    pub young: G1PausePercentiles,
    /// Mixed collection percentiles.
    pub mixed: G1PausePercentiles,
    /// Records evicted from the bounded ring before this summary was taken.
    pub dropped: u64,
    /// LANE W3-C — evacuation-failure DRAIN PASSES in the ring, and their
    /// total cost. Excluded from [`Self::young`] because they are not pauses:
    /// each one runs inside a young pause that has already recorded itself.
    ///
    /// Reported rather than dropped, because a drain pass is real work inside
    /// a real stop-the-world window and a summary that simply deleted them
    /// would under-report what the world spent stopped. Read `young` for "how
    /// long is a pause" and this for "how much of that went on recovering from
    /// evacuation failure". See [`G1PausePhases::drain_pass`].
    pub drain_passes: u64,
    /// Microseconds summed over [`Self::drain_passes`].
    pub drain_pass_us: u64,
}

// ---------------------------------------------------------------------------
// G1 Collector
// ---------------------------------------------------------------------------

// G1 garbage collector implementing the `GarbageCollector` trait.
// ---------------------------------------------------------------------------
// `is_object_address` census -- CRATONVM_DBG_G1_OBJADDR=1
// ---------------------------------------------------------------------------

/// Calls, acceptances and nanoseconds inside [`G1Collector::is_object_address`].
///
/// # What this is for
///
/// `gen_heap`'s sibling predicate got an exact object-start bitmap because it
/// was ~180 lines of header-shaped deduction with an extent computation and two
/// commit probes in it. The obvious follow-up -- give G1 one too -- is NOT the
/// same trade, and the reason is measurable rather than arguable: G1 does not
/// allocate through `Arena`, so it has no bitmap to consult and would need one
/// maintained at its own chokepoint; and its predicate is already the cheaper
/// of the two, with no extent arithmetic and no second commit probe, so the
/// expensive half a bitmap replaces in `gen_heap` is not there to replace.
///
/// Whether the cheap half is worth replacing is a number, and this is the
/// number: the share of a G1 run spent inside this function at all. Off by
/// default -- two `Instant::now()` calls around a function whose body is an
/// alignment test and two byte loads would dominate what it measures, which is
/// why the reading is a share of a run rather than a per-call cost.
pub static G1_OBJADDR_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static G1_OBJADDR_ACCEPTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static G1_OBJADDR_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[inline]
pub fn g1_objaddr_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

/// `(calls, accepted, nanos)` -- see [`G1_OBJADDR_CALLS`].
pub fn g1_object_address_census() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering;
    (
        G1_OBJADDR_CALLS.load(Ordering::Relaxed),
        G1_OBJADDR_ACCEPTED.load(Ordering::Relaxed),
        G1_OBJADDR_NANOS.load(Ordering::Relaxed),
    )
}

/// `(used_bytes, regions)` per region family, from ONE walk of the region
/// table under one read lock — so the families partition the table and the
/// sums are a snapshot, not several racing samples (gc-common w4-f,
/// `docs/feature-designs/concurrent-gc-maturation.md`).
/// Humongous = start + continuation regions; a start region's cursor spans
/// the whole object and a
/// continuation's is `0` (see `claim_humongous_span_locked`), so the object's
/// bytes are counted once while every region of the span is counted. `free` is
/// a region count. See [`G1Collector::region_census`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct G1RegionCensus {
    pub eden: (usize, usize),
    pub survivor: (usize, usize),
    pub old: (usize, usize),
    pub humongous: (usize, usize),
    pub free: usize,
    pub region_size: usize,
}

/// The least garbage (bytes) an Old region must hold to be a full-compaction
/// candidate: an eighth of a region for the ordinary full compaction, one byte
/// for the OOME path's maximal pass (JIT round 22 wave 11, lane memory;
/// [`G1Collector::arm_full_compaction_maximal`]). Pure.
pub fn full_compaction_min_garbage(region_size: usize, maximal: bool) -> usize {
    if maximal {
        1
    } else {
        region_size / 8
    }
}

