#[allow(unused_imports)]
use super::*;

pub struct G1Collector {
    /// Pre-forward mark words of objects this pause SELF-forwarded (evacuation
    /// failure), restored by `retire_forwards`. A self-forward keeps only the
    /// quartet, and a compact instance's identity hash and any thin lock live
    /// in the bits it drops. Empty outside a failing pause.
    pub self_forward_marks: parking_lot::Mutex<Vec<(usize, u32)>>,
    /// r23 w6: while a `collect_garbage` call is running, every self-forward
    /// (both evacuators) bumps [`Self::self_forward_count`], so the pause's
    /// identity scans ([`Self::self_forwarded_regions`], the retry's seeds)
    /// can answer "none" without walking the whole forwarding map. Off
    /// outside a pause (a helper called directly scans as before).
    pub self_forward_tracking: AtomicBool,
    pub self_forward_count: AtomicUsize,
    /// g1pin: the addresses this collection self-forwarded, in the order made
    /// (both evacuators; [`Self::note_self_forward`]). With pins every pause
    /// self-forwards, and the identity questions ([`Self::identity_keys`])
    /// read this instead of the whole forwarding map.
    pub self_forwarded_addrs: parking_lot::Mutex<Vec<usize>>,
    /// g1pin: this collection's per-object peer-stack pins, sorted object
    /// starts ([`g1_pins_peer_stack_objects`]). Set by the pause drivers' pin
    /// set, cleared at both ends of `collect_garbage`. The count mirrors the
    /// length, so an evacuation asks the lock only while a pin exists.
    pub peer_object_pins: parking_lot::RwLock<Vec<usize>>,
    pub peer_object_pin_count: AtomicUsize,
    /// g1pin: the kept CSet regions this collection filled around their pinned
    /// objects (`fill_around_kept_pins`). Every other object of such a region
    /// moved or died and its header is gone, so the pinned-referent verdict
    /// reads a candidate there that is no pinned object as dead
    /// ([`Self::pinned_young_referents_unheld`]). Reset by
    /// `reset_refproc_kept_young_token`.
    pub pin_filled_regions: parking_lot::Mutex<RegionSet>,
    /// The collection set being processed by the parallel evacuator, if any.
    /// A conservative root screen may run while workers own this set; it must
    /// reject an in-progress forwarding claim before any shape accessor can
    /// resolve its target and wait on `MARK_FWD_BUSY`.
    pub active_evacuation_cset: RwLock<RegionSet>,
    /// Fast gate for [`Self::active_evacuation_cset`]. Outside a parallel
    /// evacuation the root-screen hot path pays one atomic load and no lock.
    pub active_evacuation: AtomicBool,
    /// Compact-layout domain of the VM that owns this heap. See
    /// `Heap::set_layout_domain`: `class_id` is a per-`ClassStore` index, so
    /// allocating against another domain's registry entry would give the object
    /// a foreign shape. Defaults to the first domain, so an untold heap behaves
    /// as it did before domains existed.
    pub layout_domain: std::sync::atomic::AtomicU32,

    /// Collector configuration.
    pub config: G1CollectorConfig,
    /// Single contiguous backing store for every region.
    ///
    /// All `num_regions` regions are carved as adjacent `region_size` slices
    /// of this one allocation (region `i` lives at `arena_base + i*region_size`).
    /// This makes a humongous object spanning a contiguous run of region
    /// indices one physically-contiguous block, so the JIT's flat
    /// `base + HEADER_SIZE + i*stride` array addressing (and JNI-critical /
    /// Unsafe / arraycopy raw pointers) address every element correctly. The
    /// previous design gave each region its own `Vec<u8>`, so a humongous
    /// element past the first region's payload read/wrote unrelated heap
    /// memory (silent zero tail → SIGSEGV). Allocated once in
    /// [`G1Collector::new`]; never moved or reallocated, so every region base
    /// and the `region_lookup` table stay address-stable for the collector's
    /// lifetime. `Box<[u8]>` (not `Vec`) to make the no-realloc contract
    /// explicit. Dropped with the collector — no leak.
    #[allow(dead_code)]
    pub arena: cratonvm_gc_core::heap_reservation::ReservedHeap,
    /// All heap regions.
    ///
    /// # F-10 — why this is an `RwLock` and not a `Mutex`
    ///
    /// It was a single `parking_lot::Mutex<Vec<G1Region>>` covering three
    /// unrelated jobs at once: mutator allocation (`alloc_in_region`,
    /// `refill_tlab`), the whole stop-the-world pause, and — via
    /// `concurrent_mark_step` — the CONCURRENT marker, which acquired it and
    /// then held it for a whole 256-object step. The third one is what makes
    /// the name wrong: a marker that takes the same exclusive lock as the
    /// allocator is not running concurrently with the mutators, it is taking
    /// TURNS with them, and it does so at exactly the point in the cycle where
    /// the heap is filling fastest (the marker is only started because
    /// occupancy crossed IHOP).
    ///
    /// The reader/writer split follows the actual mutation pattern, which the
    /// `Vec` has always had and nothing exploited:
    ///
    /// * The `Vec` itself is built once in [`G1Collector::new`] and NEVER
    ///   resized. That is already load-bearing elsewhere — `region_lookup`,
    ///   the `*const G1Region` in the write barrier's TLS cache, and
    ///   [`RegionsBase`] all rest on it — so "a reader may hold a `&G1Region`
    ///   while another thread allocates" needs no new argument.
    /// * What actually changes is PER-REGION state, and the fields the hot
    ///   paths touch are already interior-mutable and thread-safe on their own:
    ///   `mark_bitmap` (atomic words), `marked_bytes_below_tams`
    ///   (`AtomicUsize`), `rset` (its own mutex), and — since F-11 — `cursor`
    ///   (`AtomicUsize`), and — since W3-A — `region_type` (`AtomicU8`),
    ///   `age` (`AtomicU8`) and `recycled_in_generation` (`AtomicU64`).
    /// * The fields that are NOT interior-mutable — `pinned`, `pin_count`,
    ///   `live_bytes`, `gc_efficiency`, `reuse_epoch`, `mark_start` — are
    ///   written only by phases that already bound the guard as `mut`. Those
    ///   become `write()` and keep exactly the exclusion they had.
    ///
    ///   W3-A audited that list against a second exclusion mechanism that this
    ///   note did not cover: the parallel evacuator, which holds the guard in
    ///   the DRIVER and then reaches the regions through [`RegionsBase`], so
    ///   the guard excludes nobody among the workers. Of the seven, only
    ///   `region_type` and `age` were written by a worker (in
    ///   `SharedEvac::tlab_alloc`), which is why only those two became atomic.
    ///   The rest are read by workers — `pinned` and `reuse_epoch` in
    ///   `describe_rejected_address_view`'s diagnostic line, `live_bytes`
    ///   nowhere on that path — but their sole writers
    ///   (`pin_region`/`unpin_region`, `reset`, `cleanup`'s liveness pass,
    ///   `start_concurrent_mark`) all run either before the workers are spawned
    ///   or after `std::thread::scope` has joined them, so there is no
    ///   concurrent writer to race with. That is the invariant a future change
    ///   would break: writing any of them from inside the parallel scope
    ///   reintroduces exactly the defect W3-A closed.
    ///
    ///   `recycled_in_generation` became atomic for a DIFFERENT concurrency
    ///   vector, and it is worth naming because this note is the place someone
    ///   will look: not the evacuation workers, but the write barrier's
    ///   lock-free TLS fast path, which holds neither guard. See the field's
    ///   own doc comment.
    ///
    /// So the conversion is mechanical and semantics-preserving at every site:
    /// a guard that was bound `let mut` became `write()`, one bound `let`
    /// became `read()`. The only behaviour change is that two `read()` holders
    /// now run at the same time, which is sound precisely because a `read()`
    /// site cannot reach `DerefMut` — the compiler, not a comment, is what
    /// checks that here.
    ///
    /// **What would falsify this**: a `&G1Region` reader that mutates through
    /// interior mutability in a way that is not itself thread-safe. Adding a
    /// `Cell`/`RefCell` field to `G1Region`, or handing out `&mut` from `&`
    /// through `unsafe`, breaks the argument; `RegionBuf`'s `DerefMut` (which
    /// yields `&mut [u8]`) is deliberately reachable only from
    /// `&mut G1Region`.
    ///
    /// **Lock discipline** (unchanged in kind, restated because the failure
    /// mode has a new shape): this lock is NOT reentrant and `parking_lot`'s
    /// `RwLock` is task-fair, so a waiting writer blocks new readers. Never
    /// take it twice on one thread, and never "upgrade" by taking `write()`
    /// inside a `read()` scope — drop the read guard first. The `Mutex` this
    /// replaced had the same rule and is the proof no existing site violates
    /// it: a nested acquisition would already have hung.
    pub regions: RwLock<Vec<G1Region>>,

    /// INT-3 — published un-retired TLAB tails of threads the cross-thread
    /// STW JIT takeover froze mid-JIT (plus any blocked/tearing-down thread
    /// that missed its safepoint retire), as absolute `(cursor, end)`
    /// address ranges.
    ///
    /// A frozen peer never reached `Tlab::retire`, so its reserved tail is
    /// uninitialized memory *below* its Eden region's `cursor` carrying no
    /// walkable int[]/gap filler. Every linear region walker must skip these
    /// spans (see [`jit_tlab_skip_span_len`]), and
    /// [`Self::jit_pinned_region_set_screened`] excludes the containing regions from
    /// the CSet — the owner resumes bump-allocating into `[cursor, end)`
    /// after the pause, so the region must survive the collection in place.
    /// Set under STW immediately before a collection and cleared immediately
    /// after (empty on every normal cycle) — the G1 counterpart of
    /// [`cratonvm_gc_core::jit_bounds::GenerationalHeap::set_jit_tlab_skip_regions`].
    pub jit_tlab_skip_regions: Mutex<Vec<(usize, usize)>>,
    /// Rotating start region for the BUDGETED post-evacuation CSet
    /// verification (audit §9 item 2). Only read/written by that pass, which
    /// runs on the collecting thread under the regions lock, so `Relaxed` is
    /// the whole ordering requirement: a torn or stale value costs coverage
    /// fairness for one pause, never correctness.
    ///
    /// gci i29/cv2 — that last sentence stopped being true when Lane B packed
    /// a BYTE OFFSET into this word: an offset is only an object start in the
    /// incarnation of the region it was recorded in. See
    /// [`Self::cset_verify_resume_stamp`].
    pub cset_verify_cursor: AtomicUsize,
    /// gci i29/cv2 (gci-i26cv) — the incarnation stamp of the region the
    /// packed resume offset in [`Self::cset_verify_cursor`] belongs to:
    /// `cset_verify_resume_stamp_of(region)` at the moment the offset was
    /// stored. The budgeted verifier resumes at that offset only while the
    /// region's stamp is unchanged; a region that was reset (cleanup freed it,
    /// a pause evacuated it) and refilled since is walked from the top.
    ///
    /// Without it the resume landed inside an object of the region's NEW
    /// grid, and the verifier's legacy 16-byte walk strode a live compact
    /// object's body as `Value` cells: the `corrupt Value cell` bursts of
    /// gci-i26cv (`caller=` the verifier's walk, holder `grid=INTERIOR` of a
    /// region whose grid closes on its cursor, first Mixed pause right after a
    /// cleanup that freed ~90 regions). Same ownership and ordering as the
    /// cursor (collecting thread, regions lock, `Relaxed`).
    pub cset_verify_resume_stamp: AtomicU64,
    /// gci i29/cv2 — the flagless object starts the previous
    /// `CRATONVM_G1_DBG_FLAGLESS_SWEEP` sweep found
    /// ([`Self::flagless_header_sweep`]). Empty and never locked unless the
    /// switch is on.
    pub flagless_sweep_prev: Mutex<FxHashSet<usize>>,

    /// Adaptive `needs_gc` Free-fraction threshold, in percent (baseline 25).
    /// Raised (up to 50) after any collection that recorded an evacuation
    /// failure — the free pool at trigger time is the young collection's
    /// ENTIRE to-space, so a live-young set larger than 25% of the heap
    /// otherwise self-forwards en masse on every single pause (the retry
    /// loop then drains the keeps, but at the cost of re-copying the live
    /// set several times per pause). Decays back to the baseline after a
    /// clean collection so healthy workloads keep the larger eden.
    pub needs_gc_free_percent: AtomicUsize,

    /// Predicted fraction of the young generation a young pause copies, in
    /// permille. Starts at 1000 (every young byte survives) and follows the
    /// measured `bytes_copied / young bytes` of each pause: up at once, down
    /// by a quarter of the gap. Read by the evacuation-headroom half of
    /// [`GarbageCollector::needs_gc`]; see [`g1_evac_headroom_trigger`].
    pub young_survival_permille: AtomicUsize,

    /// The Eden bytes the low-Free triggers wait for after a FUTILE pause --
    /// one that freed less than a region -- or 0 after a productive one. See
    /// [`Self::low_free_trigger_allowed`].
    pub low_free_trigger_floor: AtomicUsize,

    /// Eden bytes handed out since the last pause: TLAB carves and out-of-TLAB
    /// Eden objects, NOT objects bumped inside a TLAB (the carve already
    /// counted them). Reset at the end of every pause. Read only by
    /// [`Self::low_free_trigger_allowed`].
    pub eden_bytes_since_pause: AtomicUsize,

    /// Cached count of `RegionType::Free` regions — the answer
    /// [`GarbageCollector::needs_gc`] needs, kept as an atomic so that query
    /// costs one relaxed load instead of `regions.lock()` plus an
    /// O(num_regions) scan.
    ///
    /// `maybe_gc` runs `needs_gc()` after EVERY `new` / `newarray` /
    /// `anewarray` bytecode, so the scan it used to do ran once per allocated
    /// object over every region in the heap — 1500 regions at `-Xmx1500m`,
    /// under the one mutex every allocation path also needs. Single-threaded
    /// that is ~1 us of pure overhead per object; with several mutators it is
    /// a convoy, and the same total allocation work measured 1.5 s on one
    /// thread against 13.4 s on eight (ZGC, whose trigger is O(1), was flat at
    /// 0.8 s for both). `note_region_consumed_locked` already documents the
    /// discipline this violated: count Free regions when a region is
    /// CONSUMED, never per object.
    ///
    /// Refreshed wherever the count is already being computed under the lock
    /// (region consumption, TLAB refill, the end of a collection) and, as a
    /// self-healing backstop for any region-type transition that does neither,
    /// re-scanned every [`NEEDS_GC_RECOUNT_INTERVAL`] queries per mutator.
    /// `CRATONVM_DBG_G1ACCESSOR` census: field/array accessor calls that
    /// answered from [`Self::may_be_humongous`] with NO `regions` lock, and
    /// calls that had to take it. The load-independent half of the
    /// accessor-lock measurement — see the flag's own doc.
    pub accessor_calls_lockfree: AtomicU64,
    pub accessor_calls_locked: AtomicU64,
    /// Same census, remembered-set half: cross-region reference stores whose
    /// edge the per-thread memo had already recorded (no lock), against those
    /// that had to reach `RememberedSet::add_reference_in_generation` and take
    /// its per-region mutex. `rset_edge_same_region` counts the stores that
    /// never needed an entry at all.
    pub rset_edge_memo_hit: AtomicU64,
    pub rset_edge_recorded: AtomicU64,
    pub rset_edge_same_region: AtomicU64,
    /// W3-B — how many times the remembered-set write barrier's LOCK-FREE fast
    /// path observed [`Self::rset_cache_epoch`] move across its own insert, and
    /// therefore redid that insert down the regions-lock slow path.
    ///
    /// This is the counter for the window filed as
    /// `lane-a-rset-tls-fast-path-epoch-window`, and it is counted rather than
    /// asserted because the window is only reachable during a pause: a
    /// `debug_assert!` here would abort inside a GC pause, which manufactures a
    /// worse bug than it reports.
    ///
    /// **Unconditional**, not gated on `g1_dbg_accessor`, because unlike the
    /// three counters above it is not a census of a fast path — it is the
    /// frequency of a correctness fail-safe firing, and a fail-safe whose
    /// frequency is only observable under a debug flag is a fail-safe nobody
    /// will ever see fire. A relaxed increment on a path that has just decided
    /// to take a lock costs nothing measurable.
    pub rset_fast_path_epoch_raced: AtomicU64,
    pub free_region_count: AtomicUsize,

    /// Native-allocation pressure latch — G1's half of the "the allocation
    /// wrappers cannot collect, so the next `safe_native_call` boundary does
    /// it for them" protocol. The generational half is
    /// [`cratonvm_gc_core::jit_bounds::GenerationalHeap::young_spill_pressure`]; the shared
    /// consumer is `vm_exec::safe_native_call_impl`, the one point on the
    /// native-dispatch path where every Java argument is pinned in
    /// `native_pin_roots` and remapped afterwards, so an orchestrated moving
    /// collection is safe there.
    ///
    /// Latched whenever a NEW region is claimed for Eden (from
    /// [`Self::alloc_in_region`] or [`Self::refill_tlab`]) and the remaining
    /// Free pool has fallen below the `needs_gc` threshold; cleared by the
    /// consumer and at the end of every collection.
    ///
    /// Why this exists: the interpreter's `maybe_gc()` — the only thing that
    /// ever asks G1 to collect during normal execution — is polled from the
    /// bytecode allocation instructions (`new`/`newarray`/`anewarray`/
    /// `multianewarray`) alone. A hot loop whose body allocates only from
    /// INSIDE native methods (no allocation bytecode of its own — e.g.
    /// `AsynchronousFileChannel.read`, whose `Integer` box is allocated on the
    /// Rust side by `afc_box_integer`) therefore runs for millions of
    /// iterations with ZERO safepoint checks. Eden grows to 100% of the heap
    /// and the infallible [`GarbageCollector::alloc_object`] entry point —
    /// which has no way to report failure and so cannot retry after a GC —
    /// aborts the process with "out of heap space" while the heap is almost
    /// entirely garbage. The generational collector hides the same gap behind
    /// its young→old spill fallback (`gen_heap::alloc_object`); G1 has no
    /// equivalent, so it aborted outright. See
    /// `g1-native-alloc-no-safepoint-oom-FIXED.md`.
    pub native_alloc_pressure: AtomicBool,
    pub external_gpu: cratonvm_gc_core::gpu_critical::ExternalGpuPressureTracker,
    /// JIT round 23 wave 7, lane L4: set by [`Self::note_native_alloc_pressure`]
    /// alone (an explicit recovery request from outside the collector, e.g. the
    /// VM's allocation-failure ladder), consumed by the next young pause, which
    /// then takes the serial evacuator (`CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH`).
    pub serial_recovery_requested: AtomicBool,
    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`):
    /// consecutive young pauses that ended, while a concurrent mark cycle was
    /// open, with the Free pool under [`FUTILE_YOUNG_FREE_PCT`] of the heap's
    /// regions. Reset by any other young pause and by
    /// [`Self::take_futile_young_escalation`]. See
    /// [`Self::note_futile_young_pause`].
    pub futile_young_streak: AtomicUsize,
    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_SERIAL_FORWARD_PRESIZE`):
    /// how many forwards the last serial young or mixed pause recorded; the
    /// next one pre-sizes its `PointerMap` from it. Per collector, a capacity
    /// hint only.
    pub serial_forward_hint: AtomicUsize,
    /// g1ep: the parallel evacuator's forward lists, kept between pauses with
    /// their capacity ([`Self::take_forward_buffer`]). Each pause used to
    /// start every worker's list empty and grow it by reallocation, so every
    /// pause faulted in fresh zeroed pages for some tens of megabytes of
    /// pairs: the kernel's page clearing was 1.8% of `G1epKeepRounds 10`
    /// CPU, two thirds of it under those reallocations.
    pub forward_buffers: Mutex<Vec<Vec<(usize, usize)>>>,
    /// g1ep4: the sorted pointer maps' pair and index buffers, kept between
    /// pauses (see [`cratonvm_types::pointer_map::SortedBufferPool`]). A map
    /// the pause builds from it hands its buffers back when the VM drops it,
    /// on the next pause's publish. At most a sixteenth of the reserved heap
    /// of each kind.
    pub sorted_buffers: Arc<cratonvm_types::pointer_map::SortedBufferPool>,
    /// How many escalations [`Self::take_futile_young_escalation`] granted.
    pub futile_young_escalations: AtomicU64,
    /// r23 w6 (`CRATONVM_G1_STARVED_FULL`): the next pause is the starved
    /// full's ([`Self::arm_starved_full`]); taken by `collect_garbage` with
    /// the full-compaction arming.
    pub starved_full_armed: AtomicBool,
    /// The pause in flight is a starved full: its old half takes no copying
    /// candidates and the in-place compaction runs whatever the Free pool.
    pub starved_full_this_pause: AtomicBool,
    /// An in-place-only full compaction is armed for the next pause
    /// ([`Self::arm_in_place_compaction`]); taken with the full-compaction
    /// arming, as the starved full's is.
    pub in_place_only_armed: AtomicBool,
    /// The pause in flight is an in-place-only full compaction: as a starved
    /// full's (no copying candidates, the in-place pass whatever the Free
    /// pool), without the starved full's back-off accounting.
    pub in_place_only_this_pause: AtomicBool,
    /// The R22 aastore-humongous page: the armed in-place-only compaction
    /// also takes the Free regions below its top movable region as slide
    /// destinations ([`Self::arm_fragmented_humongous_slide`]); taken with
    /// the in-place-only arming.
    pub in_place_fills_free_armed: AtomicBool,
    /// The pause in flight's in-place pass slides into Free regions too.
    pub in_place_fills_free_this_pause: AtomicBool,
    /// Region span of the failed humongous request the allocation-failure
    /// ladder's recovery pause consumed
    /// ([`Self::arm_fragmented_humongous_compaction`]), kept for the ladder's
    /// fragmentation slide; cleared by a served humongous claim.
    pub fragmented_slide_span: AtomicUsize,
    /// The futile-young streak the next starved full needs
    /// ([`Self::note_starved_full_outcome`]).
    pub starved_full_need: AtomicUsize,
    /// Consecutive young pauses that ended, with a cycle open, under
    /// [`STARVED_FULL_FREE_PCT`] percent of the regions Free
    /// ([`Self::take_starved_full`]).
    pub starved_young_streak: AtomicUsize,

    /// gci nd48: `allocated_bytes` when the TLAB reserve last refused a small
    /// fallible request ([`G1Collector::alloc_from_reclaim_credit`]);
    /// [`RESERVE_CREDIT_UNARMED`] while the pool is out of the reserve.
    pub reserve_credit_used: AtomicUsize,
    /// gci nd48: Free regions the reclaim credit has handed out since that
    /// refusal.
    pub reserve_credit_spent: AtomicUsize,
    /// The G1 near-full proxy-define page: armed by the VM's allocation
    /// ladder once its two full collections are done
    /// ([`Self::arm_reserve_last_chance`]); the next small fallible
    /// allocation at the reserve may claim a Free region while one stays
    /// ([`Self::alloc_from_reclaim_credit`]), and consumes it.
    pub reserve_last_chance: AtomicBool,
    pub reserve_last_chance_claims: AtomicU64,
    /// gci nd48: census -- credited claims, and refusals at the reserve.
    pub reserve_credit_claims: AtomicU64,
    pub reserve_credit_refusals: AtomicU64,

    /// Per-region `(reuse_epoch, cursor, region_type)` snapshot captured at
    /// concurrent-mark start (`start_concurrent_mark`, under the `regions`
    /// lock). The TAMS equivalent: `cleanup` treats every byte allocated
    /// after this snapshot — a grown cursor, a recycled (epoch-bumped) or
    /// re-typed region — as LIVE, because the mark bitmap carries no
    /// information about objects that did not exist when marking began.
    /// Without it, cleanup freed Old regions filled by promotion DURING the
    /// mark cycle (zero marked bytes), zeroing freshly-promoted live objects
    /// wholesale (SteadyChurn: live list nodes vanished the moment the first
    /// mark cycle completed under promotion pressure).
    pub mark_start_snapshot: Mutex<Vec<(u64, usize, RegionType)>>,

    /// Eden allocation slots: for each stripe, the region index it is
    /// currently bump-allocating into, or `usize::MAX` for "none yet".
    ///
    /// # F-11 — why this is a set and not a single index
    ///
    /// It was one `AtomicUsize`, so every allocating thread in the process
    /// bumped ONE region's cursor. Once F-11 let that bump happen under a
    /// shared guard, that single cursor became the bottleneck instead of the
    /// lock: N threads compare-exchanging one word, and — because objects are
    /// tens of bytes — writing the same cache lines as each other on the way
    /// out. Measured at 4 threads on a 512 MiB heap, a shared-guard allocation
    /// into ONE Eden region was about 1.9x SLOWER per object than the exclusive
    /// lock it replaced, consistently across interleaved runs. The exclusive
    /// arm wins that comparison because `parking_lot`'s mutex barges: one
    /// thread keeps re-acquiring and runs a long, cache-hot, uncontended burst.
    ///
    /// Removing the lock is therefore only half the change. This is the other
    /// half: a thread picks a stripe once and allocates from that stripe's own
    /// region, so the cursors are distinct words and the objects land in
    /// distinct pages. `CRATONVM_G1_EDEN_STRIPES=1` collapses it back to the
    /// single global Eden, which is both the bisection lever and the arm that
    /// reproduces the measurement above.
    ///
    /// The cost is fragmentation: each stripe holds a partially-filled region,
    /// so up to `stripes - 1` regions' worth of Eden is in flight and
    /// uncollected at any moment. [`G1Collector::eden_stripe_count`] therefore
    /// caps the count at an eighth of the heap's regions, which makes the
    /// default unit-test heap (8 regions) single-striped and unchanged.
    pub eden_slots: Box<[AtomicUsize]>,
    /// Next identity hash code to assign.
    pub next_hash_code: AtomicI32,

    /// Concurrent GC phase state.
    ///
    /// Round-2 fix (HIGH — GC #5): the previously-global `mark_bitmap`
    /// has moved to `G1Region::mark_bitmap` so each region's bitmap is
    /// keyed off that region's actual data pointer (not address 0).
    /// Callers route bitmap operations through the region lookup.
    pub gc_state: Arc<ConcurrentGcState>,
    /// Global SATB queue.
    pub satb_queue: Arc<SatbQueue>,

    /// Number of collections performed.
    pub collection_count: AtomicU64,
    /// gci i27/gr: `(collection_count << 1) | kept` of the last evacuation
    /// record of this `collect_garbage` (`kept`: the pause left a young region
    /// in place); `0` from the start of every `collect_garbage`. Arms the
    /// per-object referent verdict (`g1_pinned_referents.rs`).
    pub refproc_kept_young_token: AtomicU64,
    /// Young regions kept in place by the last evacuation pause (`(armed_by_pause, set)`).
    pub kept_young_regions: Mutex<(bool, RegionSet)>,
    /// gci i5g/g1o: the [`G1CollectionType::Mixed`] records among them (see
    /// [`Self::mixed_collection_count`]).
    pub mixed_collection_count: AtomicU64,
    /// Total pause time in **microseconds** across all collections. (The public
    /// `total_pause_ms()` accessor derives milliseconds from this; storing
    /// microseconds keeps sub-millisecond young pauses from rounding to zero.)
    pub total_pause_us: AtomicU64,
    /// Bounded ring of the most recent per-collection pause records
    /// (§7 item 6 — the structured pause sink that `pause_summary()` reduces to
    /// p50/p99). Capped at `PAUSE_HISTORY_CAP`; the oldest record is evicted
    /// when full and `pause_history_dropped` counts the evictions so a summary
    /// stays honest about coverage. Lock order: this is a leaf lock, never held
    /// across `regions.lock()`.
    pub pause_history: Mutex<VecDeque<G1PauseRecord>>,
    /// Records evicted from `pause_history` because the ring was full.
    pub pause_history_dropped: AtomicU64,

    /// Step 7 (pause-target CSet sizing) — rolling per-region copy-cost
    /// calibration: an EMA of observed evacuation cost in **nanoseconds per
    /// live byte copied**, refreshed from each *mixed* collection (the
    /// collections that actually evacuate old regions). `estimated_evac_cost_ns`
    /// multiplies a region's `live_bytes` by this to bound the mixed collection
    /// set against `max_gc_pause_ms`. Initialised to 4 ns/byte (~250 MB/s,
    /// matching `region::Region::estimated_evac_cost_ns`) and clamped positive.
    /// Relaxed: a statistics/scheduling signal, not a correctness guard.
    ///
    /// # LANE W5-A — what this term actually is, after wave 5
    ///
    /// Two things changed and the field doc above is kept because it is the
    /// history: under `CRATONVM_G1_EVAC_COST_REAL` this is the cost of the
    /// **closure phase** per live byte, not of the whole pause per live byte,
    /// and it is adopted WHOLE from its first real sample instead of being
    /// dragged there one eighth at a time from the constructor value. See
    /// [`G1Collector::fold_evac_cost`] and [`g1_evac_cost_real`].
    pub evac_ns_per_byte: AtomicU64,
    /// LANE W5-A — how many real samples `evac_ns_per_byte` has taken.
    ///
    /// The counter the four waves before this one did not have. `4` printed on
    /// a `[GC-STAT]` line is indistinguishable from a calibration that settled,
    /// and that is exactly how the defect survived: the constructor value is a
    /// plausible number. `evac_cost_samples=0` beside it says "never measured"
    /// in a way that cannot be misread, and it is also the switch that decides
    /// whether the next sample is adopted whole or smoothed.
    pub evac_cost_samples: AtomicU64,
    /// LANE W5-A — the YOUNG copy cost, nanoseconds per live byte copied out of
    /// an Eden or Survivor region.
    ///
    /// A SECOND term rather than more samples for the first one, because a
    /// young pause and an old evacuation are not drawing from the same
    /// distribution:
    ///
    ///  * a young CSet is copied out of contiguous bump-allocated Eden, where
    ///    the live objects the closure reaches are dense in address order and
    ///    the source region is warm from the mutator that just filled it;
    ///  * an old CSet region is *selected for being mostly garbage*
    ///    (`mixed_gc_live_threshold_percent`), so its live bytes are scattered
    ///    through a region whose cold remainder the copy still has to step
    ///    over, and the budget prices it by `live_bytes` — the numerator of the
    ///    sparse half, not the denominator of the walk;
    ///  * a young copy's destination is a Survivor region OR a promotion into
    ///    Old, chosen per object by the tenuring threshold; an old copy's is
    ///    always Old.
    ///
    /// Folding both into one term would let a young-heavy workload's cheap,
    /// dense copying set the price of the sparse old copying that the mixed
    /// pause goal is actually trying to bound. So they are measured apart, and
    /// the young term reaches the old budget only as a BOOTSTRAP — see
    /// [`G1Collector::old_evac_ns_per_byte`].
    ///
    /// `0` means "never measured", which is a state `evac_ns_per_byte` could
    /// not represent.
    pub young_evac_ns_per_byte: AtomicU64,
    /// LANE W5-A — sample count for [`Self::young_evac_ns_per_byte`].
    pub young_evac_cost_samples: AtomicU64,
    /// LANE W5-A — pauses whose `bytes_copied` was below
    /// [`G1_EVAC_COST_MIN_SAMPLE_BYTES`] and were therefore refused as
    /// per-byte cost samples, process-wide over both terms.
    ///
    /// The counted fail-safe. A `debug_assert!` here would fire inside a pause
    /// on any workload whose young pauses copy little (a humongous churn, for
    /// instance, where almost nothing in the CSet is copyable) and would
    /// manufacture a worse bug than it reports. A counter says the same thing
    /// at the end of the run and costs one relaxed add.
    pub evac_cost_samples_refused: AtomicU64,

    /// Current old-gen bytes (for IHOP tracking).
    pub old_gen_bytes: AtomicUsize,
    /// Byte threshold at which to initiate concurrent marking.
    pub marking_threshold_bytes: AtomicUsize,
    /// F-15 — `(when, old_gen_bytes)` at the last `start_concurrent_mark`, or
    /// `None` outside a cycle. The two measurements adaptive IHOP is built on —
    /// how long marking takes, and how fast the old generation grew while it
    /// ran — are both differences against this.
    pub mark_cycle_start: Mutex<Option<(std::time::Instant, usize)>>,
    /// F-15 — decaying estimate of concurrent-mark duration, milliseconds.
    /// Zero until a cycle has completed.
    pub mark_ms_ema: AtomicU64,
    /// F-15 — decaying estimate of old-generation growth during marking, in
    /// KiB per millisecond. KiB rather than bytes so the product with a
    /// duration cannot overflow on a long cycle. Zero until measured.
    pub alloc_rate_kib_per_ms: AtomicU64,
    /// F-15 — safety multiplier on the computed headroom, as a percentage.
    ///
    /// Starts at 100 (trust the measurement). Every pause that runs out of
    /// to-space raises it — that is the collector's own evidence that the last
    /// cycle started too late — and every cycle that completes without one
    /// decays it back. This is the feedback loop pause time used to stand in
    /// for, connected to the thing it is actually about.
    pub ihop_headroom_percent: AtomicU64,
    /// F-15 — how many pauses reported to-space exhaustion, i.e. how many times
    /// the threshold has been told it was too high. A gauge nobody can read is
    /// a gauge nobody can tune.
    pub ihop_late_events: AtomicU64,

    // --- lane W2-C state ---
    //
    // Added 2026-09-20 by wave 2 lane C (the marking cycle's policy and cost).
    // Every one of them is a measurement ABOUT a mark cycle that the cycle
    // itself could not previously carry, which is why they are fields rather
    // than locals. All are `Relaxed`: each is a scheduling/diagnostic signal
    // read on a path that already synchronises through the regions lock or the
    // phase store, and none gates memory safety.
    /// Old-generation occupancy sampled at the TOP of `cleanup`, i.e. BEFORE
    /// `recompute_old_gen_bytes` applies this cycle's in-place Old frees and
    /// dead-humongous reclaim. `usize::MAX` means "no cleanup has sampled one",
    /// which is what a `note_mark_cycle_end` driven outside `cleanup` sees.
    ///
    /// This is the number the adaptive IHOP model actually wants: how far the
    /// old generation rose while marking ran, which is what decides whether
    /// the heap fills before the closure completes. Freeing at the END of the
    /// cycle does not retroactively give the marker headroom it did not have,
    /// so the post-reclaim figure `note_mark_cycle_end` used to read folds
    /// `grew == 0` for any steady-state cycle. Consumed (and reset) by
    /// `note_mark_cycle_end` under `CRATONVM_G1_IHOP_GROSS_GROWTH`.
    pub mark_cycle_gross_old_bytes: AtomicUsize,
    /// Bytes the last completed cycle got back: the drop in `old_gen_bytes`
    /// across `cleanup`'s `recompute_old_gen_bytes`, or 0 if it rose. Written
    /// by `cleanup`, read by `check_ihop`.
    pub last_cycle_reclaimed_bytes: AtomicUsize,
    /// Old-generation occupancy published by the last `cleanup`, i.e. the heap
    /// the NEXT cycle would be given. Paired with
    /// `last_cycle_reclaimed_bytes` so `check_ihop` can ask "has anything
    /// changed since the cycle that found nothing?" rather than only "is the
    /// level high?".
    pub last_cycle_end_old_bytes: AtomicUsize,
    /// How many region-sizes of old-generation growth `check_ihop` currently
    /// requires on top of `last_cycle_end_old_bytes` before it will start
    /// another cycle. 1 after a cycle that reclaimed well, doubling (to
    /// `MARK_BACKOFF_MAX_REGIONS`) after each one that did not.
    ///
    /// A fixed one-region re-arm is not enough on its own: a workload that
    /// promotes a region's worth between cycles would re-fire the trigger
    /// every time while still reclaiming nothing, which is the same spin with
    /// one region of hysteresis in it. Widening the gate is what turns a
    /// futile cycle into evidence.
    pub mark_backoff_regions: AtomicUsize,
    /// LANE W5-C — `collection_count` as it stood when the last cycle's
    /// `cleanup` folded its outcome. The denominator of the back-off's
    /// deadline (`CRATONVM_G1_MARK_BACKOFF_DEADLINE`).
    ///
    /// The back-off's only re-arm is old-generation GROWTH, and growth is
    /// blind on exactly the axis a concurrent marker exists to cover. Old
    /// objects die when a mutator overwrites the last reference to them, and
    /// that event changes NOTHING the collector can see without marking: the
    /// bytes stay in their regions, `recompute_old_gen_bytes` reports the same
    /// occupancy, and `check_ihop`'s growth test reads "the heap has not
    /// changed" about a heap in which half the old generation just became
    /// garbage. That is not a tuning error, it is the one thing the gate
    /// structurally cannot observe, so the honest bound is a deadline rather
    /// than a better level test. See `check_ihop`.
    ///
    /// `u64::MAX` is the "no cycle has folded an outcome yet" sentinel, and it
    /// is `u64::MAX` rather than 0 because 0 is a perfectly legitimate value of
    /// `collection_count` — it is what a cycle driven before the first
    /// evacuation pause records, which is the ordinary case in a test and a
    /// reachable one in a process that marks early. A zero sentinel would read
    /// "unset" about a real reading and hold the deadline shut forever.
    /// `saturating_sub` against `u64::MAX` is 0, so the sentinel also fails the
    /// deadline test on its own, and the explicit check below is belt and
    /// braces rather than the mechanism.
    pub last_cycle_end_collections: AtomicU64,
    /// LANE W5-C — how many times the deadline above let a cycle through that
    /// the growth gate had refused.
    ///
    /// Counted for the reason every other number on the `[GC] g1 ihop:` line
    /// is counted: a fail-safe nobody can see is indistinguishable from one
    /// that never fires, and those are opposite diagnoses. Large here means
    /// the growth gate is systematically wrong on this workload; zero with the
    /// deadline armed means the gate was never holding anything back and the
    /// flag can be read as inert.
    pub mark_backoff_deadline_releases: AtomicU64,
    /// How many times `check_ihop` has declined a CONSULT the level test would
    /// have passed. A back-off nobody can see is indistinguishable from a
    /// collector that has stopped collecting, which is the failure this whole
    /// lane is about — so it is reported by `ihop_model_state`.
    ///
    /// **LANE W3-C corrected this doc.** It used to say "declined a CYCLE",
    /// and `w2c-what-landed-and-what-to-measure.md`'s A/B recipe reads it that
    /// way ("suppressions climb"). `check_ihop` is a poll from the allocation
    /// slow path and from the JIT mark driver, so a single deferred cycle
    /// produces one increment per consult for as long as it stays deferred —
    /// hundreds or thousands, depending only on the allocation rate. Reported
    /// as `backoff_declined_polls` with [`IHOP_POLLS`] as its denominator; see
    /// that static for the full argument.
    pub mark_backoff_suppressions: AtomicU64,
    /// LANE W6-B — the occupancy at which `check_ihop`'s answer could change,
    /// published so a repeat poll does not have to recompute it and, more to
    /// the point, does not have to WRITE to find out it is still being
    /// declined. Zero means "nothing published"; see `check_ihop`'s fast path
    /// and `G1Collector::backoff_fast_declines` (the field beside it).
    ///
    /// Written only on the slow decline path under
    /// `CRATONVM_G1_IHOP_POLL_GATE`, and CLEARED by
    /// `note_mark_cycle_outcome` on both arms of every flag — which is what
    /// makes the fast path exact rather than approximate. Every input to the
    /// decline condition (`last_cycle_reclaimed_bytes`,
    /// `last_cycle_end_old_bytes`, `mark_backoff_regions`) has exactly one
    /// writer, and that writer is `note_mark_cycle_outcome`. So while this is
    /// non-zero, the slow path's verdict on any occupancy below it is
    /// unchanged by construction, and the fast path is not an approximation of
    /// the gate: it is the gate, without the two read-modify-writes.
    pub backoff_gate_open_at_bytes: AtomicUsize,
    /// LANE W6-B — how many polls this collector declined WITHOUT a shared
    /// write, under `CRATONVM_G1_IHOP_POLL_GATE`. The engagement count for the
    /// fix: zero with the flag armed means the fast path never ran, so that
    /// arm's wall clock says nothing about it (§7.1 of
    /// `orchestrator-wave-1-measurements.md`).
    ///
    /// A SUBSET of `mark_backoff_suppressions`, which counts every decline
    /// however it was reached — so `suppressions - fast_declines` is how many
    /// declines still cost the two global read-modify-writes, and on a fixed
    /// run that is one per change of the gate's inputs rather than one per
    /// allocation.
    ///
    /// Published in batches; see [`IHOP_POLL_FLUSH_BATCH`] for the bound and
    /// `flush_gated_polls` for the escape hatch a test uses to be exact.
    pub backoff_fast_declines: AtomicU64,
    /// Decaying estimate of old-generation growth during marking, in BYTES per
    /// millisecond — the full-precision sibling of `alloc_rate_kib_per_ms`,
    /// which is now only the diagnostic. `recompute_marking_threshold` reads
    /// this one under `CRATONVM_G1_IHOP_GROSS_GROWTH`; see the divide-once note
    /// in `fold_mark_cycle_sample` for why the KiB form could not be the model
    /// input. Zero until measured.
    pub alloc_rate_bytes_per_ms: AtomicU64,
    /// How many mark cycles have closed with `grew == 0`, i.e. have told the
    /// adaptive model nothing about the allocation rate.
    ///
    /// `recompute_marking_threshold` responds to a zero rate by leaving the
    /// STATIC IHOP in place, which is a silent disengagement: the adaptive
    /// controller stops controlling and nothing in the process says so. This
    /// counter is that statement, and it is deliberately NOT behind the
    /// measurement flag — an operator has to be able to see the disengaged
    /// state on the default build in order to decide whether to flip it.
    pub ihop_disengaged_cycles: AtomicU64,
    /// F-18 — bytes of surviving objects by AGE, accumulated during the pause
    /// in progress and consumed at its end to re-derive the tenuring threshold.
    ///
    /// Indexed by the age the surviving COPY carries, so bucket `n` reads
    /// "bytes that have now survived `n` collections". Atomic because the
    /// parallel evacuator's workers fill it concurrently.
    pub survivor_age_bytes: [AtomicUsize; G1_MAX_TENURING_AGE],
    /// F-18 — the previous pause's `survivor_age_bytes`, kept for diagnostics
    /// because the live array is cleared at the end of every pause.
    pub last_survivor_age_bytes: [AtomicUsize; G1_MAX_TENURING_AGE],
    /// Bytes this collector has copied into OLD regions, cumulative across
    /// every evacuation pause — G1's half of `VmHeap::bytes_promoted_total`.
    ///
    /// # Why the metric needed this and could not derive it
    ///
    /// `interpreter::note_gc_productivity` grants a forced collection one
    /// credit for "promotion-only" work: a cycle that conserves live bytes but
    /// drained young into old did allocation-enabling work, and
    /// `GC_OVERHEAD_LIMIT_CYCLES` consecutive uncredited cycles throw
    /// `OutOfMemoryError: GC overhead limit exceeded`. G1 had no promoted-bytes
    /// counter at all, so the dispatcher answered a hard `0` and the one credit
    /// a G1 pause is most likely to earn — its pauses ARE young drains — was
    /// inexpressible on it
    /// (`docs/feature-designs/zgc-architecture.md`).
    ///
    /// It is counted at the copy rather than derived from `RegionType::Old`
    /// occupancy deltas, because an Old-region count moves for reasons that are
    /// not promotion — humongous allocation, cleanup reclaim, and mixed-CSet
    /// evacuation of Old into Old — so a derived figure would be wrong in both
    /// directions and, unlike a hard zero, would be trusted.
    ///
    /// Atomic and `u64`: the parallel evacuator's workers bump it concurrently,
    /// and it is cumulative over a process lifetime rather than per pause.
    pub bytes_promoted_total: AtomicU64,
    /// F-18 — the age at which an object is promoted rather than copied to
    /// survivor space, re-derived after every evacuation pause.
    ///
    /// Starts at `config.promotion_age`, which is also its ceiling: the
    /// adaptive rule may only tenure EARLIER than the operator asked, never
    /// later. Tenuring later than configured would keep objects circulating in
    /// the young generation past the point the operator sized it for.
    pub tenuring_threshold: AtomicU64,
    /// gci i22/sz -- the desired survivor size (bytes) the current
    /// `tenuring_threshold` was derived against (HotSpot's
    /// `desired_survivor_size`), and the pair the pause that last ran used,
    /// for its `gc,age` line ([`Self::tenuring_xlog_state`]). Statistics only.
    pub tenuring_desired_bytes: AtomicU64,
    pub last_pause_tenuring_desired_bytes: AtomicU64,
    pub last_pause_tenuring_threshold: AtomicU64,
    /// gci i22/sz -- a threshold was derived since the VM last took the
    /// `gc,age` facts ([`G1Collector::take_tenuring_xlog`]).
    pub tenuring_xlog_fresh: AtomicBool,
    /// gci i22/sz (`CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD`) -- `cleanup`'s
    /// mixed-candidate census: count, summed live bytes, and
    /// `mixed_old_regions_selected` when it was taken. See
    /// [`G1Collector::note_mixed_reserve_census`].
    pub mixed_reserve_candidates: AtomicUsize,
    pub mixed_reserve_live_bytes: AtomicU64,
    pub mixed_reserve_selected_base: AtomicU64,
    /// gci i22/sz (residual 3 of
    /// `gci-i20ys-g1-humongous-allocation-never-asks-for-a-concurrent-start-FIXED-20261016`,
    /// under `CRATONVM_G1_HUMONGOUS_IHOP_START`): a humongous request asked
    /// for a concurrent start ([`G1Collector::humongous_alloc_wants_concurrent_start`]
    /// answered yes). HotSpot's `G1 Humongous Allocation` collection IS the
    /// concurrent start (the cause requests it); ours runs a young pause and
    /// the mark drive after it tests IHOP WITHOUT the request, so at the exact
    /// crossing the cycle opened one request late. The mark gate and the
    /// pause label read this flag ([`G1Collector::humongous_conc_start_requested`],
    /// `would_start_marking`); a cycle start clears it.
    pub humongous_conc_start_requested: AtomicBool,
    /// gci i23/tn (i22/sz page item 5) -- HotSpot's `-XX:MaxTenuringThreshold`
    /// on G1: the ceiling and the starting value of `tenuring_threshold`, and
    /// the `(max threshold M)` of the `gc,age` line. Starts at
    /// `config.promotion_age`; [`G1Collector::set_tenuring_flags`] sets it
    /// (`0..=16`, HotSpot's range).
    pub max_tenuring_threshold: AtomicU8,
    /// gci i23/tn -- the typed `-XX:MaxTenuringThreshold=0`: HotSpot's
    /// `AlwaysTenure` (`Arguments::parse_each_vm_init_arg` sets it for 0), the
    /// threshold stays 0 at every pause. A config `promotion_age` of 0 (unit
    /// tests only) keeps the historical floor of 1 after the first pause.
    pub always_tenure: AtomicBool,
    /// gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip) -- is the
    /// pause in flight the `System.gc()` door's, which tenures every live
    /// young object (HotSpot's Full GC leaves no young region behind)?
    /// Stored by `collect_garbage` from the door's thread-local request for
    /// the length of that call and read by the evacuators through
    /// [`G1Collector::evac_tenuring_threshold`] (0 while set).
    pub pause_tenures_all: AtomicBool,
    /// gci i23/tn -- HotSpot's `-XX:TargetSurvivorRatio` on G1 (percent,
    /// default 50): the share of the survivor capacity the age table may fill
    /// before the threshold drops ([`g1_desired_survivor_bytes_at`]).
    pub target_survivor_ratio: AtomicU8,
    /// gci i23/tn (i22/sz page item 4, `CRATONVM_G1_SURVIVOR_REGION_CAP`) --
    /// this pause's Survivor region cap (`usize::MAX` = unarmed: outside a
    /// `collect_garbage` pause, and with the switch off), the Survivor
    /// regions claimed under it so far, and whether a claim was refused this
    /// pause. Armed and disarmed by `collect_garbage` around the dispatch
    /// and the drain passes, so one cap covers one pause, as HotSpot's
    /// `_max_survivor_regions` does.
    pub survivor_region_cap: AtomicUsize,
    pub survivor_regions_claimed: AtomicUsize,
    pub survivor_cap_refused_this_pause: AtomicBool,
    /// gci i23/tn -- the SERIAL evacuator's twin of `TlabSet::survivor_full`:
    /// a Survivor copy found no room after the cap refused a region, so every
    /// later copy of this pause goes to Old (HotSpot sets the worker's
    /// tenuring threshold to 0). Cleared at arm and disarm.
    pub survivor_full_serial: AtomicBool,
    /// gci i23/tn -- census: copies the cap sent to Old, and pauses in which
    /// the cap refused a Survivor region. `[GC-STAT]` prints both when the
    /// first is non-zero (`gci_i13_ef_note`).
    pub survivor_cap_promotions: AtomicU64,
    pub survivor_cap_pauses: AtomicU64,

    /// String deduplication table: hash -> canonical object address.
    /// T10.9.B: FxHashMap — key is Java String hash from loaded bytecode.
    pub string_dedup_table: Mutex<FxHashMap<u64, usize>>,

    /// Whether GC event logging is enabled.
    pub gc_log_enabled: AtomicBool,

    /// Whether a concurrent mark cycle has completed and mixed GC is needed.
    pub marking_complete: AtomicBool,
    /// Remaining mixed GC cycles after a marking cycle.
    pub mixed_gc_remaining: AtomicU64,

    /// LANE W6-M — the MIXED-PAUSE REFUSAL CENSUS.
    ///
    /// Four waves asked "why does this collector never take a mixed pause?"
    /// and answered it with projections, because the three places a mixed
    /// pause can be refused are not distinguishable from outside:
    ///
    ///  1. the gate is never armed — `marking_complete` is false, so
    ///     `needs_mixed_gc()` returns false and nothing downstream runs;
    ///  2. the gate is armed but `mixed_phase_has_work()` says no, either
    ///     because no Old region is a candidate at all or because the
    ///     candidates' garbage is under `heap_waste_percent`;
    ///  3. the pause is taken but the OLD half of the collection set is
    ///     truncated — by the `old_cset_region_threshold_percent` cap or by
    ///     the Step-7 copy-time budget.
    ///
    /// All three produce the same observable ("`[GC-SUMMARY] mixed` is
    /// absent or tiny"), and a projection cannot tell them apart. These
    /// counters can. They are plain relaxed adds on a path that already
    /// stopped the world, printed on one `[GC] g1 mixed-census:` line.
    ///
    /// The candidate tallies (`_cand_`) are summed over every *poll*, not
    /// over regions — a region refused on ten consecutive polls counts ten
    /// times. That is deliberate: the question is "what did the selector see
    /// when it was asked", and a per-region high-water mark would hide a
    /// refusal that only happens while the phase is open.
    pub mixed_gate_polls: AtomicU64,
    /// Polls at which `needs_mixed_gc()` was false (case 1 above).
    pub mixed_gate_no_cycle: AtomicU64,
    /// Polls at which the gate was armed but `mixed_phase_has_work()` refused.
    pub mixed_gate_no_work: AtomicU64,
    /// Polls that produced an actual mixed collection.
    pub mixed_pauses_entered: AtomicU64,
    /// Old regions refused by `live_bytes == 0` (no data from a mark cycle).
    pub mixed_cand_no_mark_data: AtomicU64,
    /// Old regions refused by `mixed_gc_live_threshold_percent`.
    pub mixed_cand_too_live: AtomicU64,
    /// Old regions refused for being pinned.
    pub mixed_cand_pinned: AtomicU64,
    /// Refusals by the `heap_waste_percent` floor with candidates present.
    pub mixed_work_refused_waste: AtomicU64,
    /// Selections truncated by `old_cset_region_threshold_percent`.
    pub mixed_cset_capped_percent: AtomicU64,
    /// Selections truncated by the Step-7 copy-time budget.
    pub mixed_cset_capped_budget: AtomicU64,
    /// Old regions actually placed in a mixed collection set.
    pub mixed_old_regions_selected: AtomicU64,
    /// gci i10/rh -- `CRATONVM_G1_MIXED_HOTSPOT_CHOICE` (default ON since
    /// 2026-10-17, `=0` kills), read once per collector so no process global is
    /// added. Findings 1, 2 and 10.
    pub mixed_hotspot_choice: bool,
    /// gci i10/rh finding 2 -- HotSpot's per-pause minimum of old regions,
    /// `ceil(candidates at cleanup / mixed_gc_count_target)`. Switch arm only.
    pub mixed_min_old: AtomicUsize,
    /// g1ep7: mixed phases ended because every candidate left was pinned
    /// ([`Self::end_mixed_phase_if_only_pinned_left`]).
    pub mixed_phase_ended_pinned: AtomicU64,
    /// gci i10/rh finding 1 -- evacuation copies (bytes, copies) that landed in an
    /// Old region carrying mark data. Both arms; printed on `[GC] g1 mixed-live:`.
    pub promoted_into_marked_bytes: AtomicU64,
    pub promoted_into_marked_copies: AtomicU64,
    /// gci i11/gm finding 3 -- mixed pauses whose old-CSet choice charged a
    /// non-zero marginal fix-up price (`CRATONVM_G1_PAUSE_COST_MODEL` arm of
    /// [`Self::choose_old_cset`]). Printed on `[GC] g1 gm-census:`.
    pub mixed_fixup_charged_pauses: AtomicU64,
    /// gci i11/gm finding 8 -- `CRATONVM_G1_MIXED_COUNT_SKIPS_COMPACTION`
    /// (opt-in, default OFF), read once per collector: a full-compaction or
    /// allocation-failure compaction pause does not spend the mixed phase's
    /// `mixed_gc_remaining` count.
    pub mixed_count_skips_compaction: bool,
    /// gci i11/gm finding 8 -- compaction pauses that ran during a mixed phase
    /// and (switch arm) left its count alone / (default arm) spent one.
    pub mixed_count_compaction_pauses: AtomicU64,
    /// gci i11/go (i10rh finding 9) -- `CRATONVM_G1_PREPARE_MIXED` (opt-in,
    /// default OFF), read once per collector: the first pause after cleanup is
    /// young-only, HotSpot's `Pause Young (Prepare Mixed)`
    /// (`G1CollectorState::in_young_gc_before_mixed`), and mixed pauses start
    /// with the next one.
    pub prepare_mixed: bool,
    /// Armed by `cleanup` (switch arm only), consumed by the next pause's
    /// `collect_garbage` gate, and disarmed wherever the mixed phase ends
    /// (`spend_mixed_count`, `end_mixed_phase`).
    pub prepare_mixed_pending: AtomicBool,
    /// Pauses that ran as `Prepare Mixed` (the gate's fourth arm). The GC
    /// event plumbing samples it on both edges of a pause to label it.
    pub prepare_mixed_pauses: AtomicU64,
    /// gci i11/gm finding 6 -- `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK` (default
    /// ON since 2026-10-17, `=0` kills), read once per collector: eager
    /// humongous reclaim may free
    /// a PRIMITIVE-array span while a mark cycle is open, as HotSpot's
    /// `humongous_region_is_candidate` does for a `typeArray`.
    pub eager_reclaim_during_mark: bool,
    /// gci i11/gm finding 6 -- spans / bytes reclaimed by the during-mark arm,
    /// gray entries it dropped from inside them, and pauses on which the arm
    /// was armed but declined (a marker held popped work or SATB had pending
    /// entries). Printed on `[GC] g1 gm-census:`.
    pub eager_mark_spans: AtomicU64,
    pub eager_mark_bytes: AtomicU64,
    pub eager_mark_gray_dropped: AtomicU64,
    pub eager_mark_declined: AtomicU64,

    /// LANE W7-L — THE FUTILE OLD-TO-OLD COPY LOOP, and the evidence that
    /// refuses it.
    ///
    /// W6-M §5 measured a collector that took **5 397 mixed pauses and copied
    /// 99 GB** on a workload whose whole allocation is 1.8 GB, with
    /// `bytes_freed == bytes_copied` on 615 recorded pauses and a median ratio
    /// of exactly **1.00 across five thousand consecutive pauses** — an old
    /// generation being copied onto itself, region counts and free pool a
    /// fixed point, reclaiming nothing. Six of thirteen runs.
    ///
    /// Every screen the collector already has is an ESTIMATE:
    /// `mixed_gc_live_threshold_percent` and the `heap_waste_percent` floor
    /// both read `live_bytes`, which is what the last completed mark cycle
    /// stamped, and the whole content of the defect is that those numbers were
    /// wrong in the optimistic direction for thousands of pauses in a row. An
    /// estimator cannot screen out its own error. These counters are the
    /// MEASUREMENT of what the pauses actually did, taken at the one funnel
    /// every mixed pause returns through, and the breaker they drive is the
    /// only screen on this path that does not consult `live_bytes`.
    ///
    /// The streak, not a rate: one futile pause is ordinary (a phase whose
    /// last candidate turned out to be JIT-pinned), and the defect is a fixed
    /// point. See [`G1Collector::note_mixed_productivity`] and
    /// [`G1Collector::mixed_futile_breaker_allows_within`].
    pub mixed_futile_streak: AtomicU64,
    /// Mixed pauses that reclaimed less than
    /// [`MIXED_PRODUCTIVE_RATIO_PCT`] of what they copied. Counted in BOTH
    /// arms of `CRATONVM_G1_MIXED_FUTILE_GUARD`, so it is the engagement
    /// count for the defect rather than for the fix.
    pub mixed_futile_pauses: AtomicU64,
    /// Mixed pauses that cleared the bar — the denominator under the above.
    pub mixed_productive_pauses: AtomicU64,
    /// Polls that reached the breaker with the streak at or past its trip
    /// point. Counted in BOTH arms; this is the engagement count for the fix.
    pub mixed_futile_breaker_polls: AtomicU64,
    /// Polls the breaker actually REFUSED. Zero unless the guard is armed.
    pub mixed_work_refused_futile: AtomicU64,
    /// Polls the tripped breaker let through anyway as its periodic probe,
    /// so a heap that starts reclaiming again can close the breaker.
    pub mixed_futile_probes: AtomicU64,
    /// Polls whose verdict was carried by the `tight` waiver ALONE — the
    /// `heap_waste_percent` floor said no and the waiver overrode it.
    ///
    /// W6-M named the waiver as "the screen that should catch this and does
    /// not". That is an inference from the mechanism and it was never
    /// measured: `reclaimable` is computed from the same `live_bytes` the
    /// defect corrupts, so a stale-low `live_bytes` makes the floor pass ON
    /// ITS OWN and the waiver is then not load-bearing at all. This counter
    /// is the difference between those two stories and it costs one add.
    pub mixed_work_waived_tight: AtomicU64,
    /// gci i18/mx -- mixed-driver pauses NOT fed to the breaker because they
    /// failed evacuation or were a compaction
    /// (`mixed_choice::is_futile_breaker_sample`). Both arms.
    pub mixed_futile_skipped_samples: AtomicU64,
    /// gci i18/mx -- `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE` (default ON
    /// since 2026-10-15, WARNING a4af35def8; `=0` kills), read once per
    /// collector: the first breaker poll of a mixed phase (fresh mark data)
    /// restarts the futile streak.
    pub mixed_futile_reset_per_cycle: bool,
    /// gci i18/mx -- the breaker has been polled in the current mixed phase.
    /// Set by `mixed_futile_breaker_allows_within`, cleared wherever the
    /// phase ends (`spend_mixed_count`, `end_mixed_phase`). Both arms.
    pub mixed_futile_phase_polled: AtomicBool,
    /// gci i18/mx -- streaks the switch arm restarted at a phase start while
    /// they stood at or past the trip point (the latch it broke).
    pub mixed_futile_cycle_resets: AtomicU64,
    /// gci i18/mx -- `CRATONVM_G1_YOUNG_SIZES_TO_RESERVE` (opt-in, default
    /// OFF), read once per collector: the Free-fraction trigger fires at
    /// HotSpot's `G1ReservePercent` floor ([`to_space_reserve_regions`])
    /// instead of `needs_gc_free_percent`, and an evacuation failure raises
    /// the survival prediction to 100 % instead of `needs_gc_free_percent`
    /// by 8. See [`free_fraction_trigger_fires`].
    pub young_sizes_to_reserve: bool,

    /// OLD regions this collector has handed back to the free list: a mixed
    /// collection-set region that evacuated cleanly, or an all-dead old region
    /// cleanup freed in place. The trigger of the retained-layout census
    /// (`gengc-r5w3-unload7-g1-remark-may-unregister-layouts-of-unswept-instances`):
    /// a dead instance of a class the final remark unloaded leaves the heap
    /// only with its region, and only an old reclamation (or the first young
    /// pause after the remark, for dead young instances) can change the
    /// census's answer. See [`G1Collector::layout_census_due`].
    pub old_regions_reclaimed: AtomicU64,
    /// gci i18/nr (gci i17/rv finding 11): `CRATONVM_G1_CLEANUP_SCRUB_FREED_TARGETS`
    /// (default ON), read once for THIS heap at construction instead of once
    /// per process.
    pub cleanup_scrub_freed_targets: bool,
    /// gci i18/nr (gci i17/rv finding 11): the freed-target scrub's counters,
    /// per heap. See [`CleanupScrubCounters`].
    pub scrub_counters: CleanupScrubCounters,
    /// `(old_regions_reclaimed, retained-layout total)` at the last census
    /// [`G1Collector::layout_census_due`] granted; `u64::MAX` pair = never.
    pub layout_census_key: Mutex<(u64, u64)>,
    /// Census walks that met an object they could not size (the census was
    /// not taken). See [`G1Collector::object_class_ids`].
    pub layout_census_incomplete: AtomicU64,

    /// Audit fix (HIGH-3): persistent mark worklist (the "gray" stack)
    /// drained by `concurrent_mark_step`. Roots are pushed by `remark`
    /// (which the VM calls both at initial-mark and at final-remark
    /// STW points) and SATB entries are pushed when remark drains the
    /// SATB queue. Each step pops an object, marks it, and pushes its
    /// reference fields that are not yet marked. Stored as raw `usize`
    /// addresses so the queue is `Send`/`Sync` without `unsafe impl`
    /// gymnastics for `*mut u8`.
    pub mark_worklist: Mutex<Vec<usize>>,
    /// F-12 — one gray deque per concurrent-mark worker.
    ///
    /// `mark_worklist` above stays as the SHARED SEED queue: roots, SATB
    /// overwrites, evacuation keep-alives and the overflow rescan all deposit
    /// there, and every worker refills from it in `MARK_REFILL_CHUNK` bites.
    /// These are where the transitive closure actually happens — a worker pops
    /// from the back of its own deque and pushes the children it discovers onto
    /// the same end, so the common case touches a lock nobody else wants.
    ///
    /// Marking used to be ONE worker draining ONE `Mutex<Vec<usize>>`, so even
    /// a second worker would have contended on every push and pop. Mark
    /// duration is not just CPU: it sets how much headroom the IHOP heuristic
    /// has to leave, so a slow marker costs heap.
    ///
    /// **Locking discipline, and it is load-bearing.** A deque is only ever
    /// touched while its toucher holds the regions guard — `read()` for a
    /// worker, `write()` for a pause. That is what makes the post-pause gray
    /// remap correct: `remap_gray_set_after_pause` rewrites every deque under
    /// the write guard, so no worker can be holding a pre-pause address. A
    /// refill or a steal that ran without the regions guard would break it, and
    /// the symptom would be a marker dereferencing an evacuated object.
    ///
    /// Stealing takes at most ONE deque lock at a time — the thief drains into
    /// a local vector and only then extends its own — so there is no lock-order
    /// cycle between two workers stealing from each other.
    pub mark_deques: Box<[Mutex<Vec<usize>>]>,
    /// F-12 — marking workers currently holding popped-but-unscanned work.
    /// See [`ActiveMarker`] for why an empty-queue test alone is not
    /// termination.
    pub mark_active: AtomicUsize,

    /// Item 9b — the parked-worker wake handle for the current mark cycle;
    /// see [`Self::install_mark_waker`].
    pub mark_waker: Mutex<Option<std::sync::Arc<crate::g1_concurrent::ConcurrentMarkState>>>,

    /// Ten-findings item 10 — decaying estimate (7/8 old, 1/8 new) of the
    /// Phase-4 fix-up walk's cost in nanoseconds, from the last pauses'
    /// `fixup_us`. The mixed CSet's copy-time budget is the pause goal MINUS
    /// this; before, the budget priced only the copying and a mixed pause
    /// overran its goal by however long the fix-up took. See
    /// [`Self::old_cset_copy_budget_ns`].
    pub fixup_ns_ema: AtomicU64,
    /// LANE E wave 2 — the DENOMINATOR for `fixup_ns_ema`: a decaying estimate
    /// (7/8 old, 1/8 new) of how many non-CSet regions the fix-up walk covered.
    ///
    /// `fixup_ns_ema / fixup_regions_ema` is mean nanoseconds per walked
    /// region, which is the per-region price the marginal cost model charges a
    /// mixed-CSet candidate for the walk it will CAUSE. Without the
    /// denominator, `fixup_ns_ema` is a constant with respect to the very
    /// choice being made — see [`Self::old_cset_copy_budget_ns`].
    pub fixup_regions_ema: AtomicU64,
    /// gci i9/fa — the run's pause phases, summed (`[GC] g1 pause-phases:`).
    pub phase_totals: G1PhaseTotals,
    /// gci i9/fa — the young pauses' pin sets by publisher, summed
    /// (`[GC] g1 pin-attribution:`; filled only under `CRATONVM_G1_DBG_PINS`).
    pub pin_attribution: G1PinAttribution,
    /// LANE E wave 2 — a decaying estimate of what the YOUNG half of a
    /// collection set costs to copy, in nanoseconds.
    ///
    /// The second half of `lane-e-the-pause-goal-still-does-not-price-the-fixup
    /// -walk-it-causes.md`: every Eden and Survivor region is in the CSet
    /// unconditionally, so the old-half budget was spending the whole pause
    /// goal on the SECOND copy of the pause. Measured from young-only pauses
    /// (`pause_us` minus the phases that are not the closure), because those
    /// are the pauses in which the young half is the whole of the copying and
    /// so the only ones where it is separable.
    pub young_copy_ns_ema: AtomicU64,
    /// F-12 telemetry — successful steals from a peer's deque.
    ///
    /// The engagement census for the work-stealing half of F-12. Zero steals on
    /// a run with more than one worker means the seed queue alone kept everyone
    /// fed (fine, and the common case for a wide root set), but zero steals
    /// AND one worker doing all the scanning is the reading that says the
    /// parallelism is not there.
    pub mark_steals: AtomicUsize,
    /// F-12 telemetry — objects scanned, per worker id.
    ///
    /// The distribution is the point, not the total: a parallel marker whose
    /// work all lands on worker 0 is a serial marker with extra locks.
    pub mark_worker_scans: Box<[AtomicUsize]>,

    /// Round-9 gc HIGH-5 — set when any `mark_worklist` push is
    /// dropped because the cap was hit. The marker checks this flag at
    /// the end of remark and falls back to a conservative full re-walk
    /// of all live regions: every already-marked object is re-scanned
    /// and its outgoing references are pushed again. This replaces the
    /// previous `panic!` (which a hostile Java app could trip) with a
    /// time/correctness trade — no crash, just longer mark.
    pub mark_worklist_overflowed: AtomicBool,

    /// G1MARK-8 — set when the marker popped a gray-set entry whose header
    /// failed the plausibility gate (`classify_mark_scan_target`): a wild
    /// child pointer read from a corrupt/stale ref slot, or (vanishingly
    /// unlikely) a torn header. Scanning such an "object" would amplify the
    /// corruption — its garbage `num_slots`/`array_length` extent would be
    /// walked and more garbage pushed as children. We skip the scan instead,
    /// but that leaves the closure potentially incomplete, so `cleanup`
    /// consults this flag and RETAINS everything for the cycle (no in-place
    /// Old-region frees, no humongous reclaim) — the Generational marker's
    /// "mark all old-gen for this cycle" fail-safe, region-flavored.
    pub mark_saw_implausible: AtomicBool,
    /// g1ep7: this cycle scanned its young regions as root regions at mark
    /// start, so young objects are implicitly live and never traced
    /// ([`Self::root_region_skips`], `g1_root_regions.rs`). Set by every cycle
    /// start, cleared by an abort; read only while marking is active.
    pub root_regions_cycle: AtomicBool,
    /// r11w19 census: set by the final remark's driver
    /// (`VmHeap::g1_final_remark_and_cleanup`) under
    /// `CRATONVM_DBG_JIT_ROOTSCAN=1`, so [`Self::remark`] parks the JIT pins'
    /// orphan seeds in [`Self::deferred_pin_seeds`] instead of pushing them;
    /// [`Self::finish_deferred_pin_seeds`] counts, after the root/SATB closure
    /// has drained, the orphans it did NOT reach, then seeds them.
    pub defer_pin_seeds: AtomicBool,
    pub deferred_pin_seeds: Mutex<Vec<usize>>,
    /// Whether the last completed cycle's `cleanup` trusted its closure (no
    /// implausible gray, gray set drained): the condition under which its
    /// mark bitmap may prove an object dead. Cleared when a cycle starts or
    /// aborts. See [`MarkProvenDead`].
    pub last_mark_trusted: AtomicBool,
    /// gcd d10/o — a full-compaction pause is requested
    /// ([`Self::arm_full_compaction`]); the next collection consumes it.
    pub full_compaction_armed: AtomicBool,
    /// The G1 near-full proxy-define page: the `[base, end)` address ranges
    /// of the regions the last pause's in-place full compaction slid
    /// (`g1_compact_in_place.rs`), or empty. Cleared at the start of every
    /// `collect_garbage`. The VM's post-pause verdicts read it
    /// ([`Self::last_pause_slid_ranges`]): in a slid region an address that is
    /// not a key of the pause's pointer map held no live object, even if it
    /// now parses as one.
    pub last_pause_slid: Mutex<Vec<(usize, usize)>>,
    /// The G1 near-full proxy-define page: the TLAB / fallible-allocation
    /// reserve's floor in regions ([`Self::tlab_reserve_regions`]): 2, or 1
    /// under `CRATONVM_G1_TLAB_RESERVE_FLOOR_ONE` (read once, at construction).
    pub tlab_reserve_floor: usize,
    /// `CRATONVM_G1_FREE_TRIGGER_EDEN_FLOOR` (default ON), read once at
    /// construction: see [`Self::low_free_trigger_allowed`].
    pub free_trigger_eden_floor: bool,
    /// The G1 near-full proxy row: `CRATONVM_G1_BREACHED_RESERVE_EDEN`
    /// (default ON), read once at construction: see
    /// [`Self::breached_reserve_defers`].
    pub breached_reserve_eden: bool,
    /// The G1 near-full proxy row: `CRATONVM_G1_BREACHED_RESERVE_TENURES`
    /// (default ON), read once at construction: see
    /// [`Self::breached_reserve_tenures`].
    pub breached_reserve_tenures: bool,
    /// The G1 near-full proxy row: `CRATONVM_G1_BREACHED_RESERVE_BACKOFF`
    /// (default ON), read once at construction: see
    /// [`Self::breached_reserve_backs_off`].
    pub breached_reserve_backoff: bool,
    /// The G1 near-full proxy row: `CRATONVM_G1_BREACHED_RESERVE_FULL`
    /// (default ON), read once at construction: see
    /// [`Self::note_breached_reserve_full`].
    pub breached_reserve_full: bool,
    /// The Free pool the last full compaction left ([`usize::MAX`] before
    /// the first): [`Self::note_breached_reserve_full`] arms another only
    /// once the pool has fallen below it.
    pub breached_full_free_floor: AtomicUsize,
    /// Full compactions [`Self::note_breached_reserve_full`] armed.
    pub breached_full_arms: AtomicU64,
    /// The G1 near-full proxy row: `CRATONVM_G1_BREACHED_RESERVE_LAST_REGION`
    /// (default ON), read once at construction: see
    /// [`Self::breached_reserve_last_region_fits`].
    pub breached_reserve_last_region: bool,
    /// Bytes the Old regions' unused tails held at the end of the last pause
    /// (`publish_old_tail_bytes`): the destination space a young pause with
    /// no Free region left can still tenure into.
    pub old_tail_bytes: AtomicUsize,
    /// Survivor regions at the last census ([`Self::publish_region_census`]);
    /// only pauses create them. Read by [`Self::low_free_trigger_allowed`].
    pub survivor_region_count: AtomicUsize,
    /// gcd d10/o — the pause in flight is a full-compaction pause: the mixed
    /// drivers take [`Self::full_compaction_candidates`] as their old CSet,
    /// without the percentage cap or the pause-time budget.
    pub full_compaction_this_pause: AtomicBool,
    /// JIT round 22 wave 11 (lane memory, `CRATONVM_G1_OOME_MAXIMAL_COMPACTION`):
    /// the armed full compaction is the OOME path's last-resort MAXIMAL pass
    /// ([`Self::arm_full_compaction_maximal`]): [`Self::full_compaction_candidates`]
    /// takes any Old region with garbage, not only those with an eighth of a
    /// region of it. Cleared when the pause that consumes the arm ends.
    pub full_compaction_maximal: AtomicBool,
    /// gcd d10/o — full-compaction pauses run (see [`Self::arm_full_compaction`]).
    pub full_compaction_pauses: AtomicU64,
    /// Gray-set entries refused as [`GrayRefusal::NotAllocated`] -- addresses
    /// that were never a live object in their region's current incarnation.
    ///
    /// These no longer drive `cleanup`'s retain-everything fail-safe, so they
    /// need their own counter: a fail-safe that stops firing must not become a
    /// fail-safe nobody can see. Read and reset by `cleanup`, which reports it
    /// on its per-cycle `g1 cleanup summary` debug line. (This said "the
    /// `[GC] g1 cycle` line" until round 12; that line, in `gc_metrics.rs`,
    /// has never carried it.)
    pub mark_oob_gray_skips: AtomicUsize,
    /// Gray entries and cap-arm seeds [`classify_mark_scan_target`] refused
    /// because their header lacks `GC_FLAG_HEADER` (r11w19 g1store, proposal
    /// W18-1). Each one impugned its cycle. Read and reset by `cleanup`, which
    /// warns on a non-zero count and reports it on the summary line every
    /// cycle: a count that shows up EVERY cycle names an allocation path that
    /// publishes headers without the flag, and is the reading that decides
    /// `CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG`.
    ///
    /// Per collector since round 12 wave 1 (proposal W19-2). It was a process
    /// static, so two collectors in one process (parallel unit tests, two VMs)
    /// read and reset each other's count.
    pub mark_gray_without_header_flag: AtomicU64,
    /// F-10 engagement counter — how many times the concurrent marker has
    /// ACQUIRED the regions lock to drain a batch of gray objects.
    ///
    /// The change F-10 makes is invisible from outside: the same objects get
    /// marked either way, and a passing mark test proves nothing about how long
    /// the lock was held. This is the free engagement census for it — a step of
    /// `n` objects should bump this by about `n / MARK_LOCK_BATCH` with the
    /// batching on, and by exactly 1 with `CRATONVM_G1_MARK_LOCK_YIELD=0`. A
    /// reading of 1 per step is how you would discover the flag was off, or
    /// that a future edit hoisted the guard back out of the loop.
    pub mark_lock_batches: AtomicUsize,
    /// F-11 engagement counters — allocations served by the SHARED-guard fast
    /// path versus those that had to take the exclusive guard.
    ///
    /// The finding is about which lock allocation takes, and nothing about a
    /// returned pointer says which one it was. These make the answer readable:
    /// a healthy steady state is `shared` dominating by orders of magnitude,
    /// with `exclusive` counting roughly one per region claimed. `shared == 0`
    /// on a run that allocated means `CRATONVM_G1_SHARED_ALLOC=0` was in force
    /// (or a future edit removed the fast path), which is exactly the "the gate
    /// was inert" reading a timing number alone can never give.
    pub alloc_shared_claims: AtomicUsize,
    /// The exclusive half of [`Self::alloc_shared_claims`].
    pub alloc_exclusive_claims: AtomicUsize,
    /// TLAB refills served under the shared guard (F-11).
    pub tlab_shared_carves: AtomicUsize,
    /// TLAB refills that had to take the exclusive guard (F-11).
    pub tlab_exclusive_carves: AtomicUsize,
    /// Throttle counter for [`G1Collector::mark_oob_report_budget`].
    pub mark_oob_report_count: AtomicUsize,
    /// `CRATONVM_G1_DBG_GRAY_PROV=1` — for each address currently on the gray
    /// set, WHERE it was pushed from and (for a child push) the parent object
    /// whose field named it.
    ///
    /// The G1MARK-8 implausible-header gate reports the address it refused and
    /// the region that contains it, which is enough to know the closure was
    /// abandoned and not enough to know why. The three candidate sources want
    /// three different repairs: a stale gray that an evacuation should have
    /// remapped or dropped, a SATB entry logged against memory that has since
    /// been recycled, or a field of a live object that does not hold an object
    /// address at all. Only the pusher can tell them apart, so it records it.
    ///
    /// Diagnostic only, and empty unless the flag is set.
    pub gray_prov: Mutex<std::collections::HashMap<usize, (&'static str, usize)>>,
    /// Distinct `(holder, target)` pairs the V7b post-evacuation verifier has
    /// already reported.
    ///
    /// The verifier walks every surviving region linearly on a rotating
    /// budget, so a single unrepaired slot is re-reported on every pause that
    /// reaches its region. On `TestKillProcessWhileWriting` that turned SIX
    /// distinct holders into 48 617 log lines, and the page that recorded them
    /// read the line count as a corruption RATE. One line per distinct pair,
    /// with the raw total carried on the line, keeps both numbers readable and
    /// separable.
    pub v7b_reported: Mutex<std::collections::HashSet<(usize, usize)>>,

    /// INT-8 — referent-slot hiding. Addresses of every Weak/Soft/Phantom
    /// `Reference` OBJECT registered with the VM's `ReferenceProcessor` at
    /// mark start (published by the initial-mark STW via
    /// [`Self::set_reference_skip_set`]). While a cycle is active,
    /// `scan_object_refs` skips slot 0 (the referent) of exactly these
    /// objects, so the trace cannot keep a weakly-reachable referent alive
    /// through its (strongly-reachable) Reference — the taint that made
    /// bitmap-based reference processing inert. Maintained across every
    /// mid-cycle evacuation pause by [`Self::remap_reference_skip_set`]:
    /// survivors are re-keyed through the pause's pointer map and CSet
    /// casualties are PRUNED (a stale entry could alias a reused address
    /// and hide an innocent object's slot 0 — under-marking). Cleared at
    /// cleanup/abort; empty outside a cycle.
    pub reference_skip: Mutex<FxHashSet<usize>>,
    /// F-12 — `reference_skip.len()`, readable without taking its lock.
    ///
    /// `scan_object_refs` consults the skip set once per non-array object, and
    /// with one marker that was an uncontended mutex — a few nanoseconds,
    /// invisible. With N markers it is a convoy on a single global lock taken
    /// per SCANNED OBJECT, which measured as a large part of why parallel
    /// marking was initially SLOWER than serial marking on a 160,000-object
    /// graph. The set is almost always empty (it is populated only while a mark
    /// cycle has live Weak/Soft/Phantom `Reference` objects registered), so a
    /// relaxed load that skips the lock removes the convoy outright.
    ///
    /// Maintained beside every mutation of the set, all of which are
    /// stop-the-world. It is a HINT in the safe direction only: a stale zero
    /// would skip a live referent's hiding, so it is stored AFTER the set is
    /// filled and BEFORE it is cleared — see the sites. Being conservative the
    /// other way (a stale non-zero) only costs a lock acquisition.
    pub reference_skip_len: AtomicUsize,

    /// Finalizer-resurrection input for the CURRENT collection (see
    /// [`Self::collect_garbage_with_finalizers`]): referent addresses of
    /// registered, not-yet-enqueued finalizable objects. Consumed (taken)
    /// by the serial young/mixed paths' Phase 3.5, which evacuates any of
    /// them that died in the CSet so `finalize()` can still run against
    /// valid memory. Empty on every plain `collect_garbage` call.
    pub pending_finalizer_roots: Mutex<Vec<usize>>,
    /// Output half of the finalizer-resurrection protocol: the POST-copy
    /// addresses of objects Phase 3.5 resurrected this collection. Drained
    /// by [`Self::collect_garbage_with_finalizers`].
    pub resurrected_finalizers: Mutex<Vec<usize>>,
    /// The depth-2 finalizer set of the LAST collection
    /// (`VmHeap::take_resurrection_closure`): the FROM-space (pre-collection)
    /// address of every object Phase 3.5 freshly evacuated -- the resurrected
    /// candidates and everything its drain pulled out of the CSet after them.
    /// An object the strong closure had already copied is not fresh there, so
    /// it is never recorded. Cleared by `collect_garbage_with_finalizers` at
    /// entry, drained by the VM once per pause. A LEAF lock.
    pub resurrection_closure: Mutex<Vec<usize>>,
    /// gci i30/rp: the same set at its POST-collection addresses (each
    /// `resurrection_closure` entry through the collection's final pointer
    /// map), NOT drained. The kept-young-region referent verdict
    /// (`g1_pinned_referents.rs`) reads it to tell a holder that is only
    /// finalizer-reachable from a strong one. Cleared at the start of every
    /// `collect_garbage` (`reset_refproc_kept_young_token`), written by
    /// `collect_garbage_with_finalizers` after the collection. A LEAF lock.
    pub resurrection_holders: Mutex<Vec<usize>>,

    /// Region indices still holding UNRESOLVED self-forwarded objects after
    /// the evacuation-failure retry loop gave up (wedged drain / pass cap).
    /// Paired with [`Self::kept_unresolved_live`]: inside such a region,
    /// ONLY the recorded self-forwarded addresses are live — everything
    /// else below the cursor is dead garbage the failing pass never
    /// scanned, whose ref slots were never rewritten. `is_addr_in_live_region`
    /// must not report those dead bodies as live, or post-GC reference
    /// processing "restores" a weak referent whose fields dangle into
    /// regions freed the same pause (G1CORE-3). Cleared at the start of
    /// every retry evaluation; normally both sets are empty.
    /// Ten-findings item 1 — humongous spans a slot was scanned INTO during
    /// THIS pause.
    ///
    /// Set by the two slot loops that already resolve a referent's region
    /// (`scan_and_evacuate_refs`, `scan_source_region_for_cset_refs`), so the
    /// cost is a region-type read on data those loops hold anyway; the lock is
    /// taken only for the rare slot that really does point at a span.
    ///
    /// This is the whole of "referenced" for a span whose own remembered set is
    /// empty. Such a span can only be reached from the CSet (scanned in full),
    /// from a root (added separately), or from a JIT-pinned region (walked
    /// separately, because pinning is exactly what keeps it out of the CSet) --
    /// every OTHER holder would have recorded an rset edge on the span's start
    /// (`record_outgoing_rset_edges` normalises humongous targets to it).
    pub humongous_referenced: Mutex<RegionSet>,
    pub kept_unresolved_regions: Mutex<RegionSet>,
    /// The self-forwarded (live-in-place) addresses within
    /// [`Self::kept_unresolved_regions`].
    pub kept_unresolved_live: Mutex<std::collections::HashSet<usize>>,
    /// Lock-free fast gate for the two sets above (they are empty except in
    /// the rare wedged-drain window; `is_addr_in_live_region` is a hot path).
    pub kept_unresolved_any: AtomicBool,

    /// SECURITY FIX (V7a): RSet write-barrier TLS-cache epoch.
    ///
    /// `post_write_barrier_rset`'s fast path caches a stable `*const
    /// G1Region` keyed by region index. Because the regions `Vec` never
    /// reallocates, that pointer stays address-valid even after the
    /// region is recycled (reset to `Free` and re-typed) by a
    /// collection. The old fast path therefore could record an inbound
    /// reference into a *just-recycled* region's rset (only the slow path
    /// gated on `RegionType::Free`), holding a reference that the next GC
    /// drains as garbage.
    ///
    /// This monotonic counter is bumped (under the `regions` lock, with
    /// `Release` ordering) at every point that recycles/retypes regions:
    /// the start of `young_collection`, `mixed_collection`, and
    /// `cleanup`. The fast path stamps the current epoch into its TLS
    /// entry and re-loads + compares it (with `Acquire`) on every hit; a
    /// mismatch forces the slow path, which re-validates `region_type !=
    /// Free` under the lock. The `Release`/`Acquire` pair establishes the
    /// happens-before edge so a reclassification can never be missed by a
    /// concurrent mutator's cached entry.
    pub rset_cache_epoch: AtomicU64,

    /// Process-unique identity of this collector instance, minted from a
    /// global monotonic counter in [`G1Collector::new`].
    ///
    /// `post_write_barrier_rset`'s TLS cache must key its cached `*const
    /// G1Region` to the collector instance that produced it. Using the
    /// collector's own address (`self as *const Self`) for that is unsound:
    /// a later collector can be constructed at the SAME address as a dropped
    /// one (stack slot reuse across calls, or allocator block reuse), and if
    /// its `rset_cache_epoch` history happens to line up too — trivially
    /// true for two identically-driven collectors, e.g. unit tests running
    /// the same scenario twice — the fast path revalidates a dangling
    /// pointer into the dropped collector's freed `regions` storage and
    /// writes through it (observed as intermittent 0xC0000374 heap
    /// corruption in parallel `cargo test -p cratonvm-gc` runs). A minted id
    /// is never reused within the process, so a cache entry can only ever
    /// match the instance that created it.
    pub instance_id: u64,

    /// Address-to-region lookup table for O(log R) `region_for_ptr` queries.
    ///
    /// Each entry is `(base_addr, region_idx)`, sorted ascending by
    /// `base_addr`. Built once in [`G1Collector::new`] and never mutated
    /// afterward: the outer `regions: Vec<G1Region>` is constructed with a
    /// fixed length and never `push`/`pop`ed, and each region's `data` Vec
    /// is allocated once with `region_size` capacity — `G1Region::reset`
    /// only zero-fills, it does not reallocate — so the backing-buffer
    /// addresses are stable for the entire lifetime of the collector.
    ///
    /// This replaces the previous O(R) linear scan inside
    /// `scan_and_evacuate_refs` and the write barrier, which was an
    /// audit-flagged hot-path bottleneck (CRIT-P4): with 256 regions, a
    /// 100-slot object incurred ~25k linear probes during evacuation.
    pub region_lookup: Vec<(usize, usize)>,

    /// F-05 — the arena-wide card table: one byte per 512 bytes of heap,
    /// recording whether a cross-region reference store ever landed there.
    ///
    /// The remembered set answers "which REGIONS hold an edge into me"; this
    /// answers "where INSIDE one of them". Phase 2 needs both, and before F-05
    /// it had only the first, so acting on a single remembered edge cost a walk
    /// of the entire source region. Shared (`Arc`) with every [`G1Region`],
    /// which clears its own range in [`G1Region::reset`] — see
    /// [`crate::g1_cards`] for the lifetime rule and for why a scanned card is
    /// deliberately NOT cleaned.
    pub cards: std::sync::Arc<crate::g1_cards::G1CardTable>,

    /// JIT round 21, lane vhcae (VHCAE21-1): the compiled post barrier's edge
    /// filter ([`G1JitEdgeFilter`]), written by [`Self::post_write_barrier_rset`]
    /// and re-tagged by [`Self::bump_rset_cache_epoch`].
    pub jit_edge_filter: Box<G1JitEdgeFilter>,

    /// F-05 engagement counters. A feature whose engagement cannot be counted
    /// is one nobody can tell is running, and these are read out per pause in
    /// [`G1PausePhases`] (`rset_regions_skipped` / `rset_bytes_scanned` /
    /// `rset_bytes_skipped`).
    ///
    /// `card_regions_offered` counts source regions Phase 2 was asked to walk;
    /// `card_regions_skipped` the subset that had no dirty card at all and were
    /// therefore not walked; `card_bytes_scanned` the bytes of objects whose
    /// reference slots were visited; `card_bytes_skipped` the bytes of objects
    /// stepped over because they touched no dirty card. The two byte counters
    /// sum to what the pre-F-05 walk would have visited, so the ratio is the
    /// engagement, and a zero in `card_bytes_skipped` beside a non-zero
    /// `card_bytes_scanned` says the screen ran and refused — which a bare
    /// "cards scanned" count could never distinguish from "the screen never ran".
    pub card_regions_offered: AtomicU64,
    pub card_regions_skipped: AtomicU64,
    pub card_bytes_scanned: AtomicU64,
    pub card_bytes_skipped: AtomicU64,
    /// Card cleaning — cards this pause rewrote to CLEAN, and cards it left
    /// (or put back) DIRTY because an object starting there still references
    /// another region.
    ///
    /// The pair is the engagement census for the cleaning pass, and it has to
    /// be a pair for the reason the scanned/skipped pair does: a `kept` with no
    /// `cleaned` beside it says the pass ran and found everything still live,
    /// which is a different fact from the pass never having run. A `cleaned`
    /// that stays near zero across a run is the F-05 saturation this exists to
    /// remove, still happening.
    pub card_cards_cleaned: AtomicU64,
    pub card_cards_kept: AtomicU64,

    // ---- W4-A — the block-offset table's ENGAGEMENT CENSUS ----
    //
    // Three counters, and the set is chosen so that the three ways this lever
    // can produce "no effect" are distinguishable from each other. That is not
    // fastidiousness: this round retracted three measurements (see
    // `orchestrator-wave-1-measurements.md` §4, §7.1, §7.2) and every one of
    // them was an experiment that did not check it was exercising the thing it
    // named. A lever that reports only wall clock cannot tell "the jump fired
    // constantly and bought nothing" from "the jump never fired".
    //
    // * `block_jumps_taken` — jumps the walk actually made. ZERO here with the
    //   lever armed means the heap offered nothing to skip, not that the table
    //   is broken.
    // * `block_bytes_jumped` — the bytes those jumps stepped over WITHOUT
    //   sizing an object. This is the number the whole structure moves, and it
    //   is a COUNT: §8.1 of the orchestrator page is the precedent for
    //   preferring one to a difference of two noisy wall clocks when the effect
    //   is smaller than the host's noise band, which this one is by
    //   construction.
    // * `block_entries_refused` — candidate entry addresses that failed the
    //   header screen and were therefore NOT jumped to. The safety valve's own
    //   engagement count. Anything but zero on a healthy heap says the table's
    //   invariant is not holding and the lever must go back to off.
    //
    // `block_bytes_jumped` is deliberately NOT folded into `card_bytes_skipped`.
    // That counter means "bytes of objects stepped over because they touched no
    // dirty card", which is a statement about objects the walk SIZED; these are
    // bytes it never looked at. Adding them would inflate the card screen's
    // skip-rate with work a different feature did, and the skip-rate is the
    // number `CRATONVM_G1_CARD_CLEAN`'s default is decided on.
    pub block_jumps_taken: AtomicU64,
    pub block_bytes_jumped: AtomicU64,
    pub block_entries_refused: AtomicU64,
    /// Source regions whose walk stopped early because no card from its
    /// position to the region's end was dirty, and the bytes that saved.
    ///
    /// A separate pair from the jumps above because it engages for a different
    /// reason: this one reads only the card cursor, never the block-offset
    /// table, so a run in which the table is useless and this is not is a
    /// perfectly possible and perfectly informative outcome.
    pub block_tail_breaks: AtomicU64,
    pub block_tail_bytes: AtomicU64,

    // ---- LANE E, wave 2 — making the card screen's skip-rate OBSERVABLE ----
    //
    // The four counters above are accumulated on every pause and, until now,
    // rendered in exactly one place: the `[GC] g1 card-clean:` line in
    // `print_gc_summary`. That line is emitted only from `vm-cli`'s
    // normal-return teardown, and every JUnit workload in the suites ends in
    // `System.exit`, which never unwinds Rust frames — so on the workloads the
    // card table was measured on, the skip-rate was never printed at all.
    //
    // That makes one specific experiment unrunnable from a shipped binary, and
    // it is the experiment that decides `CRATONVM_G1_CARD_CLEAN`'s default: G1's
    // card table is ADDITIVE-ONLY, so it saturates by construction and the
    // screen decays toward useless over a long run; cleaning is what stops
    // that. "Does the skip-rate decay, and does cleaning hold it up" is a
    // question about a TREND, so a single shutdown total could not answer it
    // even if it were printed. The two fields below carry the trend's endpoints
    // onto the per-pause `[GC-STAT]` line, where a run does not have to
    // terminate cleanly for the number to exist. See §5 of
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    /// The card screen's byte skip-rate over the FIRST
    /// [`G1_CARD_TREND_WINDOW`] instrumented pauses, in parts per million,
    /// frozen once the window closes. The trend's left endpoint.
    pub card_first_window_scanned: AtomicU64,
    pub card_first_window_skipped: AtomicU64,
    /// The same rate, smoothed 7/8 over every pause. The trend's right
    /// endpoint. An EMA rather than a last-N ring because the ring that would
    /// answer it (`pause_history`) holds 65 536 records and walking it once per
    /// pause to render a log line is a cost the log line does not justify.
    pub card_skip_rate_ppm: AtomicU64,

    /// Lock-free inclusive-exclusive bounds `[arena_base, arena_end)` of the
    /// single contiguous backing arena. Immutable for the collector's
    /// lifetime (the `arena` `Box` is allocated once in [`G1Collector::new`]
    /// and never moved or resized), so they can be read with no atomics and
    /// no `regions.lock()`.
    ///
    /// Used by [`G1Collector::is_addr_in_live_region`] as an O(1) lock-free
    /// reject for the overwhelming majority of conservative-root-scan
    /// candidate words (return addresses, ints, native-stack addresses) that
    /// fall outside the heap arena entirely. That function is the per-word
    /// hot path of `scan_active_jit_frames` / `update_root_snapshot`, which
    /// run on *every* object-returning native call; the previous
    /// implementation took the regions mutex and linearly scanned all
    /// `num_regions` regions for each candidate word, contending
    /// catastrophically on deep-stack JIT-on workloads. This mirrors the
    /// lock-free `[base, end)` bounds gate `gen_heap` already adopted for the
    /// identical reason (see `GenerationalHeap::is_object_address`).
    pub arena_base: usize,
    pub arena_end: usize,
    /// F-16 — the committed prefix at construction (`-Xms`, or the ergonomic).
    /// The floor `uncommit_trailing_free_regions` will not shrink below: an
    /// operator who asked for an initial heap asked not to pay for growing back
    /// into it.
    pub initial_commit_bytes: usize,
    /// `log2(config.region_size)` (F-09).
    ///
    /// `G1Collector::new` rounds the requested region size up to a power of
    /// two so this exists; see [`normalize_region_size`] for why that rounding
    /// is not a policy decision. It makes `lookup_region_for_addr` — the
    /// hottest read in the collector, called TWICE per compiled reference store
    /// — a shift instead of a 64-bit hardware divide.
    pub region_shift: u32,

    /// This pause was entered through
    /// [`Self::collect_garbage_with_finalizers`], i.e. some object registered
    /// for finalization is unreachable-but-not-yet-finalized.
    ///
    /// Separate from `pending_finalizer_roots` because that list is CONSUMED by
    /// Phase 3.5 (`std::mem::take`), so by Phase 5 — where
    /// [`Self::eager_reclaim_humongous_locked`] runs — it is empty on every
    /// path and testing it there would silently always pass. The distinction
    /// matters for exactly one shape: a humongous object with a finalizer.
    /// Phase 3.5 resurrects dead finalizable objects out of the CSet, and a
    /// humongous object is never IN the CSet, so it is never resurrected and
    /// never lands in `resurrected_finalizers` either — it would just be freed
    /// out from under a `finalize()` that has not run yet.
    pub finalizer_pause: AtomicBool,

    /// The addresses [`Self::collect_garbage_with_finalizers`] was handed this
    /// pause, kept for [`Self::eager_reclaim_humongous_locked`].
    ///
    /// Separate from `pending_finalizer_roots` for the reason that field's doc
    /// gives -- Phase 3.5 CONSUMES that list with `std::mem::take`, so by the
    /// time eager reclaim runs it is empty on every path. This copy is not
    /// consumed, so the reclaim can name the one shape `finalizer_pause` used
    /// to decline the entire reclaim for: a HUMONGOUS object with a finalizer,
    /// which is dead, is never in the CSet, is therefore never resurrected, and
    /// so is invisible to the census.
    ///
    /// Naming those spans is strictly cheaper than the gate it replaces.
    /// `finalizer_pause` is true for ANY registered not-yet-enqueued
    /// finalizable object -- one live `FileInputStream` is enough -- so the gate
    /// disabled eager humongous reclaim for the whole process. Measured on H2
    /// `TestKillProcessWhileWriting` (2026-09-02), gate versus no gate:
    /// 76 787 young pauses -> **88**, humongous regions 830 -> **26**, free
    /// regions 184 -> **980**, collection set 1.2 -> **128** regions, and the
    /// class goes from a 900 s cap to **passing in 633 s**.
    pub finalizer_addrs_this_pause: Mutex<Vec<usize>>,

    /// Collection-overlay values in HUMONGOUS regions this evacuation pause
    /// treated as live (rooted for a non-CSet owner, followed from a forwarded
    /// owner, or kept for the mark snapshot). The eager
    /// humongous reclaim names them live: a humongous array held only by an
    /// overlay row (a big `TreeMap`'s backing `data`) has no heap slot naming
    /// it, so neither the census nor the remembered set would. See
    /// `g1_overlay.rs`.
    pub overlay_live_this_pause: Mutex<Vec<usize>>,
    /// CSet overlay owners the current collection found dead, by pre-pause
    /// address; taken by the VM's post-pause prune
    /// ([`Self::take_overlay_condemned_owners`]).
    pub overlay_condemned_owners: Mutex<FxHashSet<usize>>,
    /// The mark cycle's snapshot of every overlay row this heap owns, owner ->
    /// values, captured at initial mark and carried across every evacuation
    /// pause of the cycle. Empty outside a cycle. See `g1_overlay.rs`.
    pub(in crate::g1) overlay_snapshot: RwLock<overlay::OverlaySnapshot>,
    /// The young-pause remembered set over overlay rows (`g1_overlay.rs`).
    pub(in crate::g1) overlay_rset: Mutex<overlay::OverlayRemSet>,
    /// `overlay_snapshot.len()`, readable without its lock (the marker's
    /// per-object gate). Zero outside a cycle.
    pub overlay_snapshot_len: AtomicUsize,
    /// Class ids of the snapshot's owners, as a bitmap: the marker asks the
    /// snapshot only about objects of a class that owns a row.
    pub overlay_snapshot_classes: Box<[AtomicU64]>,

    /// Persistent parallel-evacuation worker threads (see [`cratonvm_gc_core::evac_pool`]).
    ///
    /// Created on the FIRST parallel pause rather than in [`G1Collector::new`],
    /// and deliberately so: a `G1Collector` is cheap and short-lived in the
    /// unit suite (hundreds are constructed and dropped), most of those never
    /// evacuate anything in parallel, and eagerly spawning
    /// `gc_worker_threads - 1` OS threads for each would cost far more than the
    /// per-pause spawn this pool exists to remove. A collector that does
    /// evacuate pays the spawn exactly once and reuses the threads for the rest
    /// of its life; the pool is joined when the collector drops.
    ///
    /// Sized from `parallel_worker_count()` at first use. That value is fixed
    /// for the process (config plus a `OnceLock` flag), so the pool is never
    /// the wrong width — and if it ever were, `EvacPool::scope` clamps rather
    /// than over-dispatching.
    pub evac_pool: std::sync::OnceLock<cratonvm_gc_core::evac_pool::EvacPool>,
    /// zt pl4 item 3: the CPU, nanoseconds, the concurrent-mark worker
    /// threads spent in mark steps (`g1_concurrent`; HotSpot's
    /// `sun.threads.cpu_time.gc_conc_mark`).
    pub conc_mark_cpu_ns: std::sync::atomic::AtomicU64,
    /// zt pl4 item 3: the CPU the collecting (driver) thread spent inside
    /// collections, nanoseconds: its share of the parallel work, booked with
    /// the pool helpers' as `gc_parallel_workers` (`VmHeap`'s dispatcher).
    pub driver_cpu_ns: std::sync::atomic::AtomicU64,

    /// G1AUD-9 — rotating start index for the Free-region search
    /// ([`Self::claim_free_region`]).
    ///
    /// [`find_free_region_from`] restarted its linear scan at region 0 on every
    /// call, so claiming the `k`th region of the heap cost `k` probes and
    /// filling a heap of `R` regions cost `O(R^2)` — 1500 regions at
    /// `-Xmx1500m` is over a million region-type reads under the one mutex
    /// every allocation path also needs. The Free pool is consumed
    /// front-to-back and replenished in bulk by a pause, so resuming the scan
    /// where the last claim ended finds the next Free region in O(1) amortized
    /// while remaining exhaustive (the search wraps, so a region freed *behind*
    /// the cursor is still found).
    ///
    /// A stale hint costs at most one extra wrap; it is never a correctness
    /// input (the claim re-checks `region_type == Free` under the lock).
    pub free_scan_hint: AtomicUsize,

    /// Item 8 — the downward twin of [`Self::free_scan_hint`] for young
    /// claims: the highest region index the next `claim_free_region_young`
    /// should probe first. `usize::MAX` means "start at the top of the
    /// committed prefix"; the claim clamps it.
    pub young_scan_hint: AtomicUsize,

    /// G1AUD-9 — per-destination-type evacuation allocation cursor, the
    /// serial evacuator's equivalent of the parallel evacuator's per-worker
    /// TLAB.
    ///
    /// [`Self::alloc_in_type_locked`] used to answer "where does this survivor
    /// go?" with a scan of the WHOLE region table from index 0, for EVERY
    /// evacuated object. Young survivors land in fresh regions claimed from
    /// the tail of the Free pool, so the scan walked every Eden, Survivor, Old
    /// and Free region ahead of the destination before finding it — an O(R)
    /// probe per object, i.e. `O(R x objects_copied)` per pause, on the serial
    /// path that is still the DEFAULT (parallel evacuation is opt-in, and
    /// `mixed_collection` is pinned to serial regardless).
    ///
    /// The hint records the region the previous object of that type landed in.
    /// It is validated on every use (right type, not in the CSet, has room)
    /// and falls back to the same exhaustive scan on a miss, so it can only
    /// ever save work — never change which objects can be placed.
    /// `usize::MAX` means "no hint".
    pub evac_dest_survivor: AtomicUsize,
    pub evac_dest_old: AtomicUsize,

    /// G1AUD-9 — cached count of Eden + Survivor regions, published wherever
    /// [`Self::free_region_count`] is (they are counted in the same pass).
    /// The input to the young half of [`GarbageCollector::needs_gc`].
    pub young_region_count: AtomicUsize,

    /// G1AUD-9 — adaptive young-generation size, in REGIONS: the young region
    /// count at which a collection is requested even though the Free pool is
    /// still comfortable (`CRATONVM_G1_YOUNG_PAUSE_TARGET`).
    ///
    /// # What this closes
    ///
    /// `max_gc_pause_ms` reached exactly one decision in this collector: how
    /// many OLD regions a mixed collection set may take
    /// (`select_old_regions_for_mixed_gc`, and the copy of that logic inside
    /// `mixed_collection`). The YOUNG half — which is every young pause, and so
    /// the overwhelming majority of all pauses — was bounded by nothing but the
    /// Free pool: `needs_gc` fires at `free < 25%`, so Eden grows to roughly
    /// 75% of `-Xmx` before the first collection and young pause time scales
    /// with the HEAP SIZE. Raising `-Xmx` made every pause longer. For a
    /// collector whose entire proposition is a configurable pause goal, that is
    /// the goal not being implemented on the path that matters.
    ///
    /// # Why it starts at the maximum, and why the maximum is not a trigger
    ///
    /// So that it is a no-op until a pause is actually measured to overrun.
    /// `record_collection` shrinks the target after a pause longer than
    /// `max_gc_pause_ms` and grows it back while pauses stay under half of it,
    /// bounded by [`Self::young_region_bounds`]. (The reverse — starting small
    /// and growing — would change the trigger for every existing workload on
    /// the first allocation, which is a throughput decision no measurement here
    /// supports.)
    ///
    /// Starting at the ceiling is NOT on its own enough to make it a no-op, and
    /// this doc claimed otherwise until a measurement said so. The ceiling is
    /// 60% of the region count; the free-pool trigger does not fire until 75%
    /// is consumed. So on any heap with headroom the CEILING ITSELF was the
    /// binding trigger, and the cap manufactured collections on a workload
    /// whose pauses had never once overrun. Measured (2026-08-18,
    /// `probes/G1ChurnPauseProbe 64 300`, `-Xmx2048m`, 1.2 GiB of garbage): the
    /// other two arms took ZERO pauses and this one took a 210 ms pause, for a
    /// 20% wall-clock cost and no latency to gain — there were no pauses to
    /// shorten.
    ///
    /// `needs_gc` therefore also requires `target < ceiling`: the cap acts only
    /// on the collector's own evidence that this workload's young generation is
    /// too big for its goal. Regression:
    /// `a_young_target_at_its_ceiling_never_triggers_a_collection`.
    ///
    /// # The anti-storm rule
    ///
    /// A trigger the collector cannot satisfy is worse than no trigger: G1
    /// already has one recorded case of a refusal turning a run into 330264
    /// no-op pauses (see `root_coverage_incomplete_reason`). If a pause frees
    /// nothing and copies nothing — every young region pinned, an empty
    /// collection set — the young count does not fall, so the cap would ask for
    /// another pause immediately, forever. `record_collection` therefore resets
    /// the target to the maximum on any unproductive pause, and only a
    /// PRODUCTIVE overrun tightens it again.
    pub young_target_regions: AtomicUsize,
    /// gci i7/g1y — the young (Eden/Survivor) regions the LAST young or mixed
    /// pause kept out of its collection set because a pin held them
    /// (`jni_pinned_out + jit_pinned_out` of that pause). They stay young after
    /// the pause and count in [`Self::young_region_count`], but no pause can
    /// reclaim them while the pin holds. Under
    /// `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS=1` the young-target arm of
    /// `needs_gc` measures the young generation net of them; see
    /// [`young_target_arm_fires`]. Written on every recorded pause, read only
    /// with the switch on.
    pub young_pinned_kept_regions: AtomicUsize,
    /// gci i12/sx: `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS`
    /// ([`g1_young_target_net_of_pins`]), read once when this collector is
    /// built instead of in a process `OnceLock`.
    pub young_target_net_of_pins: bool,
    /// gci i14/yt: `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`
    /// ([`g1_young_target_hotspot_model`]), read once per collector: size the
    /// young target the way HotSpot's `G1Policy::update_young_length_bounds`
    /// does (predicted base time + per-region cost against the goal, bounds
    /// `max(G1NewSizePercent, survivors + 1)`), and measure the young-target
    /// arm of `needs_gc` as Eden claimed since the last pause plus the packed
    /// Survivor footprint. See [`hotspot_young_target`].
    pub young_target_hotspot_model: bool,
    /// gci i14/yt — young (Eden + Survivor) regions in the census at the end
    /// of the last recorded pause: Survivor destinations, plus every young
    /// region a pin kept. The HotSpot-shaped arm counts Eden claimed SINCE then
    /// (`young_region_count - this`), see [`hotspot_young_arm_count`].
    pub young_after_pause_regions: AtomicUsize,
    /// gci i43/gy — the operator's young sizing as HotSpot 25's
    /// `G1YoungGenSizer` resolves it ([`G1YoungSizer`], packed by
    /// [`G1YoungSizer::encode`]): the kind, and its two region counts.
    /// `0` / `0` / `0` is [`G1YoungSizer::Defaults`] (`G1NewSizePercent` /
    /// `G1MaxNewSizePercent`, the bounds before i43). Written once, before the
    /// first allocation, by [`Self::set_young_gen_sizing`]; read by
    /// [`Self::young_region_bounds`].
    pub young_sizer_kind: AtomicU8,
    pub young_sizer_a: AtomicUsize,
    pub young_sizer_b: AtomicUsize,
    /// gci i43/gy — the young ceiling is the OPERATOR's (`-Xmn`,
    /// `-XX:MaxNewSize`, `-XX:NewRatio`; [`G1YoungSizer::ceiling_is_operators`]):
    /// the young-target arm of `needs_gc` fires AT the ceiling (HotSpot's
    /// young list is never longer than its maximum), instead of going inert
    /// there as it does over the default 60 % ceiling. Set with the sizer.
    pub young_ceiling_binding: AtomicBool,
    /// gci i45/gy — `CRATONVM_G1_YOUNG_BOUNDS_COMMITTED`
    /// ([`g1_young_bounds_committed_enabled`]), read once per collector: the
    /// young bounds are taken from the committed regions (the soft capacity,
    /// [`Self::heap_target_regions`]) and re-derived on every heap size change
    /// (HotSpot's `G1YoungGenSizer::heap_size_changed`), not from the `-Xmx`
    /// grid. An atomic only so a test can flip the arm on a built collector.
    pub young_bounds_follow_committed: AtomicBool,
    /// gci i43/gy (lane lf's hunk H-gy1) -- HotSpot's `-XX:SurvivorRatio` on
    /// this collector: `_max_survivor_regions = ceil(young target /
    /// SurvivorRatio)` (the pause's Survivor cap and the desired survivor
    /// size). 8 (HotSpot's default) unless [`G1Collector::set_survivor_ratio`]
    /// set it.
    pub survivor_ratio: AtomicUsize,
    /// gci i14/yt — `ceil(G1PausePhases::surv_bytes / region_size)` of the
    /// same census: the regions HotSpot's PLAB-packed survivors would occupy.
    pub young_survivor_equiv_regions: AtomicUsize,
    /// gci i14/yt — the HotSpot-shaped model's predictors (decaying averages,
    /// 7/8 like the file's other EMAs), taught by young-only pauses:
    /// closure picoseconds per byte of closure work (copied + rset-scanned),
    /// the pause NET of the copy (HotSpot's "base time": roots, rset, fix-up,
    /// free, verify, other, all young-size independent), and bytes copied per
    /// collection-set region (HotSpot's survival-rate predictor; since gci
    /// i16/pl, Eden-sourced bytes per EDEN region whenever the pause's
    /// survivor-age histogram allows the split -- see
    /// `teach_young_cost_model`).
    pub yt_work_ps_per_byte: AtomicU64,
    pub yt_base_us: AtomicU64,
    pub yt_surv_bytes_per_region: AtomicU64,
    /// gci i14/yt — `CRATONVM_G1_SURVIVOR_PLAB` census, this collector's
    /// lifetime: fresh shared Survivor regions claimed, PLAB chunks handed
    /// out, and bytes of chunk tails stamped as filler at retirement. Printed
    /// at exit as `[GC] g1 survivor-plab:` when the switch is on.
    pub survivor_plab_regions: AtomicU64,
    pub survivor_plab_chunks: AtomicU64,
    pub survivor_plab_tail_bytes: AtomicU64,
    /// gci i16/pl — the same census's direct allocations (finding 4):
    /// objects of at least 10 % of a PLAB copied straight into the shared
    /// Survivor region, and their bytes. Printed on the same exit line.
    pub survivor_plab_direct_objects: AtomicU64,
    pub survivor_plab_direct_bytes: AtomicU64,
    /// gci i25/pl — PLAB claims that found the shared word at
    /// `SURVIVOR_PLAB_SWITCHING` and waited for a peer's install (once per
    /// waiting claim, not per spin). Printed on the same exit line as
    /// `switch_waits=`; the page's hypothesis 1 is decided by it.
    pub survivor_plab_switch_waits: AtomicU64,
    /// gci i16/pl — the HotSpot-shaped young model's survivor split (finding
    /// 12), written only under `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`:
    /// the Survivor census (regions, bytes) at the end of the last recorded
    /// pause, the same census as it stood when the pause being recorded
    /// STARTED (what its collection set took as Survivor), the decaying
    /// survival of Survivor-sourced bytes (per mille of the Survivor bytes a
    /// pause took in), and how many pauses taught the split.
    pub yt_surv_regions_last: AtomicUsize,
    pub yt_surv_bytes_last: AtomicU64,
    pub yt_surv_regions_in: AtomicUsize,
    pub yt_surv_bytes_in: AtomicU64,
    pub yt_surv_survival_permille: AtomicU64,
    pub yt_split_samples: AtomicU64,
    /// gci i12/sx: `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS`
    /// ([`g1_free_trigger_net_of_pins`]), read once per collector; it is read
    /// on the allocation path (`needs_gc`), so it stays a field load.
    pub free_trigger_net_of_pins: bool,
    /// gci i12/sx: `CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC`
    /// ([`g1_verify_unbounded_on_verbose_gc`]), read once per collector.
    pub verify_unbounded_on_verbose_gc: bool,
    /// gci i13/ef finding 9: `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD`
    /// ([`g1_survivor_overflow_to_old`]), read once per collector: a Survivor
    /// copy that finds no room is retried in Old before the evacuation fails
    /// (serial arm; gci i27/gs: the parallel arm too).
    pub survivor_overflow_to_old: bool,
    /// gci i13/ef finding 9: Survivor copies placed in Old by that retry.
    pub survivor_overflow_promotions: AtomicU64,
    /// gci i13/ef finding 4: `CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR`
    /// ([`g1_pinned_young_as_survivor`]), read once per collector: an Eden
    /// region a pin kept out of a young or mixed CSet is retyped Survivor at
    /// the end of the pause, so no Eden region survives a pause (HotSpot's
    /// `Eden regions: a->0`). See [`G1Collector::relabel_pinned_eden_within`].
    pub pinned_young_as_survivor: bool,
    /// gci i13/ef finding 4: Eden regions retyped Survivor by that rule.
    pub pinned_eden_relabelled: AtomicU64,
    /// gci i13/ef finding 6: `-XX:MinHeapFreeRatio` for the sizing policy
    /// (HotSpot default 40). Set by [`G1Collector::set_heap_free_ratios`].
    pub heap_min_free_ratio: AtomicU8,
    /// gci i13/ef finding 6: `-XX:MaxHeapFreeRatio` (HotSpot default 70).
    pub heap_max_free_ratio: AtomicU8,

    // ---- LANE E, wave 2 (2026-09-20) — the adaptive heap-sizing policy -----
    //
    // `CRATONVM_G1_HEAP_RESIZE` gates every one of these; with the flag off
    // `heap_target_regions` is pinned at the region-table length, which is
    // exactly the constant the collector used before, and nothing below is
    // read anywhere that changes a decision.
    /// The SOFT heap capacity, in regions: how much of the `-Xmx` region grid
    /// the collector is currently willing to fill before it collects.
    ///
    /// # Why a soft capacity is the growth lever, and not `commit_through_region`
    ///
    /// `lane-e-heap-never-shrinks-and-never-resizes-to-occupancy.md` asks for a
    /// "GC overhead is above X%, commit more" rule, and the obvious reading —
    /// commit more pages — turns out to be a no-op: every region of the table
    /// is already claimable, `claim_free_region*` commits through on demand,
    /// and `needs_gc` measures the free fraction against the WHOLE table. So
    /// the committed prefix already tracks demand up to `-Xmx` and there is
    /// nothing for a growth rule to widen.
    ///
    /// What the collector does not have is a capacity BELOW `-Xmx` that it can
    /// choose to sit at. That is the quantity this field adds, and it is the
    /// one that makes both directions meaningful at once: raising it means
    /// "collect less often, hold more memory", lowering it means "collect
    /// sooner, hold less" — and, because the trailing-free-run shrink is
    /// floored at it, lowering it is also what makes pages actually go back to
    /// the OS. `-Xms` is its floor and the region-table length its ceiling, so
    /// an operator who named either still gets exactly what they asked for.
    ///
    /// Initialised to the CEILING even when the policy is enabled, for the same
    /// reason `young_target_regions` starts at its ceiling: a policy that
    /// starts by taking something away has to be right on the first pause, and
    /// a policy that starts by giving nothing away only has to be right
    /// eventually.
    pub heap_target_regions: AtomicUsize,
    /// Smoothed fraction of recent wall time spent inside STW pauses, in parts
    /// per million (so 50 000 = 5 %).
    ///
    /// The missing input the finding names: the collector kept `total_pause_us`
    /// but never the wall time BETWEEN pauses, so "GC is taking N % of this
    /// process" was not computable from anything it recorded. Measured over the
    /// pause and the mutator interval that preceded it, then smoothed 7/8 — the
    /// same weight, and for the same reason, as `evac_ns_per_byte`: one
    /// anomalous interval (a debugger stop, a page-cache stall) must not move a
    /// sizing decision on its own.
    pub gc_overhead_ppm: AtomicU64,
    /// When the previous pause ENDED. The denominator's other half.
    ///
    /// A `Mutex<Option<Instant>>` rather than an atomic because `Instant` is
    /// opaque and has no wire format; the lock is taken once per pause, beside
    /// the `pause_history` lock the same function already takes, and is never
    /// contended (only a pause writes it, and pauses are serialised by the
    /// regions guard).
    pub last_pause_end: Mutex<Option<std::time::Instant>>,
    /// Consecutive pauses whose occupancy and GC overhead were both inside the
    /// shrink band. The hysteresis: a heap that shrinks under a live program
    /// and then has to grow again is worse than one that never shrinks, so a
    /// single quiet pause is not allowed to give memory back.
    pub heap_shrink_streak: std::sync::atomic::AtomicU32,
    /// How many times the policy raised / lowered the soft capacity, and how
    /// many bytes the shrink actually returned to the OS across the run.
    ///
    /// Counted rather than only logged because the interesting failure is
    /// OSCILLATION — a grow count and a shrink count that are both large and
    /// nearly equal is the hysteresis being too weak, and that is not visible
    /// from any single `[GC-STAT]` line.
    pub heap_grow_events: AtomicU64,
    pub heap_shrink_events: AtomicU64,
    pub heap_uncommitted_bytes: AtomicU64,
    /// Humongous allocation requests and the ones that found no contiguous run
    /// (lane E proposal P6, the smallest useful half of it).
    ///
    /// An `OutOfMemoryError` on a heap with 40 % of its regions Free is the
    /// signature of humongous fragmentation, and until these existed nothing in
    /// any log said so: `claim_humongous_span_locked`'s `?` and an ordinary
    /// out-of-regions failure were indistinguishable to every caller.
    pub humongous_requests: AtomicU64,
    pub humongous_failures: AtomicU64,
    /// The Free-region count and the longest contiguous Free run observed at
    /// the most recent humongous FAILURE. The two numbers that turn "G1 OOMed"
    /// into "G1 OOMed with 103 free regions and a longest run of 2".
    pub humongous_fail_free_regions: AtomicU64,
    pub humongous_fail_longest_run: AtomicU64,
    /// Region span of the most recent unsatisfied humongous allocation. Its
    /// failure can be exhaustion before marking and fragmentation afterwards.
    /// This is armed only
    /// by the allocation-failure ladder *after* it has completed a fresh mark
    /// cycle; ordinary pauses must not turn a latency policy into a full
    /// compaction policy.
    pub last_failed_humongous_span: AtomicUsize,
    /// A one-pause full-compaction plan prepared under STW.  Its sources are
    /// evacuated through the existing serial mixed collector, but destinations
    /// are kept outside the target window so the window becomes one contiguous
    /// Free span for the failed humongous retry.
    pub pending_humongous_compaction: Mutex<Option<HumongousCompactionPlan>>,
    /// Destination range excluded while the above plan executes and then
    /// reserved for its exact failed request. Per-G1 instance, never
    /// process-wide; allocation is stopped at that point.
    pub humongous_compaction_avoid: Mutex<Option<HumongousCompactionReservation>>,
    /// Old regions a staging compaction pause filled with evacuated objects.
    /// They have no mark data (`live_bytes == 0`) until the next cycle, so the
    /// planner would refuse them as `blocked_by_unknown_mark`; it treats these
    /// as fully live instead. Reset when a ladder arms a fresh request and by
    /// every non-staging compaction pause.
    pub compaction_staged_old: Mutex<Vec<usize>>,
    /// Staging pauses since the ladder last armed a request, bounded by
    /// [`HUMONGOUS_COMPACTION_MAX_STAGES`].
    pub compaction_stages: AtomicUsize,

    /// This collector's entry in the process-global live-heap registry.
    ///
    /// RAII only — see `gen_heap::RELOCATABLE_HEAPS_LIVE`. G1 publishes into
    /// neither of the two tables the frame-band verifier tests, so on its own
    /// it already fails that verifier closed; the registration is what keeps a
    /// MIXED process honest, where another collector's published table would
    /// otherwise be read as an answer about G1's addresses.
    pub _bounds_registration: cratonvm_gc_core::jit_bounds::RelocatableHeapRegistration,
}

// SAFETY: All fields are either atomic, behind Mutex, or Arc. Raw pointers
// in the mark bitmap are heap-managed and only accessed during STW pauses.
unsafe impl Send for G1Collector {}
unsafe impl Sync for G1Collector {}

/// Monotonic source of process-unique [`G1Collector::instance_id`] values.
/// Starts at 1 so 0 can serve as a never-assigned sentinel in debugging.
/// Wraparound after 2^64 collectors is not a practical concern.
pub static NEXT_G1_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Teardown counterpart to the `publish_jit_read_bounds` call in
/// [`G1Collector::new`].
///
/// Without this the read table would keep naming an arena whose `Box` has been
/// freed, and a guarded inline `getfield` would happily do a raw load into it.
/// [`cratonvm_gc_core::jit_bounds::GenerationalHeap`]'s `Drop` zeroes the store-side table
/// for exactly this reason; embedding and unit tests are where a heap actually
/// gets dropped.
///
/// **Owner-checked since 2026-09-01.** It was unconditional, on the precedent
/// `GenerationalHeap::drop` set — "if another live heap owns the table it
/// re-publishes (Generational at its next GC, G1 at construction), and until
/// then helper-only is a safe, merely slower, state". That precedent was itself
/// withdrawn: the gen-side `Drop` measured the claim and found it false (3 of 3
/// collections on a default-collector run ran with an empty table, i.e. nothing
/// re-published), and both it and `ZgcRealHeap::drop` are owner-checked now.
/// This was the last of the three still wiping a table it might not own — a
/// short-lived G1 collector (a sizing probe, a unit test in a peer thread)
/// could blank a live heap's read bounds and cost every inline `getfield` in
/// the process its fast path for good.
///
/// The discriminator is this collector's own arena base, which is what
/// `publish_jit_read_bounds(0, arena_base, ..)` in [`G1Collector::new`] wrote
/// into slot 0.
impl Drop for G1Collector {
    fn drop(&mut self) {
        // F-16: the arena is a `ReservedHeap` now, so its base comes from
        // `base()` rather than from a slice pointer.
        cratonvm_gc_core::jit_bounds::clear_jit_read_bounds_owned_by(self.arena.base());
        // Precise root coverage — and the movable envelope with it,
        // owner-checked on the same discriminator. A dropped collector that
        // left its envelope behind would have the frame-band verifier classify
        // a live heap's addresses against a dead arena's range.
        cratonvm_gc_core::jit_bounds::clear_movable_bounds_owned_by(self.arena.base());
        // F-08 - and the barrier geometry with it. Owner-checked for the same
        // reason: a dropped collector must not leave numbers behind that
        // compiled code would then read as a live arena's.
        cratonvm_gc_core::jit_bounds::clear_jit_g1_barrier_owned_by(self.arena.base());
        // JIT round 23 wave 3, lane L4: and the young-holder table with it.
        cratonvm_gc_core::jit_bounds::clear_jit_g1_young_owned_by(self.arena.base());
    }
}

/// `CRATONVM_GC_G1_MOVABLE_PINS=0` — stop letting G1's pin set honour the
/// movable/rewritable partition, as it did before 2026-09-12.
/// **Default ON**, i.e. a pin whose object is named only through words some
/// channel rewrites is dropped, exactly as the generational path already does.
///
/// # Why it was off, and what changed
///
/// It shipped off on 2026-09-09 for a measured reason: the filter was correct
/// and wired, and it dropped **1 pin out of 34** on H2 `TestValueMemory`,
/// because 4794 of ~5200 JIT roots arrived from the A5 unregistered-frame SPAN
/// sweep, which has no per-frame layout and publishes neither half of the
/// partition. A live GC behaviour change bought for one pin in thirty-four is
/// risk without return.
///
/// Two things moved since. The A5 sweep does not engage on this workload at
/// all any more (`a5_sweeps=0` in `[bandpath]`), so the roots that starved the
/// filter are gone; and the frame-deopt `SavedRegisters` GPR image is now
/// REWRITTEN rather than vetoed (`register_image_remap_admits`), which is what
/// the surviving pins were made of.
///
/// Measured on `probes/TvmProbe.java` — the standalone port of
/// `org.h2.test.unit.TestValueMemory`, whose forty rows assert
/// `used <= memory * 3` — under `-XX:+UseG1GC --Xmx 2g`, four arms of one
/// binary run CONCURRENTLY so this host's load applies to all of them equally:
///
/// | arm | rows over threshold | worst row | sum of all rows |
/// |---|---:|---:|---:|
/// | neither | 4 | 3224 | 91988 |
/// | this filter alone | 4 | 3224 | 91042 |
/// | the remap widening alone | 3 | 3224 | 91790 |
/// | **both** | **0** | **2537** | **59357** |
///
/// Read the first three rows before the fourth: **neither half does anything
/// on its own**, and that is the whole argument for flipping this default
/// together with the remap rather than separately. The filter has nothing to
/// drop while every word vetoes its object, and the rewrite buys nothing while
/// the consumer ignores the claim it publishes.
///
/// The fail-closed direction is unchanged and lives in the PUBLISHER:
/// `publish_unrewritable_band_roots` claims movable only for words it can
/// argue about, and one unverifiable sighting of an address vetoes every
/// movable claim made for it elsewhere.
pub fn g1_movable_pins_enabled() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// JIT round 21 lane parkcredit -- the pure decision behind
/// [`G1Collector::jit_pinned_region_set_screened`]'s snapshot half: which published
/// words still pin. The movable claim (`is_movable`) and its veto
/// (`is_unrewritable`) are the CALLING thread's thread-locals, so they may
/// drop only `own` words; every word in `others` (another thread's deposit, a
/// blocked peer's helper window) pins, whatever the caller claimed for the
/// same object (`jr21-g1-initiator-movable-claim-unpinned-peer-words`).
///
/// Returns the kept words and the claim / veto counts over `own`, which the
/// `[g1][MOVPIN]` line prints.
pub fn g1_kept_pin_words(
    own: Vec<usize>,
    others: Vec<usize>,
    honour_movable: bool,
    is_movable: impl Fn(usize) -> bool,
    is_unrewritable: impl Fn(usize) -> bool,
) -> (Vec<usize>, usize, usize) {
    let (mut n_mov, mut n_unrew) = (0usize, 0usize);
    let mut kept: Vec<usize> = own
        .into_iter()
        .filter(|&addr| {
            let claimed = is_movable(addr);
            let vetoed = is_unrewritable(addr);
            n_mov += claimed as usize;
            n_unrew += vetoed as usize;
            !(honour_movable && claimed && !vetoed)
        })
        .collect();
    kept.extend(others);
    (kept, n_mov, n_unrew)
}





/// `CRATONVM_G1_CLEANUP_IGNORES_PEER_STACK_WORDS` (default ON since ngr, `=0`
/// kills; t1 W13,
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// cleanup's keep set leaves out the words captured from blocked peers'
/// native stacks ([`g1_pins_peer_stack_words`]). Those words pin a region so
/// a pause never MOVES an object a non-reference word may name, and the wake
/// fold stores nothing over it. Freeing an unmarked object stores nothing
/// either, and a stale word, typically a dead `ObjectRef` copy left in an
/// interpreter Rust frame, otherwise keeps a dead humongous span for good:
/// after JIT churn, four such 1 MiB spans split a 96 MiB heap so that no
/// 53-region run was left for a 52 MiB array HotSpot allocates. JIT pins and
/// `region.pinned` (JNI critical) are still honoured.
pub fn g1_cleanup_ignores_peer_stack_words() -> bool {
    true
}



/// `CRATONVM_DBG_JIT_ROOTSCAN` (the VM root gatherer's switch of the same
/// name): also print the remark's `[g1-jitpin-census]` line.
pub fn g1_dbg_jit_rootscan() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| false)}

/// Whether the final remark measures which JIT-pin orphans only a pin keeps
/// alive ([`G1Collector::finish_deferred_pin_seeds`]): the census switch,
/// and only while the pin seeds are on at all.
pub fn g1_jit_pin_need_census() -> bool {
    g1_dbg_jit_rootscan()
}





/// `CRATONVM_G1_PEER_PINS_SKIP_POST_BLOCK_REGIONS` (default ON; `=0` restores
/// pinning every captured word's region): a blocked peer's captured stack word
/// that names a region handed out AFTER that peer entered its blocked window
/// neither pins the region nor reaches the blocked-wake fold
/// ([`G1Collector::drop_post_block_peer_stack_words`]).
///
/// Such a word cannot be a reference: everything the peer can reach it read
/// before blocking, and every object placed since lives in a region whose
/// hand-out stamp is above the peer's block stamp (`gc_quiescence`'s "ngr"
/// section). It is a stale stack word, and because a humongous object starts
/// at its region's base, a stale TLAB bound in a parked Reference Handler's
/// frame named a 34 MB `ArrayList` array of `NativeGrowthReclaimProbe`'s
/// `alAdd` row: the allocation-failure compaction found every window pinned
/// (`blocked_by_pin=77`) and the row threw `OutOfMemoryError` in 5 runs of 6
/// (page `native-growth-reclaim-probe-fails-on-g1`). Dropping the capture,
/// not only the pin, keeps the `g1-jit-windows-parknanos-cached-invoke-target`
/// guarantee: the fold stores nothing over a word that is not a reference. A
/// peer with no published stamp keeps every pin.
pub fn g1_peer_pins_skip_post_block_regions() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        true
    })
}



/// Is any of `words` (sorted, every one in region `idx`) the start of an
/// object of that region, as the pause sees it? A humongous start region has
/// one object, at its base; a continuation region and a Free region have none.
/// Eden, Survivor and Old are walked from base to cursor as g1pin's object pin
/// walks them ([`peer_walk_step`]); a filler or a frozen peer's TLAB tail
/// starts no object. Fails closed: a walk that stops on a stretch it cannot
/// parse answers `true`.
pub fn peer_words_name_an_object_start(
    regions: &[G1Region],
    idx: usize,
    words: &[usize],
    jit_skips: &[(usize, usize)],
) -> bool {
    let Some(region) = regions.get(idx) else {
        return true;
    };
    let base = region.data.as_ptr() as usize;
    match region.region_type() {
        RegionType::HumongousStart => return words.binary_search(&base).is_ok(),
        RegionType::HumongousContinuation | RegionType::Free => return false,
        RegionType::Eden | RegionType::Survivor | RegionType::Old => {}
    }
    let cursor = region.cursor();
    let last = words.last().copied().unwrap_or(0);
    let mut offset = 0usize;
    while offset < cursor && base + offset <= last {
        let addr = base + offset;
        let Some((len, placeable)) = peer_walk_step(addr, cursor - offset, jit_skips).extent()
        else {
            return true;
        };
        if placeable && words.binary_search(&addr).is_ok() {
            return true;
        }
        offset += len;
    }
    false
}

/// The post-block screen's decision for one captured word: its region was
/// handed out (`region_stamp`) after the peer read `block_stamp` on entering
/// its blocked window. A peer with no stamp (`None`) is never screened.
pub fn peer_word_names_post_block_region(region_stamp: u64, block_stamp: Option<u64>) -> bool {
    block_stamp.is_some_and(|b| region_stamp > b)
}



/// Whether this pause must read the conservative JIT pin snapshot: a compiled
/// frame is live by either detector (see [`g1_pins_honour_unregistered_frame`]),
/// or some thread has pins published (see [`g1_pins_honour_any_publication`]).
pub fn jit_frames_live_for_pins() -> bool {
    cratonvm_gc_core::gc_quiescence::conservative_peers_live()
        || cratonvm_gc_core::gc_quiescence::is_active()
        || (cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack())
        || (cratonvm_gc_core::gc_quiescence::pinned_jit_root_count() > 0)
}

/// Publishes the CSet while parallel workers may leave a source header in the
/// transient `FORWARDED | BUSY` state. Dropping the guard closes that window
/// even when a collection exits through an unwind.
pub struct ActiveEvacuationCsetGuard<'a> {
    pub collector: &'a G1Collector,
}

impl Drop for ActiveEvacuationCsetGuard<'_> {
    fn drop(&mut self) {
        // Publish inactive before clearing membership: a reader that already
        // observed `true` may still take the read lock and conservatively see
        // the old CSet, while a later reader takes the lock-free false path.
        self.collector
            .active_evacuation
            .store(false, Ordering::Release);
        self.collector.active_evacuation_cset.write().clear();
    }
}

