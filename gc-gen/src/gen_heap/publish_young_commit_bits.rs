#[allow(unused_imports)]
use super::*;

pub use cratonvm_gc_core::jit_bounds::published_young_range_committed;

/// Publish the young commit bitmaps, with the bounds they describe, as one
/// snapshot (see [`YOUNG_COMMIT_SCREENS`]). An arena with no bitmap (a wholly
/// committed store) is published as no arena, which answers exactly as before
/// gen r5w1: `true`, not this screen's question.
pub fn publish_young_commit_bits(arenas: [(usize, usize, Option<Arc<[AtomicU64]>>); 2]) {
    YOUNG_COMMIT_SCREENS.publish(arenas.map(|(base, end, bits)| {
        bits.map(|bits| CommitScreen {
            base,
            end,
            bits: Some(bits),
        })
    }));
}



impl Drop for GenerationalHeap {
    fn drop(&mut self) {
        // The wipe thread holds raw addresses into an arena this drop frees.
        self.join_evacuated_wipe();
        // The guarded inline getfield's safety argument is "anything inside the
        // published bounds points at a mapped arena". Once this heap's arenas
        // free, that stops holding — so the global tables must not keep naming
        // them.
        //
        // But they are PROCESS-GLOBAL and this heap may not be their publisher.
        // Clearing unconditionally is how a short-lived heap — a sizing probe,
        // an init-time heap replaced once `-Xmx` is parsed — wipes the bounds a
        // DIFFERENT, still-live heap published. The previous comment here
        // accepted that ("if another live heap owns the table it re-publishes at
        // its next GC; until then helper-only is a safe, merely slower, state"),
        // and for the inline-getfield fast path it really is only slower.
        //
        // It stopped being only slower once a CORRECTNESS predicate started
        // reading the same table: `addr_in_published_young_regions` is how the
        // moving-young frame-band verifier decides whether a word is young, so
        // an empty table makes that verifier answer "nothing unpublished" for
        // every frame — vacuously. Measured: with the vacuous-pass guard in
        // place, 3 of 3 collections on a default-collector run reported
        // coverage incomplete, i.e. the table was empty for the whole run and
        // the "re-publishes at its next GC" claim above did not hold.
        //
        // So clear only what this heap actually owns. The young-from base is
        // the discriminator: `store_region_bounds_locked` writes it to slot 0
        // of both tables, so a table still naming this heap's young-from arena
        // is a table this heap published and nobody has replaced.
        let owned_base = self.lock_young_from().base_ptr() as usize;
        let published = JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire);
        if published == owned_base {
            for w in JIT_REGION_BOUNDS.words.iter() {
                w.store(0, Ordering::Release);
            }
        }
        if JIT_READ_BOUNDS.words[0].load(Ordering::Acquire) == owned_base {
            clear_jit_read_bounds();
        }
        // gen r4w6/review6: the young commit screen names this heap's arenas
        // too. Unpublish it when it does (a snapshot naming another live
        // heap's arenas is left alone); readers already holding it keep
        // reading a leaked, still-valid snapshot.
        let owned_to = self.young_to.lock().base_ptr() as usize;
        YOUNG_COMMIT_SCREENS.clear_if_names(&[owned_base, owned_to]);
    }
}

/// The young half of [`GenerationalHeap::reclaimed_hole_at`], captured once by
/// [`GenerationalHeap::reclaimed_hole_probe`] (gc-common w16-x).
#[derive(Debug, Clone, Default)]
pub struct YoungReclaimProbe {
    pub from_base: usize,
    pub from_used: usize,
    pub from_cap: usize,
    /// `(offset, size)` of every young from-space free block, offset-sorted.
    pub from_free: Vec<(usize, usize)>,
    /// The inactive semispace, `[to_lo, to_hi)`.
    pub to_lo: usize,
    pub to_hi: usize,
}

/// Every young address the last collection VACATED, captured once by
/// [`GenerationalHeap::young_vacated_probe`] (gen r5w5/pin9).
///
/// The stale-root audits' question -- "does this root still name bytes the
/// cycle took away?" -- used to be answered by the inactive semispace alone,
/// which is right after a Cheney swap and blind after a pinned in-place cycle:
/// that cycle does not swap, and the bytes it vacates (dead objects and the
/// old copies of the survivors it moved) are INSIDE the active semispace. This
/// answers both. Valid for the pause that captured it: once mutators run, the
/// allocator re-serves the in-place spans.
#[derive(Debug, Clone, Default)]
pub struct YoungVacatedProbe {
    /// The inactive semispace, `[lo, hi)`.
    pub inactive: (usize, usize),
    /// The last pinned in-place cycle's vacated spans, absolute, sorted,
    /// disjoint; empty unless the collection that just ran was one.
    pub in_place: Vec<(usize, usize)>,
}

impl YoungVacatedProbe {
    /// Is `addr` inside the inactive semispace or a span the last pinned
    /// in-place cycle vacated?
    pub fn contains(&self, addr: usize) -> bool {
        if addr >= self.inactive.0 && addr < self.inactive.1 {
            return true;
        }
        let i = self.in_place.partition_point(|&(a, _)| a <= addr);
        i > 0 && addr - self.in_place[i - 1].0 < self.in_place[i - 1].1
    }

    /// `[lo, hi)` of the inactive semispace (what
    /// [`GenerationalHeap::young_inactive_semispace_range`] answers).
    pub fn inactive_semispace(&self) -> (usize, usize) {
        self.inactive
    }

    /// The in-place spans, `(addr, len)`, sorted and disjoint.
    pub fn in_place_spans(&self) -> &[(usize, usize)] {
        &self.in_place
    }
}

/// A generational garbage-collected heap.
///
/// Young generation: two semi-spaces (from/to) using Cheney copying.
/// Old generation: non-moving free-list allocator with mark-sweep.
/// Card table: tracks old→young cross-generation references.
pub struct GenerationalHeap {
    /// Compact-layout domain of the VM that owns this heap. See
    /// `Heap::set_layout_domain`: `class_id` is a per-`ClassStore` index, so
    /// allocating against another domain's registry entry would give the object
    /// a foreign shape. Defaults to the first domain, so an untold heap behaves
    /// as it did before domains existed.
    pub layout_domain: std::sync::atomic::AtomicU32,

    /// Young generation from-space (allocation target).
    pub young_from: Mutex<Arena>,
    /// Young generation to-space (GC copy target).
    pub young_to: Mutex<Arena>,
    /// Old generation (promoted objects).
    pub old_gen: Mutex<OldGen>,
    /// `CRATONVM_DBG_STALE_OBJREF` quarantine ring of evacuated arenas.
    ///
    /// Empty (no cost) when the flag is unset. While
    /// [`cratonvm_gc_core::stale_objref_debug::enabled`] reads true,
    /// `collect_garbage_inner` routes each cycle's just-evacuated
    /// (all-garbage) `young_from` arena through this ring instead of
    /// resetting it immediately: a stale native `ObjectRef` held across that
    /// GC still resolves to a header showing `is_forwarded() == true` for
    /// [`cratonvm_gc_core::stale_objref_debug::quarantine_cycles`] extra full minor-GC
    /// cycles (`CRATONVM_DBG_STALE_OBJREF_CYCLES`, default 1 — the original
    /// single-arena behaviour), which [`Self::get_header`] turns into a hard
    /// panic instead of the object silently reading back all-zero (or,
    /// worse, a same-slot-reused unrelated object) once that memory is
    /// actually reclaimed. Ordered oldest-first; the front arena's grace
    /// period has elapsed and its memory is the next to be reused. See
    /// wildfly-parallel-boot-stale-objectref-residual.md
    /// and wildfly-stale-objectref-debug-assertion-scoping.md.
    pub quarantine: Mutex<VecDeque<Arena>>,
    /// Persistent worker threads for the parallel young copy phase (see
    /// [`cratonvm_gc_core::evac_pool`]), created on first use.
    ///
    /// Lazy, not built in the constructor, for two reasons. Most heaps in this
    /// process never run a parallel copy phase at all — the moving cycle is
    /// only one of the young collector's two paths and the worker policy has
    /// its own floor — and a `GenerationalHeap` is constructed in a great many
    /// unit tests, each of which would otherwise pay for OS threads it never
    /// dispatches to.
    ///
    /// Sized once, from the first cycle that asks. A later cycle wanting more
    /// workers than the pool has gets the pool's count instead
    /// (`EvacPool::scope` clamps, and `ParEvac::drain` counts heads off the
    /// clamped number so the termination handshake stays correct).
    pub evac_pool: std::sync::OnceLock<cratonvm_gc_core::evac_pool::EvacPool>,
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
    pub region_bounds: [(AtomicUsize, AtomicUsize); 3],
    /// Lock-free mirror of each region's per-granule COMMIT bitmap, as ONE
    /// immutable snapshot of all three regions' `[base, end)` and maps (slot
    /// order young from, young to, old). A region whose `bits` is `None` needs
    /// no screen: it is wholly committed (every region falls back to one when
    /// `CRATONVM_GC_RESERVE=0`; the old gen was always one until gen
    /// r4w2/oldgen2 put it on `HeapStore`). Nothing published yet also reads
    /// as "no screen".
    ///
    /// gen r5w1/young5 (2026-09-26): this used to be a `(ptr, words)` pair
    /// per slot plus a hold of the owning `Arc`s, written `ptr` then `words`
    /// and read `words` then `ptr`, so a reader overlapping a republish could
    /// pair one publish's pointer with another's length, read a map whose
    /// `Arc` the hold had just dropped, or ask slot 0's map about the arena
    /// the flip had moved into slot 1: the per-heap twin of item 1 of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    /// The snapshot is answered by ADDRESS (whichever arena contains it),
    /// its map's length is its own, and nothing it reaches is freed before
    /// the heap drops. Latent before (the one writer runs inside a pause and
    /// the readers are mutators on this heap), closed by construction now.
    ///
    /// # Why `region_bounds` is not enough
    ///
    /// [`Self::region_bounds`] publishes `[base, base + CAPACITY)`, and
    /// capacity is RESERVED address space: under the default lazy-commit
    /// backing store (`cratonvm_gc_core::reservation`) most of that range is unmapped
    /// until an allocator cursor reaches it. [`Self::is_object_address`]
    /// dereferences its candidate to read the header, so a conservative stack
    /// word that merely LOOKS like a young-gen pointer sent it into an
    /// unmapped page — a SIGSEGV inside the validator whose whole job is to
    /// answer "is this safe to touch?". Measured 2026-09-03: that is what
    /// killed ~35% of the Spring Framework suite under `-XX:+UseGenerationalGC`
    /// while ZGC, whose validator consults a live-base registry instead of
    /// dereferencing, passed 2832/2848.
    ///
    /// Published from [`Self::store_region_bounds_locked`], at exactly the
    /// same points and under exactly the same STW discipline as the bounds
    /// themselves — the two must not disagree about which arena a slot names.
    pub commit_bits: CommitScreenTable<3>,
    /// Lock-free pointers to the two young arenas' exact object-start bitmaps,
    /// and the `Arc`s that keep them alive.
    ///
    /// Same shape and the same reason as [`Self::commit_bits`]:
    /// `is_object_address` is the VM's hottest validator and was deliberately
    /// made lock-free, so it must reach the bitmap through a published pointer
    /// rather than by locking the arena. `(0)` reads as "no bitmap", which is
    /// the answer whenever `CRATONVM_GC_OBJECT_STARTS` is off and for the old
    /// generation, which has no `Arena` at all.
    ///
    /// The pointer is retired BEFORE the `Arc` is dropped, so no reader can be
    /// handed a pointer this heap is about to free.
    pub object_starts: [AtomicUsize; 2],
    pub object_starts_hold: Mutex<[Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>; 2]>,
    /// gcf f4/y: the bitmaps the LAST publish replaced, kept alive until the
    /// next publish so a reader that loaded their pointer just before it is
    /// never left holding freed memory. See [`Self::store_object_starts_locked`].
    pub object_starts_retired: Mutex<[Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>; 2]>,
    /// r23 w4 review (R-L4z item 2): the two young arenas' lazy-dirty mirrors
    /// (`Arena::lazy_dirty_mirror`: a set bit = inside a span a pinned
    /// in-place cycle left unzeroed, which holds no object), published like
    /// [`Self::object_starts`] -- same slots, same retire-then-drop -- so
    /// `object_start` can DECLINE a stale conservative word into a dead
    /// payload without locking. `0` = no mirror (no lazy cycle yet): one
    /// relaxed-cost load and a not-taken branch on the validator.
    pub lazy_dirty_mirrors: [AtomicUsize; 2],
    pub lazy_dirty_mirrors_hold: Mutex<[Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>; 2]>,
    pub lazy_dirty_mirrors_retired: Mutex<[Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>; 2]>,
    /// Card table covering the old generation's address space.
    ///
    /// T5.5.2 (HIGH-1 fix): the table uses interior mutability for all
    /// state, so no outer `Mutex` is needed. The mutator write barrier
    /// ([`Self::write_barrier`]) stores the card byte directly through
    /// [`CardTable::mark_dirty_lockfree`] — no per-thread buffer, no queue,
    /// no lock. (gcd d1/d: this said "the same byte store the JIT's inline
    /// post barrier emits". That barrier, `CRATONVM_JIT_INLINE_CARD_MARK=1`,
    /// only READS the card inline and calls into `mark_dirty_lockfree` for a
    /// clean card; it never stores the byte unconditionally.)
    ///
    /// gen r4w2/youngpolicy (2026-09-23): this doc used to say the barrier
    /// calls `thread_local_dirty_addr` and that the collector's
    /// [`CardTable::flush_all`] + [`CardTable::drain_pending`] fold "queued
    /// offsets" in at GC start. That buffered pipeline has had no production
    /// writer since gc-genpause F5.2; both calls are kept in the two young
    /// collectors as a no-op-when-empty backstop for a future buffered or
    /// refinement-thread barrier (see `card_table.rs`'s STATUS paragraph).
    pub card_table: CardTable,
    /// gen r4w4/cards4 — THIS heap's compiled-reference-store barrier plan
    /// (the post gate and its mask shape), read by `build_helpers_for` for the
    /// VM that owns this heap. Boxed so the addresses compiled code bakes are
    /// stable for the heap's life.
    ///
    /// It used to be the process-global [`JIT_REF_STORE_GATES`], which a ZGC
    /// heap in the same process rewrites (`reset_generational_state` clears
    /// `post_active`; construction republishes the FLOOR shape, which is wrong
    /// for this collector). See
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    /// Only `post_active`, `post_skip_mask` and `published` of this block are
    /// meaningful; the PRE gate stays the process-wide marker disjunction
    /// (`JIT_REF_STORE_GATES.pre_active`), which is conservative across heaps
    /// by construction.
    pub jit_ref_store_gates: Box<JitRefStoreGates>,
    /// Next identity hash code.
    pub next_hash_code: AtomicI32,
    /// Young GC threshold in bytes.
    ///
    /// An `AtomicUsize` rather than a `Mutex<usize>` because
    /// [`Self::needs_gc_with_jit_allocation_frame`] reads it, and that runs on
    /// the ALLOCATION path — a mutex there is a shared cache line taken
    /// exclusive by every allocating thread to read one word that only ever
    /// changes at a safepoint. Both writers (the post-collection pause-goal
    /// feedback and the heap-expansion reset) run stop-the-world, so the
    /// read-modify-writes they perform have no racing writer and a plain
    /// load/store pair is exactly equivalent to what the lock provided.
    pub young_gc_threshold: AtomicUsize,
    /// The three numbers [`Self::needs_gc_with_jit_allocation_frame`] needs
    /// from `young_from`, republished on every unlock of that mutex.
    ///
    /// # Why these exist rather than another `from.lock()`
    ///
    /// The trigger predicate runs on the ALLOCATION path -- once per
    /// interpreter allocation slow path and once per JIT refill -- and it took
    /// `young_from.lock()` for `used()`, `free_list_bytes()` and `capacity()`.
    /// Even uncontended, that is an atomic read-modify-write on a line every
    /// allocating thread wants, to read three words that are only a heuristic:
    /// the alloc-failure backstop, not this predicate, is what makes the
    /// collector correct. ZGC's equivalent predicate is two relaxed loads.
    ///
    /// # Why the choke point is the GUARD and not the mutation sites
    ///
    /// `arena.rs` has 83 assignments to `cursor` / `high_cursor` /
    /// `free_bytes_total`, and the obvious shape -- publish beside each one --
    /// is 83 chances to miss one, silently, with the symptom being a mistuned
    /// GC trigger rather than anything that fails a test. Every one of those
    /// writes happens under this mutex, so the mutex GUARD is a choke point
    /// that already exists and cannot be bypassed: [`YoungFromGuard`]
    /// republishes on `Drop`, so a mutation that forgets to publish is not
    /// expressible.
    ///
    /// The reader gets a triple that was consistent at some past unlock, never
    /// a torn one. Staleness is bounded by one lock hold and costs at worst a
    /// collection decided one allocation late -- which is what the predicate's
    /// own anti-livelock floor and the allocation-failure backstop already
    /// tolerate by construction.
    ///
    /// `CRATONVM_GC_TRIGGER_LOCKFREE=0` takes the lock instead, so the two are
    /// one binary apart; `CRATONVM_DBG_GC_TRIGGER_VERIFY=1` reads the published
    /// triple, then takes the lock and asserts it agrees with the arena.
    pub young_from_pub_used: AtomicUsize,
    pub young_from_pub_free: AtomicUsize,
    pub young_from_pub_capacity: AtomicUsize,
    // Volatile field access uses `SeqCst` fences inside
    // `get_field_volatile`/`set_field_volatile`; no global lock is
    // needed (and the previous `Mutex<()>` here serialised every
    // volatile access across the entire heap, mirroring nothing in
    // the JMM). Removed to match the fence-only approach used by
    // `heap::Heap`.
    /// Global SATB queue for concurrent GC write barrier logging.
    /// Shared with the concurrent marker; `None` if concurrent GC is not enabled.
    pub satb_queue: Option<Arc<SatbQueue>>,
    // gc-common w5-e: the `concurrent_gc_state` field that sat here is gone.
    // Its only reader was `satb_barrier`'s phase gate, and the barrier now
    // gates on `satb_queue` (see `satb_barrier`). The phase tracker is held by
    // the VM (`HeapRealm::concurrent_gc_state`) and by every cycle's marker.
    /// gen r4w4/concmark4 (2026-09-24) — the VM's shared concurrent-cycle
    /// state, kept for the OLD-GEN POLICY (not for any barrier): the STW
    /// trigger needs to know whether a cycle is open
    /// (`ConcurrentGcState::cycle_in_progress`) and the adaptive start
    /// threshold lives in it. `None` until `enable_concurrent_gc`, which is the
    /// legacy policy's behaviour exactly (no cycle can be open).
    pub conc_cycle_state: Option<Arc<ConcurrentGcState>>,
    /// gen r4w6 (orchestrator) -- the periodic shrink cycle's clock
    /// (`shrink_cycle_due`): `[old-gen collections seen, minor count when
    /// they were last seen or a periodic cycle last opened]`.
    pub shrink_cycle_clock: [AtomicU64; 2],
    /// gc-common w36-d — the depth-2 finalizer set of the LAST collection
    /// (`VmHeap::take_resurrection_closure`): PRE-collection addresses of every
    /// object a finalizer-resurrection round marked that the strong closure had
    /// not. Cleared at the start of every collection, appended by each round
    /// (the non-moving young sweep's drain, the moving Phase 2.5b, the old
    /// generation's `OldGenFinalizerPass`), drained by the VM once per pause.
    /// Per heap, so per VM. A LEAF lock: nothing is acquired while it is held.
    pub resurrection_closure: Mutex<Vec<usize>>,
    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the OLD-ONLY root
    /// section of the pause in flight -- the snapshots of unmounted virtual
    /// threads none of whose words is young
    /// (`VmHeap::stage_parked_old_only_roots`). Left out of the young phase's
    /// root slice; taken and added back by the same pause's major (the moving
    /// Phase 5 in `collect_garbage_inner_with_pins`, the non-moving old sweep
    /// in `run_non_moving_young_cycle`); cleared when the collection door
    /// returns, so it never outlives its pause. Per heap, so per VM. A LEAF
    /// lock.
    pub parked_old_only_roots: Mutex<Vec<usize>>,
    /// Maximum young semi-space size (limits growth).
    pub max_young_semi_size: usize,
    /// The semi-space capacity this heap was built with: `MemoryUsage.getInit()`
    /// of the Eden pool (`jmx_memory_pools`), HotSpot's initial generation
    /// size. gen r4w6/review6 (2026-09-24); it reported `-1`
    /// (`gengc-r4w2-obs-jmx-residuals-on-the-generational-backend` item 4).
    /// The Survivor pool reports `0` since gen r5w2/obs6 (it is the empty
    /// copy reserve, not usable heap).
    pub initial_young_semi_size: usize,
    /// The `-Xmx` budget for the WHOLE committed heap:
    /// `young_from.capacity() + young_to.capacity() + old_gen.capacity()`
    /// must never exceed it.
    ///
    /// Before this field existed, `-Xmx` bounded only the *initial* split.
    /// [`with_sizes`] derived the young growth ceiling from the young semi
    /// alone (`young_semi * MAX_HEAP_EXPANSION_FACTOR`), with no reference to
    /// the total or to the old generation, so adaptive expansion could commit
    /// far more than the user asked for: measured at `--Xmx 1g`, a live run
    /// held `young_from` 512 MiB + `young_to` **1024 MiB** + old 512 MiB — 2 GiB
    /// of Java heap, 4.0 GiB RSS — and was killed by the Linux OOM killer
    /// instead of throwing a catchable `OutOfMemoryError`. With the default
    /// 1/4-of-RAM ergonomic `-Xmx` that ceiling scales with the host: on a
    /// 31 GiB box the ~7.75 GiB default admitted a ~19 GiB committed heap.
    ///
    /// `usize::MAX` means "no budget" and reproduces the historical
    /// behaviour exactly. That is what the explicit-size constructors
    /// ([`new`], [`with_sizes`]) pass: they take absolute arena sizes rather
    /// than a heap budget to divide, and the GC's own tests drive them with
    /// deliberately tiny arenas that rely on expansion. Only
    /// [`with_capacity`] — the sole production path, fed straight from
    /// `-Xmx` via `VmHeap::new_with_overrides` — sets a real budget.
    ///
    /// gen r5w3/oldgen7 — the one exception, opt-in
    /// (`CRATONVM_GC_OLD_BORROW_YOUNG`): an old generation that has GROWN past
    /// its construction capacity ([`Self::initial_old_gen_capacity`],
    /// `OldGen::grow_after_refusal`) takes budget the young pair's CAPACITIES
    /// still claim. The young generation cedes it through its trigger rather
    /// than its arenas (`cede_young_budget_to_old`): from-space is collected
    /// at or below `(Xmx - old capacity) / 2` occupancy, so its survivors — the
    /// to-space's worth — fit that too, and the COMMITTED heap stays inside
    /// `-Xmx` on every path that collects at the trigger. A young arena soft
    /// limit that bounds the bump cursor itself is the residual (see
    /// `docs/feature-designs/generative-gc-architecture.md`).
    pub max_heap_bytes: usize,
    /// HotSpot Serial Runtime.maxMemory(): -Xmx less one fixed copy-reserve survivor.
    /// Zero for explicit-arena test heaps without an -Xmx budget.
    pub serial_pool_max_total: usize,
    /// HotSpot Serial's fixed Tenured Gen pool maximum for the same budget.
    pub serial_tenured_pool_max: usize,
    /// gen r5w3/oldgen7 — the old generation's capacity at construction: its
    /// share of `-Xmx`. [`Self::cede_young_budget_to_old`] caps the young
    /// trigger only once the old generation has grown past it, so a heap that
    /// never borrows keeps its trigger exactly as before.
    pub initial_old_gen_capacity: usize,
    /// gcj j3/og3 (B2, `CRATONVM_GC_OLD_CAPACITY_SHRINK`) — the young-trigger
    /// cap [`Self::cede_young_budget_to_old`] last LOWERED the trigger to (`0`:
    /// none), so a capacity shrink can hand exactly that budget back.
    pub young_ceded_cap: AtomicUsize,
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
    pub numa_node_hint: usize,
    /// Total number of NUMA nodes seen at construction time (>=1).
    ///
    /// Cached so the slow path doesn't have to re-query
    /// [`cratonvm_gc_core::numa::global_topology`] on every allocation. Used by the
    /// stubbed hint logic to short-circuit on single-node hosts (preserving
    /// existing behavior exactly).
    pub numa_num_nodes: usize,
    /// Phase H (RH.1) statistics — updated during every minor/major GC.
    pub stats: HeapStats,
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
    pub force_promote_all: std::sync::atomic::AtomicBool,
    /// gen r4w6/young6: adaptive tenuring — the `-XX:` tenuring flags this
    /// heap was configured with and the adaptive threshold. Per heap, never a
    /// process global. Inert (the fixed [`PROMOTION_AGE`]) unless engaged;
    /// see [`Self::effective_promotion_age`].
    pub tenuring: tenuring::TenuringState,
    /// Survivors the previous moving cycle copied, as the capacity hint for
    /// the next cycle's `pointer_map` (gc-genpause F4).
    ///
    /// # Why a hint and not a better data structure
    ///
    /// `pointer_map` takes exactly one insert per surviving object -- measured
    /// at `pointer_map_len = fwd_copies = 1,446,210` in a single 450 ms drain
    /// -- and it started every cycle EMPTY. Growing a hash table from zero to
    /// 1.4 M entries reallocates and rehashes about twenty times, moving
    /// roughly 2.8 M entries in total on top of the 1.4 M real inserts. That
    /// growth, not hashing, is the cost: `moving-young-throughput-20260726`'s
    /// profile named `RawTable::reserve_rehash` at 8.3 % of the whole process
    /// beside `HashMap::insert` at 40.7 %.
    ///
    /// The obvious-looking alternative -- a sorted `Vec<(from, to)>`, push on
    /// the collector side and binary-search on the consumer side -- is worse
    /// here, and the call sites say so: `pointer_map.get(..)` appears 127
    /// times against 18 `insert`s. It is a read-mostly structure with point
    /// lookups, so trading O(1) hashing for ~21 cache-missing probes per `get`
    /// would pay for a cheaper build with a more expensive everything-else.
    /// Pre-sizing takes the growth cost out without touching the shape any
    /// consumer depends on.
    ///
    /// A hint, never a bound: a cycle with more survivors than the last one
    /// simply grows as before. Starts at zero, which reproduces exactly the
    /// old behaviour on the first collection.
    pub prev_survivor_count: std::sync::atomic::AtomicUsize,
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
    pub young_spill_pressure: std::sync::atomic::AtomicBool,
    /// Anti-livelock state for the young-GC trigger: the `minor_gc_count` the
    /// trigger last sampled a post-collection live figure at, and the live
    /// floor derived from it (`0` = no floor, use the ordinary threshold).
    ///
    /// The young collector that runs while compiled code exists is the
    /// NON-MOVING sweep, which reclaims dead spans in place and can only
    /// promote selectively — so it cannot shrink a live young set that has
    /// grown past the trigger. Without a floor, `needs_gc` then answers true
    /// on essentially every allocation forever: the VM sweeps continuously,
    /// each sweep frees a sliver, and the mutator stops making progress.
    /// Measured on `RequestMappingMessageConversionIntegrationTests`
    /// (spring-webflux): wedged at test 151/160 with live=511 MB against a
    /// 512 MB trigger, ~0 forward progress over 8+ minutes at the default
    /// 4 GB heap, while the same run completes in 9m26s at 6 GB (where the
    /// bigger semi-space keeps live under the trigger). See
    /// [`Self::needs_gc_with_jit_allocation_frame`].
    pub young_trigger_seen_gc_count: std::sync::atomic::AtomicU64,
    pub young_trigger_floor: std::sync::atomic::AtomicUsize,
    /// gengc-round1-core (2026-09-20): did the young cycle that just ran
    /// REFUSE to do anything?
    ///
    /// [`Self::collect_garbage_inner`] has two "nothing destructive has
    /// happened yet, skip this collection" exits — to-space cannot cover
    /// from-space, and the object-start walk could not complete (which the T-3
    /// reserved-TLAB-tail refusal also routes through). Both say "over-retain;
    /// retried next cycle", but there was no mechanism that made the retry
    /// wait for a *next* cycle: the arena is byte-for-byte what it was on
    /// entry, so `needs_gc` answers `true` again on the very next allocation
    /// and the VM re-enters the pause — safepointing every mutator, taking all
    /// three arena locks, draining the card buffers and re-running the O(young
    /// allocated) object-start walk — only to refuse again. That is a GC storm
    /// whose only brake is the GC-overhead limit.
    ///
    /// Set by [`Self::note_skipped_young_cycle`], which also arms
    /// [`Self::young_trigger_floor`] so the next collection waits for genuine
    /// growth (the same anti-livelock floor a no-progress non-moving sweep
    /// already arms). Read by the two public entry points so a refusal does
    /// not feed a zero-length "pause" into the pause-goal loop, where it would
    /// clear a pending halving trial and silently defeat
    /// [`next_young_trigger`]'s revert rule.
    ///
    /// 2026-09-21: each of the three causes now also records its own
    /// `gc_metrics::decision_reason` code —
    /// `SKIPPED_YOUNG_TO_SPACE_UNDERSIZED`, `SKIPPED_YOUNG_WALK_INCOMPLETE`
    /// and `SKIPPED_YOUNG_RESERVED_TLAB_TAILS` — so a refusal is a number an
    /// operator can read back and not only a `warn` in a log. This flag stays
    /// the in-process one-bit answer to "did the cycle that just ran refuse?",
    /// which is what `verbose_gc_end` and the pause-goal loop need and what
    /// the reason code (process-wide, last-cycle) cannot give them. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub young_cycle_skipped: std::sync::atomic::AtomicBool,
    /// gcd d9/c — did the last COMPLETED young cycle leave young unable to
    /// drain? `true` after a non-moving sweep whose promotion walk was gated
    /// off (`sweep_young_non_moving`: `promotion_age_reachable && !selective_on`,
    /// in practice an un-rewritable peer -- a refusing helper window or a
    /// frozen take-over peer); `false` after every moving or pinned-copy cycle
    /// and every promoting sweep. A refused cycle leaves it as it was. Read
    /// through [`Self::young_drain_blocked`] and
    /// [`Self::young_starved_behind_blocked_drain`]; see the former for why.
    pub last_cycle_drain_blocked: std::sync::atomic::AtomicBool,
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
    pub jit_tlab_skip_regions: Mutex<Vec<(usize, usize)>>,

    /// This heap's entry in the process-global live-heap registry.
    ///
    /// Purely an RAII counter — see [`RELOCATABLE_HEAPS_LIVE`]. It exists
    /// because this heap publishes into two single-tenant global tables, and a
    /// SECOND live heap makes the frame-band verifier's residency test answer
    /// `false` for every address belonging to whichever heap lost the slot.
    /// Held as a field rather than registered by a matched pair of calls so the
    /// count cannot drift on an early return or an unwind.
    pub _bounds_registration: RelocatableHeapRegistration,
    /// The helper thread zeroing the semi-space the last moving cycle
    /// evacuated (gen-gc-five, 2026-09-02). `Arena::reset` used to `memset`
    /// the whole allocated from-space INSIDE the pause — up to the semi-space
    /// capacity per cycle, and every one of those bytes is zeroed again by
    /// the TLAB refill or the old-gen allocator before an object lands on
    /// it. The evacuated arena is the next cycle's to-space: no mutator
    /// allocates into it, so the wipe can run while they execute. It is
    /// joined at the top of every collection, before anything reads or writes
    /// that arena, and by `Drop`.
    pub evacuated_wipe: Mutex<Option<cratonvm_types::latched_thread::LatchedJoinHandle<()>>>,
    /// State of the pause-goal feedback loop between collections; see
    /// [`next_young_trigger`].
    pub young_trigger_feedback: Mutex<TriggerFeedback>,
    /// Set while [`Self::evacuated_wipe`] is running. `is_object_address`
    /// then declines the inactive semi-space: a conservative candidate there
    /// could otherwise parse a header the wipe is halfway through.
    pub wipe_in_flight: std::sync::atomic::AtomicBool,
    /// `--verbose:gc`: emit one `[GC] generational: …` line per collection.
    ///
    /// gengc-round2-core2 (2026-09-20). `VmHeap::enable_gc_logging` has three
    /// arms; G1 and ZGC each flip a per-collector flag that a per-pause
    /// emitter reads, and the Generational arm turned nothing on because this
    /// heap had no flag and no gated statement to flip it for. What the user
    /// got was worse than silence: an affirmative `tracing::info!` followed by
    /// no per-collection output for the rest of the run, which reads as "no
    /// collections happened". See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// PER-HEAP, not a process global, and deliberately so: two heaps can be
    /// live at once (see [`RELOCATABLE_HEAPS_LIVE`]) and the flag belongs to
    /// the one the operator asked about. Written by
    /// [`Self::set_verbose_gc`], read once per collection by
    /// [`Self::verbose_gc_begin`]. `Relaxed` throughout — this gates a
    /// diagnostic print, so a store racing a collection may land on either
    /// side of it and both answers are correct.
    ///
    /// Note what this flag does NOT gate: the shutdown census
    /// (`[GC] generational: minor=N major=M` and the counter block beside it)
    /// is emitted by `VmHeap::print_gc_summary` and was never missing. The gap
    /// was only ever the per-collection line, which is what `-verbose:gc`
    /// means on HotSpot and what the other two backends already emit.
    pub gc_log_enabled: std::sync::atomic::AtomicBool,
    /// Pause accounting for completed (non-refused) collections, kept by the
    /// two public entry points: `[count, total_us, total_ms_ceiled]`.
    ///
    /// gen r4/plumbing (2026-09-23). This backend kept NO accumulated pause
    /// time at all, so `GarbageCollectorMXBean.getCollectionTime()` answered
    /// with the collection COUNT in its millisecond slot (see
    /// `native-builtins/src/jmx.rs`) — a 400 ms pause and a 40 µs one both
    /// added "1 ms". G1 has kept `total_pause_us` for months.
    ///
    /// The third word rounds each pause UP to a whole millisecond before
    /// summing. That is the unit JMX reports in, and the rounding is what keeps
    /// the one property callers already depend on: H2's
    /// `Utils.collectGarbage()` spins until `getCollectionTime()` moves, so a
    /// sub-millisecond collection must still advance it. Summing whole
    /// milliseconds per collection guarantees that and over-reports by at most
    /// one millisecond per collection, where the count-as-time it replaces was
    /// wrong by the whole pause.
    ///
    /// A refused cycle (`young_cycle_skipped`) is excluded from all three, for
    /// the same reason `minor_gc_count` excludes it: nothing was collected, and
    /// a time that grows while the count does not would read as a phantom
    /// collection.
    pub pause_totals: [AtomicU64; 3], // gen r4/plumbing
    /// `[armed, consumed, consumed_stale, dropped_on_divert]` for the
    /// promote-on-pressure arm, over the heap's life.
    ///
    /// gen r4w4/young4 (2026-09-24): the arm is no longer CARRIED across a
    /// non-moving cycle (the `promote_pressure_carried` bit that did so is
    /// gone). `consumed` now counts a non-moving sweep that tenured on the arm
    /// as well as a moving cycle; `dropped_on_divert` counts a divert under
    /// `CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES` and a sweep that could not
    /// promote that cycle; `consumed_stale` can no longer move and is kept as
    /// the regression detector. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    /// Read through [`Self::promote_pressure_census`].
    pub promote_pressure_census: [AtomicU64; 4], // gen r4w2/youngpolicy
    /// `CRATONVM_DBG_GC_STRESS` anti-livelock state (gen r4w2/youngmark,
    /// 2026-09-23, for the NON-MOVING young path; since gen r4w3/young2 the
    /// sampled floor serves the moving path too). See [`gc_stress_decision`].
    ///
    /// `[0]` — `minor_gc_count` as it stood at the end of the most recent
    /// non-moving young cycle (`u64::MAX` = none yet). `[1]` — the
    /// `minor_gc_count` the stress floor was last sampled at.
    pub young_stress_epochs: [AtomicU64; 2], // gen r4w2/youngmark
    /// The sampled stress floor (live young bytes that must be exceeded before
    /// the stress trigger fires again); `0` = none. See [`gc_stress_decision`].
    pub young_stress_floor: std::sync::atomic::AtomicUsize, // gen r4w2/youngmark
    /// Net TLAB carve bytes (carved minus returned tails) as they stood when
    /// the stress floor was last sampled — the second epoch snapshot the
    /// carve-aware stress bound needs. See [`gc_stress_carve_bound`].
    pub young_stress_net_carved_at_epoch: AtomicU64, // gen r4w6/tlab6
    /// `CRATONVM_GEN_TLAB_TAIL_SINK`, latched when the heap is built (one read
    /// per VM, never per retire). See [`Self::return_tlab_tail`].
    pub tlab_tail_sink: bool, // gen r4w6/tlab6
    /// `-Xms`'s young floor: the larger of the two semi-space prefixes
    /// [`Self::commit_initial_heap`] committed (the from-space's share), `0`
    /// when no `-Xms` was given. [`Self::uncommit_evacuated_young`] never
    /// leaves the evacuated semi-space committed below it. gen r5w5/sizer9,
    /// `gengc-r5w3-oldgen7-young-uncommit-ignores-the-xms-floor`.
    pub young_xms_floor: AtomicUsize, // gen r5w5/sizer9
    /// `CRATONVM_GEN_PINNED_COPY_DEFRAG` (opt-in, with
    /// `CRATONVM_GEN_PINNED_YOUNG_COPY`), latched when the heap is built: the
    /// pinned copy takes a term-4 cycle only when the previous non-moving
    /// sweep left young fragmented ([`young_space_fragmented`]). gcf f2/y.
    pub pinned_copy_defrag: bool, // gcf f2/y
    /// gcf f2/y: the last non-moving sweep measured young fragmented. Set by
    /// the sweep, consumed (swapped to `false`) by the next term-4-alone
    /// cycle's pinned-copy candidacy. Inert unless [`Self::pinned_copy_defrag`].
    pub young_defrag_pending: std::sync::atomic::AtomicBool, // gcf f2/y
    /// gcf f2/y: `[sweeps measured, sweeps that left young fragmented,
    /// term-4 cycles the gate admitted to the pinned copy, term-4 cycles it
    /// sent to the sweep]` (`[GC] pinned_copy_defrag:`), then gch h2/yg's
    /// `[large-request fragmentation misses that armed the gate]`
    /// ([`Self::note_young_fragmentation_miss`]).
    pub pinned_copy_defrag_census: [AtomicU64; 5], // gcf f2/y, gch h2/yg
    /// `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK` (opt-in), latched when
    /// the heap is built: a term-4 cycle the promotion-OOM guard alone kept
    /// from the pinned copy is offered to it (gcj j1/yg,
    /// [`gen_pinned_copy_takes_promotion_risk_enabled`]).
    pub pinned_copy_takes_promotion_risk: bool, // gcj j1/yg
    /// gcj j1/yg: `CRATONVM_GEN_PINNED_COPY_SURVIVOR_RESERVE`, read at
    /// construction ([`pinned_survivor_reserve`]).
    pub pinned_copy_survivor_reserve: bool, // gcj j1/yg
    /// gcj j1/yg: term-4 cycles and the first term that kept each from the
    /// pinned copy, indexed by the `T4O_*` constants (`[GC] young_term4_offer:`,
    /// [`Self::term4_offer_census_line`]).
    pub term4_offer_census: [AtomicU64; T4O_LEN], // gcj j1/yg
    /// gcj j2/yg2: `CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY` (default ON, `=0`
    /// kills), latched when the heap is built: the non-moving sweep keeps the
    /// known-zero claim for the previous set's bytes still on the free list,
    /// and the TLAB tail sink returns tails without forgetting the set
    /// ([`gen_young_known_zero_carry_enabled`]).
    pub young_known_zero_carry: bool, // gcj j2/yg2
    /// t1 wave 8, lane YGZERO: `CRATONVM_GEN_TLAB_TAIL_DIRTY_DECLINES`
    /// (default ON, `=0` kills), latched when the heap is built
    /// ([`gen_tlab_tail_dirty_declines_enabled`], `return_tlab_tail`).
    pub tlab_tail_dirty_declines: bool, // t1 w8 YGZERO
    /// JIT round 23 wave 6, lane L4: `CRATONVM_GEN_SWEEP_LAZY_ZERO` (default
    /// ON, `0` kills), latched when the heap is built: the non-moving sweep
    /// leaves its reclaimed spans to the lazy-dirty set and its mirror instead
    /// of zeroing them in the pause ([`gen_sweep_lazy_zero_enabled`]).
    pub sweep_lazy_zero: bool, // r23 w6
    /// gcj j3/yg3: the smallest useful young TLAB for this heap's VM, the floor
    /// of the smallest-whole refill and the tail sink's `min_free`
    /// (`CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR`, [`Self::set_min_tlab_floor`]).
    /// `tlab::min_tlab_size()` until the VM installs its typed minimum.
    pub min_tlab_floor: usize, // gcj j3/yg3
    /// gcf f3/q: the young-premium cuts' engagement counters, indexed by the
    /// `YP_*` constants (`[GC] young_premium:`, [`Self::young_premium_census_line`]).
    pub young_premium_census: [AtomicU64; YP_LEN], // gcf f3/q
    /// `CRATONVM_GEN_WEDGED_YOUNG_TRIGGER` (default ON), latched when the heap
    /// is built: the young trigger's floor rises to the non-moving trigger
    /// while the old generation is wedged ([`young_trigger_floor_judged`]).
    pub young_wedged_trigger: bool, // gcd d9/b
    /// gcf f7/c: HotSpot Serial's eden for this heap's `-Xmx` / young flags
    /// ([`serial_eden_bytes`]), in bytes, when
    /// `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` was on as the heap was built
    /// ([`Self::arm_serial_eden_trigger`]); `0` = off. Fixed for the heap's
    /// life. When non-zero it replaces the percent knob in
    /// [`Self::configured_young_trigger`] and the semi-space in
    /// [`Self::tenuring_eden_for`].
    pub young_serial_eden: usize, // gcf f7/c
    /// gcj j1/yg: HotSpot Serial's survivor space for this heap's `-Xmx` and
    /// young flags (`Y / (SurvivorRatio + 2)`, aligned), armed with
    /// [`Self::young_serial_eden`]; the floor of the pinned copy's survivor
    /// reserve ([`pinned_survivor_reserve`]). 0 when the Serial-shaped
    /// trigger did not arm.
    pub young_serial_survivor: usize, // gcj j1/yg
    /// gcj j1/yg: the bytes the last pinned in-place cycle kept in young or
    /// promoted only because its free spans ran out
    /// ([`Self::note_pinned_young_cycle`]).
    pub pinned_survivor_estimate: AtomicUsize, // gcj j1/yg
    /// gkr: eager cycles left after a dense lazy object-start grid
    /// ([`LazyStartsBackoff`]).
    pub lazy_starts_backoff: LazyStartsBackoff, // gkr
    /// gcj yg3-residuals R3: the Serial geometry this heap was built with
    /// ([`gen_serial_young_geometry_enabled`]); `None` = the semi-space pair
    /// of before.
    pub young_geometry: Option<cratonvm_gc_core::gen_young_sizing::SerialPairGeometry>,
    /// gcj yg3-residuals R3: young live bytes sampled at the first trigger
    /// check after each young collection ([`geometry_young_trigger`]'s
    /// `carry`), and the collection count it was sampled at.
    pub young_geometry_carry: AtomicUsize,
    pub young_geometry_seen_gc: AtomicU64,
    /// gci i45/sc: `-XX:SurvivorRatio` for this heap (HotSpot's 8 unless the
    /// launcher carried one, `VmConfig::survivor_ratio`), set before the heap
    /// is shared ([`Self::set_survivor_ratio`]): the ratio Serial's eden
    /// ([`serial_eden_bytes_at`]), the survivor reserve
    /// ([`serial_survivor_reserve_at`]) and the tenuring target read.
    pub serial_survivor_ratio: usize, // gci i45/sc
    /// gci i22/ed: HotSpot Serial's CURRENT eden when
    /// `CRATONVM_GEN_SERIAL_EDEN_INITIAL` armed this heap
    /// ([`Self::follow_initial_heap_eden`]); `0` = not following, and
    /// [`Self::young_serial_eden`] (the maximum) is the eden. Read through
    /// [`Self::serial_eden_current`]; written only at construction and inside
    /// a pause ([`Self::serial_eden_follow_after_cycle`]).
    pub young_serial_eden_now: AtomicUsize, // gci i22/ed
    /// gci i22/ed: HotSpot's generation sizes the follow models
    /// ([`SerialEdenFollow`]); inert while `young_serial_eden_now` is 0.
    pub serial_eden_follow: Mutex<SerialEdenFollow>, // gci i22/ed
    /// gci i1/yg: HotSpot Serial's eden for this heap's `-Xmx` / young flags
    /// when `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK` was on as the heap was built
    /// ([`Self::arm_serial_eden_trigger`]); `0` = the age clock is off and
    /// every young cycle ages its survivors by one, as before. Fixed for the
    /// heap's life.
    pub young_age_clock_eden: usize, // gci i1/yg
    /// gci i1/yg: the age clock's state ([`AgeClock`]); inert while
    /// [`Self::young_age_clock_eden`] is 0.
    pub age_clock: Mutex<AgeClock>, // gci i1/yg
    /// gci i3/yg: the age clock also charges old-direct allocations
    /// (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`,
    /// [`gen_serial_eden_age_clock_old_direct_enabled`]); set only when the
    /// clock arms. Fixed for the heap's life.
    pub young_age_clock_old_direct: bool, // gci i3/yg
    /// gci i3/yg: old-direct bytes allocated since the last
    /// [`Self::age_clock_begin`] ([`Self::age_clock_charge_old_direct`]).
    /// A relaxed add on the old-direct paths, only while
    /// [`Self::young_age_clock_old_direct`] is set.
    pub age_clock_old_direct_pending: AtomicUsize, // gci i3/yg
    /// gci i3/yg: set by the VM's allocation-failure door right before it asks
    /// for a collection ([`Self::mark_next_young_cycle_forced`]); read and
    /// cleared by the two public collection doors
    /// ([`Self::feed_pause_goal_loop`]). Tells the pause-goal loop a cycle was
    /// forced, not fired by the trigger (`CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED`).
    pub young_cycle_forced: std::sync::atomic::AtomicBool, // gci i3/yg
    /// gci i4/yg: `CRATONVM_GC_YOUNG_PAUSE_MS` as this heap was built
    /// ([`young_pause_goal_ms_from_env`]); `0` = no goal. Per heap: it was a
    /// process-wide `OnceLock`, so every heap used the FIRST heap's goal
    /// (`gci-i3yg-young-collection-review-findings` item 1).
    pub young_pause_goal_ms: u64, // gci i4/yg
    /// gci i4/yg (`CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`, opt-in,
    /// [`gen_young_exhausted_refill_collects_enabled`]): a JIT-guarded TLAB
    /// refill that young could not serve at all asks
    /// [`Self::young_exhausted_refill_wants_collection`] for a young collection
    /// instead of sending the object to the old generation. Fixed for the
    /// heap's life at construction.
    pub young_exhausted_refill_collects: bool, // gci i4/yg
    /// gci i22/dr (`CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`, opt-in,
    /// [`gen_tlab_refill_smallest_whole_enabled`]): young TLAB refills take
    /// the smallest whole free block first
    /// ([`Self::refill_smallest_whole_block`]). Fixed for the heap's life at
    /// construction.
    pub tlab_refill_smallest_whole: bool, // gci i22/dr
    /// gci i4/yg: the young epoch (`minor_gc_count`) the first TLAB refill of
    /// the current young cycle saw, and `young_tlab_carved_bytes` at that
    /// refill ([`Self::note_young_refill_epoch`]); `u64::MAX` before any.
    /// Written only while [`Self::young_exhausted_refill_collects`] is set.
    pub young_refill_epoch: AtomicU64, // gci i4/yg
    pub young_refill_carved_at_epoch: AtomicU64, // gci i4/yg
    /// gci i4/yg: the young epoch whose exhausted refill last asked for (and
    /// was granted) a collection; at most one per epoch.
    pub young_exhausted_fired_epoch: AtomicU64, // gci i4/yg
    /// gci i4/yg: `[granted, no_progress, repeat]` verdicts of
    /// [`Self::young_exhausted_refill_wants_collection`], printed on
    /// `[GC] gen-alloc:` as `gen_refill_exhausted_*`.
    pub young_exhausted_census: [AtomicU64; 3], // gci i4/yg
    /// gc wave 9 (lane gcC): HotSpot Serial's eden, in bytes, when
    /// `CRATONVM_GEN_AGE_BY_SERIAL_EDEN` was on as the heap was built
    /// ([`Self::arm_serial_eden_trigger`]); `0` = off. When non-zero a young
    /// cycle ages its survivors only once per eden of young allocation
    /// ([`serial_eden_age_epoch`], [`Self::open_young_age_epoch`]), and
    /// adaptive tenuring sizes its survivor target from this eden.
    pub young_age_eden: usize,
    /// gc wave 9 (lane gcC), under [`Self::young_age_eden`]: `[credit carried
    /// since the last aging cycle, young live bytes when the last cycle (or
    /// refused attempt) ended, completed aging cycles, completed held
    /// (non-aging) cycles]`. Untouched when unarmed.
    pub young_age_epoch: [AtomicUsize; 4],
    /// gc wave 9 (lane gcC): whether the young cycle in progress ages its
    /// survivors ([`Self::cycle_promotion_age`]). Always `true` unless
    /// [`Self::young_age_eden`] is armed, so the default path is unchanged.
    pub young_age_this_cycle: std::sync::atomic::AtomicBool,
    /// gcg h1/ob: `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` (default
    /// `YOUNG_GC_THRESHOLD_PERCENT`), read once as this heap was built
    /// ([`young_trigger_percent`]); the configured trigger and the tenuring
    /// eden derive from it. Per heap: it was a process-global `OnceLock`.
    pub young_trigger_pct: usize, // gcg h1/ob
    /// `[raised, deferred, drain_blocked]`: post-collection floor samples that
    /// [`young_trigger_floor_judged`] raised to the cap / deferred, because
    /// young could not drain (old generation wedged, or gcd d9/c's blocked
    /// drain), and the stand-downs owed to the blocked drain alone
    /// ([`Self::young_drain_stood_down_cap`]). Printed as
    /// `[GC] young_trigger_wedged:`.
    pub young_wedged_census: [AtomicU64; 3], // gcd d9/b
    /// The allocation-failure ladder's work over this heap's life, fed by the
    /// VM (`gc_and_alloc.rs`) through [`Self::note_oome_ladder`]; slots in
    /// [`oome_ladder`]. Printed as `[GC] oome_ladder:`.
    pub oome_ladder_census: [AtomicU64; oome_ladder::LEN], // gcd d9/b
    /// The subset of [`Self::pause_totals`] spent in collections that ALSO
    /// reclaimed the old generation (`gc_quiescence::old_gen_reclaimed_last_cycle`
    /// — the mark-compact major GC or the in-place old sweep):
    /// `[count, total_us, total_ms_ceiled]`.
    ///
    /// gen r4w2/obs (2026-09-23). What splits the one JMX collector bean into
    /// HotSpot's two: a Serial-shaped heap reports a young collector (`Copy`)
    /// and a full collector (`MarkSweepCompact`), and a full collection counts
    /// under the full collector ONLY. `pause_totals - old_pause_totals` is the
    /// young bean, so the two beans' counts always sum to the collections this
    /// heap completed and a cycle that ran both generations is counted once.
    /// `HeapStats` cannot answer this: `minor_gc_count` is bumped by cycles that
    /// also ran old gen, and `major_gc_count` by more than one old-gen path per
    /// cycle. See `docs/feature-designs/generative-gc-architecture.md`.
    pub old_pause_totals: [AtomicU64; 3], // gen r4w2/obs
    /// gcg g6/yg: `(count, us)` of the young halves of pauses that also ran an
    /// unrequested major ([`Self::escalation_mark`]); [`Self::jmx_collectors`]
    /// books them under `Copy`.
    pub escalated_young_totals: [AtomicU64; 2], // gcg g6/yg
    /// gcg g6/yg: the instant the current young cycle started an UNREQUESTED
    /// major (the moving path's Phase 5 occupancy major, or the non-moving
    /// sweep's old-generation sweep). Per heap, never ambient: cleared at both
    /// cycle entries, set by [`Self::note_escalation_mark`], taken by
    /// `note_collection_pause` right after the cycle (all inside the pause).
    /// gcg h1/ob: with the old generation's occupancy at the decision and the
    /// booked split the VM takes ([`EscalationState`]).
    pub escalation_mark: Mutex<EscalationState>, // gcg g6/yg, h1/ob
    /// Pool occupancy right after the most recent completed collection, for
    /// `MemoryPoolMXBean.getCollectionUsage()`: `[eden_used, tenured_used,
    /// recorded]` (`recorded` is 0 until the first collection completes, when
    /// the JMM answer is "undefined").
    ///
    /// gen r4w2/obs (2026-09-23). Written by [`Self::note_collection_pause`]
    /// with no arena lock held. Eden is the from-space's `used - free_list`
    /// (the same live measure the pools report), not the bump cursor.
    ///
    /// gen r5w3/obs7 (2026-09-26): `[0]` is now read as the young SURVIVORS
    /// of the last collection (`Survivor Space`'s usage and collection usage;
    /// eden's collection usage is 0), and `[1]` moves only on a collection
    /// that reclaimed the old generation (HotSpot Serial's rule for the
    /// tenured pool). See [`Self::jmx_memory_pools`].
    pub last_collection_usage: [AtomicUsize; 3], // gen r4w2/obs
    /// gci i14/mp finding 3: `[eden, survivor, tenured]` committed bytes at the
    /// end of the last collection that affected each pool (HotSpot's
    /// `getCollectionUsage().getCommitted()`); tenured is valid only once
    /// `last_collection_usage[2]` has [`TENURED_COLLECTION_STAMPED`].
    pub last_collection_committed: [AtomicUsize; 3],
    /// Last coherent Serial reporting estimate; never holds an arena lock.
    pub serial_pool_snapshot: Mutex<Option<[crate::serial_pool_accounting::PoolUsage; 3]>>,
    pub serial_pool_snapshot_fallbacks: AtomicU64,
    pub serial_collection_used: [AtomicUsize; 3],
    /// `[stamp, young_live_bytes]` of the most recent NON-MOVING young cycle:
    /// `stamp` is `minor_gc_count` as that cycle left it (`u64::MAX` = none
    /// yet), `young_live_bytes` the young live set it traced (survivors left
    /// in place plus bytes selective promotion moved out). Written by
    /// `sweep_young_non_moving`; read by
    /// [`Self::adapt_young_trigger_to_pause`], which latches on it instead of
    /// `GcStats::bytes_copied` when the stamp says the cycle it is judging
    /// was non-moving. Per heap, never a process global.
    ///
    /// gen r4w3/young2 (2026-09-23), see
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub non_moving_latch: [AtomicU64; 2], // gen r4w3/young2
    /// Each JMX collector bean's last collection (`getLastGcInfo()`) and the
    /// `GarbageCollectionNotificationInfo` notifications not yet delivered.
    /// Written by the VM's per-collection accounting, drained outside the
    /// pause. gen r4w3/obs2 (2026-09-23); see `gc_metrics`'s "GC notifications".
    pub gc_notifications: cratonvm_gc_core::gc_metrics::GcNotificationQueue, // gen r4w3/obs2
    /// The conservative-JIT-root divert census — see
    /// [`Self::conservative_divert_census`]. gen r4w4/young4 (2026-09-24).
    pub conservative_divert_census: [AtomicU64; CONSERVATIVE_DIVERT_CENSUS_LEN], // gen r4w4/young4
    /// The pinned in-place young copy's census — see
    /// [`Self::pinned_young_census`]. gen r4w5/pinned5 (2026-09-24).
    pub pinned_young_census: [AtomicU64; PINNED_YOUNG_CENSUS_LEN], // gen r4w5/pinned5
    /// gcg g1/y: `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`'s engagement --
    /// `[exact cycles, seeds, plan objects, dropped objects, dropped bytes]`;
    /// gcj j2/pn2: then `[filter-drop cycles, their dropped objects, their
    /// dropped bytes, refused-forward kept objects]`.
    /// See [`Self::pinned_exact_roots_census_line`].
    pub pinned_exact_census: [AtomicU64; 10], // gcg g1/y, gcj j2/pn2, gcj j2/x1
    /// The young evacuator's refusal ledger, per heap (gcd d3/m; two process
    /// statics until then). See [`ForwardRefusalLedger`] and
    /// [`Self::forward_refusal_reason`].
    pub forward_refusals: Arc<ForwardRefusalLedger>, // gcd d3/m
    /// The pinned copy's take-over arm
    /// (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`). See
    /// [`Self::pinned_takeover_census`]. gcd d3/m.
    pub pinned_takeover_census: [AtomicU64; 4], // gcd d3/m
    /// The pinned copy's unproven-frame arm
    /// (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`), as
    /// [`Self::pinned_takeover_census`]. See [`Self::pinned_unproven_census`].
    pub pinned_unproven_census: [AtomicU64; 4], // gcj j1/yg
    /// Why the take-over arm declined. See
    /// [`Self::pinned_takeover_declines`]. gcd d5/s.
    pub pinned_takeover_declines: [AtomicU64; takeover_decline::LEN], // gcd d5/s
    /// gen r5w5/pin9: the absolute `(addr, len)` spans, sorted and disjoint,
    /// that the last pinned in-place young cycle took back (dead objects and
    /// the old copies of moved survivors, quarantined ones included). Set by
    /// that cycle, cleared when the next collection starts, empty after any
    /// other cycle. Read through [`Self::young_vacated_probe`].
    pub young_in_place_vacated: Mutex<Vec<(usize, usize)>>, // gen r5w5/pin9
    /// gcj j1yg-pause: the moving cycle's dirty-card edge list and their young
    /// values, kept (cleared, capacity intact) between cycles. A JIT-warm
    /// javac cycle fills ~700 K edges (~22 MB); a fresh buffer every pause
    /// paid its page faults inside the pause.
    pub card_scan_scratch: Mutex<(Vec<(ObjectRef, usize, usize)>, Vec<usize>)>, // gcj j1yg-pause
    /// gcj pcp: the parallel moving copy's per-worker shards, kept emptied
    /// between cycles (`gen_evac::EvacShard::recycled`) so their lists start
    /// each pause with the capacity the last one grew to.
    pub evac_shard_scratch: Mutex<Vec<crate::gen_evac::EvacShard>>, // gcj pcp
    /// gcj pcp: the parallel dirty-card scan's per-group buffers
    /// ([`CardScanPart`]), kept emptied between cycles.
    pub card_scan_parts: Mutex<Vec<CardScanPart>>, // gcj pcp
    /// gcj pcp: the sorted pointer map's buffers, handed back when the VM
    /// drops a pause's map (`CRATONVM_GEN_POINTER_MAP_SORTED`).
    pub sorted_map_buffers: std::sync::Arc<cratonvm_types::pointer_map::SortedBufferPool>, // gcj pcp
    /// gcj j1yg-pause round 3: the next pause PROMISED to stay minor
    /// ([`Self::decide_next_pause_minor`]); taken by its trigger decision,
    /// which defers an occupancy-only major to the pause after.
    pub minor_promised: std::sync::atomic::AtomicBool, // gcj j1yg-pause r3
    /// gcj j1yg-pause round 3: majors a promise deferred (census).
    pub minor_promise_deferrals: AtomicU64, // gcj j1yg-pause r3
    /// gcj j1yg-pause round 3: the last trigger decision deferred a major on a
    /// promise; the next pause may not promise (so a deferred major runs no
    /// later than the pause after).
    pub minor_promise_just_deferred: std::sync::atomic::AtomicBool, // gcj j1yg-pause r3
    /// The unrecorded-precise-root refusal's census, gcd d2/h (2026-09-27):
    /// `[cycles_refused, failed_open, roots_sum, ledger_guarded_cycles]` (the last, gcf f4/y:
    /// conservative cycles guarded under `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER`), then
    /// gch h1/yp's `[excluded_cycles, excluded_roots_sum]`: guarded cycles on which the
    /// exclusion set held at least one root that would otherwise have been an offender, and
    /// the sum of those roots. See [`Self::unrecorded_young_root_refuses_cycle`],
    /// [`Self::unrecorded_young_root_census`] and
    /// [`Self::unrecorded_young_root_exclusion_census`]. Then gch h2/yg's
    /// `[refusal_streak_now, refusal_streak_max]`: consecutive refused guard
    /// decisions (a non-refusing decision resets the first), and the longest
    /// such run ([`Self::unrecorded_young_root_max_streak`]). Then gci i10/gr's
    /// `cards_guarded_cycles` (slot 8): moving cycles on which the dirty-card /
    /// overlay value guard ran (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`;
    /// [`Self::unrecorded_young_root_cards_guarded_cycles`]).
    pub unrecorded_root_census: [AtomicU64; 9], // gcd d2/h, gcf f4/y, gch h1/yp, gch h2/yg, gci i10/gr
    /// The lowest unrecorded from-space root of the last moving attempt that
    /// met one (`0` = none yet). A cycle refuses only for an address other
    /// than this one, so one persistent stale root costs one skipped cycle,
    /// not every cycle. gcd d2/h.
    pub last_unrecorded_root: AtomicUsize, // gcd d2/h
    /// gch h2/yg (`gcd-d1e-proposal-every-moving-cycle-pins-blocked-peer-interior-words`):
    /// the measurement that proposal asks for, kept flag on or off. Indexed by
    /// the `BPI_*` constants; see [`Self::blocked_peer_interior_census`].
    pub blocked_peer_interior_census: [AtomicU64; BPI_LEN], // gch h2/yg
    /// gch h2/yg (`gch-h1yp-raw-jni-locals-count-turns-the-pinned-copy-off-for-the-vms-life`):
    /// of the pinned copy's `ledger_incomplete` declines
    /// (`pinned_young_census[3]`), those on which a thread of the VM held raw
    /// JNI local references (`gc_quiescence::raw_jni_locals_open`), whatever
    /// else also made the read incomplete.
    pub pinned_ledger_incomplete_raw_jni: AtomicU64, // gch h2/yg
    /// gch h4/wr (`gch-h3jx-gen-true-root-major-keeps-unstarted-compat-threads-one-cycle-FIXED-20261012`,
    /// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9): the PRE-collection
    /// addresses of the watched young survivors (every address a weak table
    /// hands back after the pause: `gc_quiescence::watched_referents_snapshot`)
    /// that the last non-moving true-root major's trace did NOT reach. The
    /// young phase kept each one in place only for a dead old holder's card,
    /// so it is dead: every post-collection survival question about it answers
    /// "did not survive" (`VmHeap::watched_pre_gc_addr_survived`,
    /// `pre_gc_addr_did_not_survive`, the String table's sweep), which is what
    /// lets the major free its old referents without a weak table handing it
    /// back (the gcd d5/r use-after-free). Sorted ascending. Written at the end
    /// of `run_non_moving_young_cycle`'s major, cleared when the next
    /// collection starts: from that major to the next young collection the
    /// addresses still hold those same dead objects (nothing frees or reuses a
    /// kept-in-place young block before a young collection), so the verdict
    /// cannot outlive its objects. Per heap, never a process global.
    pub true_root_watched_left_out: Mutex<Vec<usize>>, // gch h4/wr
    /// gch h4/wr: the length of the above, so the per-address survival
    /// questions (one per tracked reference, per pooled String) take no lock
    /// on the common pause, where it is 0.
    pub true_root_watched_left_out_len: AtomicUsize, // gch h4/wr
    /// gch h4/wr: the run's total of the above (on no `[GC]` line; the
    /// `CRATONVM_DBG_WATCHREF` line and the unit tests read it).
    pub true_root_watched_left_out_total: AtomicU64, // gch h4/wr
    /// gci i12/sx: `CRATONVM_DBG_IHASH_WITNESS` (i11/jd's identity-hash
    /// witness), read once when this heap is built instead of in a process
    /// `OnceLock`. See [`gci_i11jd_ihash_witness_enabled`].
    pub ihash_witness: bool, // gci i12/sx
    /// Off-heap GPU VRAM and live-wrapper retention tracker for GC pacing.
    pub external_gpu: cratonvm_gc_core::gpu_critical::ExternalGpuPressureTracker,
}

// gch h2/yg: slots of `GenerationalHeap::blocked_peer_interior_census`.
/// Relocating young cycles (past the last refusal gate, not the forced
/// unit-test entry) on which the blocked-peer capture was examined.
pub const BPI_CYCLES: usize = 0;
/// ...whose capture held at least one young word that is not an object start
/// (the words the blocked-wake fold cannot rewrite).
pub const BPI_CYCLES_WITH_WORDS: usize = 1;
/// The sum of those words over the run.
pub const BPI_WORDS_SUM: usize = 2;
/// ...of the cycles with words, those whose plan took them (the pinned copy
/// or option B). The difference
/// `BPI_CYCLES_WITH_WORDS - BPI_CYCLES_PINNED` is the cycles that relocated
/// under a blocked peer's unrewritable word.
pub const BPI_CYCLES_PINNED: usize = 3;
pub const BPI_LEN: usize = 4;

/// Slots of [`GenerationalHeap::conservative_divert_census`].
pub const CONSERVATIVE_DIVERT_CENSUS_LEN: usize = 6;

/// One young cycle's view of the conservative JIT-root population at the
/// moment `divert_non_moving`'s term 4 (`unrewritable_conservative_jit_roots`)
/// sent it to the non-moving sweep. gen r4w4/young4 (2026-09-24).
///
/// The divert is all-or-nothing: ONE conservatively-discovered JIT root
/// anywhere in the process and the whole young cycle stops copying. Whether a
/// narrower rule could let the cycle copy — pinning only the objects the
/// conservative words name (Shenandoah / G1-style object or region pinning),
/// or precise derived-pointer tables so the interior words stop being
/// conservative at all — depends on numbers nobody had: how many objects the
/// pin registry actually names in young from-space on such a cycle, and how
/// often it names none (so the only obstacle left is the interior/derived
/// words the registry cannot see). See
/// `docs/feature-designs/generative-gc-architecture.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConservativeDivertSample {
    /// Conservative pins every thread published (`pinned_jit_roots_snapshot`).
    pub pins_total: u64,
    /// The subset inside young from-space `[base, base + used)` — the objects
    /// a pin-aware copying cycle would have to leave in place.
    pub pins_young: u64,
    /// Did the INITIATOR's band scan publish any unrewritable-region root
    /// (`unrewritable_jit_root_count`)? `false` on a cycle where every word
    /// the initiator could not rewrite named nothing.
    pub initiator_veto: bool,
}

impl ConservativeDivertSample {
    /// Count the pins that land in `[lo, hi)`.
    pub fn from_pins(pins: &[usize], lo: usize, hi: usize, initiator_veto: bool) -> Self {
        Self {
            pins_total: pins.len() as u64,
            pins_young: pins.iter().filter(|&&a| a >= lo && a < hi).count() as u64,
            initiator_veto,
        }
    }
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

/// gen r4w6/tlab6: the young from-space takes a retiring TLAB's unused tail
/// back instead of the retire burying it under an `int[]` filler. Attached per
/// buffer by the VM's refill site (`Tlab::attach_tail_sink`), never registered
/// process-wide. Off unless `CRATONVM_GEN_TLAB_TAIL_SINK` — see
/// [`GenerationalHeap::return_tlab_tail`].
impl cratonvm_gc_core::tlab::TlabTailSink for GenerationalHeap {
    fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool {
        self.return_tlab_tail(start, end)
    }
}

// ---------------------------------------------------------------------------
// Per-cycle copy tally.
//
// Phase H used to derive `bytes_promoted` / `objects_promoted` /
// `bytes_copied_young` / `objects_copied_young` by walking every
// `pointer_map` entry after the copy phase and dereferencing each destination
// header to recompute its size. On a workload with ~756 000 survivors per
// collection that walk is ~756 000 random header reads, and it measured
// 29 ms of a 424 ms median stop-the-world pause — to recompute a size
// `forward_object_impl` already had in hand.
//
// A thread-local is sound for the SERIAL copy phase and not merely convenient:
// `forward_object_impl` takes `&mut Arena` for both destinations, so that path
// is single-threaded by construction and the borrow checker enforces it.
//
// It is NOT sound for the parallel copy phase, and the fix is not to make this
// an atomic. `gen_evac`'s workers each tally into their own `EvacShard` — the
// same reason `forwards` is per-worker, i.e. this is a per-object write on the
// hottest path of the pause — and the driver folds every shard in with
// [`copy_tally_merge`] after the completion barrier. A worker thread's own
// thread-local is never read, which is exactly why it must never be written:
// a stale claim that "the copy phase is single-threaded" would have made these
// four counters silently under-report by however much the helpers copied.
//
// Layout: [bytes_promoted, objects_promoted, bytes_copied_young,
// objects_copied_young, fwd_reencounters, fwd_copies] — the last pair is
// `copy_tally_arm`'s (gated on `gcpause` on the serial path, counted
// unconditionally by `gen_evac`'s workers). The comment named only the first
// four until 2026-09-23 (gengc-round4-move).
// ---------------------------------------------------------------------------
thread_local! {
    pub static COPY_TALLY: std::cell::Cell<[u64; 6]> = const { std::cell::Cell::new([0; 6]) };
}

/// Record one copied object, classified by the arena it actually landed in
/// (promotion can fall back to to-space when old gen is full, so the
/// destination is the truth, not `should_promote`).
#[inline]
pub fn copy_tally_add(promoted: bool, bytes: u64) {
    COPY_TALLY.with(|t| {
        let mut a = t.get();
        let i = if promoted { 0 } else { 2 };
        a[i] += bytes;
        a[i + 1] += 1;
        t.set(a);
    });
}

/// Is the `forward_object` arm tally on? Diagnostic only, and it fires on the
/// re-encounter path as well -- about 1.5M calls per collection on a Spring
/// workload -- so it is behind the same flag that prints the breakdown rather
/// than paid for unconditionally. (`copy_tally_add` is NOT gated: it replaced
/// Phase H's statistics walk, and those counters are committed to
/// `self.stats` every cycle.) Latched once per process, as before.
pub fn copy_tally_arm_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| gc_flags().dbg_gcpause)
}

/// Bump one of the two `forward_object` arm counters: slot 4 counts
/// re-encounters (the object was already forwarded, so this call is a pure
/// lookup), slot 5 counts actual copies. Their ratio is what says whether
/// `cheney_drain` is a copying cost or a lookup cost.
///
/// UNGATED since gen r4w3/young2: the caller checks `CopyDiag::tally_arm`,
/// which [`CopyDiag::current`] reads once per cycle from
/// [`copy_tally_arm_enabled`].
#[inline]
pub fn copy_tally_arm(reencounter: bool) {
    COPY_TALLY.with(|t| {
        let mut a = t.get();
        a[if reencounter { 4 } else { 5 }] += 1;
        t.set(a);
    });
}

/// The pause-goal loop's memory between two collections. See
/// [`next_young_trigger`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TriggerFeedback {
    /// A halving under trial: the threshold it replaced and the pause that
    /// provoked it. Judged by the next collection's pause.
    pub trial: Option<(usize, u64)>,
    /// The survivor volume at which a halving was found not to help. While a
    /// later over-goal pause copies within a factor of two of it, the nursery
    /// is known not to be the lever and the trigger is left alone.
    pub latched_copied: Option<u64>,
    /// What the loop DID, counted by [`next_young_trigger`] (the decision
    /// arms) and [`GenerationalHeap::adapt_young_trigger_to_pause`] (the
    /// inputs). gen r4/alloc, 2026-09-23 — see [`YoungTriggerCensus`].
    pub census: YoungTriggerCensus,
    /// gci i2/yg (`CRATONVM_GC_YOUNG_PAUSE_TRIAL_JUDGED_IN_BAND`, opt-in,
    /// [`young_pause_trial_judged_in_band_enabled`]): an in-band pause
    /// (`goal / 4 <= pause <= goal`) JUDGES a pending halving trial instead
    /// of silently dropping it. Fixed for the heap's life at construction.
    pub judge_in_band: bool,
    /// gci i3/yg (`CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED`, opt-in,
    /// [`young_pause_skip_forced_enabled`]): a young cycle the VM's
    /// allocation-failure door ran
    /// ([`GenerationalHeap::mark_next_young_cycle_forced`]) is not fed to the
    /// loop. Fixed for the heap's life at construction.
    pub skip_forced: bool,
}

/// gci i3/yg: the reader of `CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED` (opt-in,
/// `runtime_flag_on`; read once per heap, at construction).
///
/// `gci-i2yg-young-trigger-loop-and-age-clock-review-findings` item 2: the
/// pause-goal loop adapts to every measured young pause, including the cycles
/// an ALLOCATION FAILURE forced (a fragmented young free list refusing a
/// large array, `gcg-g1c-...`). Such a cycle comes after however much the
/// free list could serve, whatever the trigger is, so halving the trigger
/// cannot shorten it, and the lowered trigger outlives the forced regime
/// (vi1-yg `yg1_cost_yp_ck_3`: `ladder_forced=403` of 433 minors, 12
/// halvings). HotSpot Serial has no such loop. With the switch a forced
/// cycle is counted (`adapt_calls`, `adapt_skipped_forced`) but neither
/// opens nor judges a trial and leaves the threshold where it is, exactly as
/// a pause that also reclaimed the old generation (`adapt_on_major`). Off:
/// the historical loop. Opt-in because it moves the trigger (pause shape).
pub fn young_pause_skip_forced_enabled() -> bool {
    false
}

/// gci i2/yg: the reader of `CRATONVM_GC_YOUNG_PAUSE_TRIAL_JUDGED_IN_BAND`
/// (opt-in, `runtime_flag_on`; read once per heap, at construction).
///
/// [`next_young_trigger`]'s in-band arm (`goal / 4 <= pause <= goal`) clears a
/// pending halving trial without judging it, so a halving that bought nothing
/// is never reverted when the very next pause lands a hair under the goal --
/// and the loop is then free to halve AGAIN on the pause after. Observed in
/// `vi1-yg` `yg1_cost_yp_ck_3` (`CRATONVM_DBG=gcpause`): 203 ms -> halve
/// 64 -> 32 MB; 200 ms (in band, trial dropped); 205 ms -> halve 32 -> 16 MB
/// (a fresh trial, not a judgement), where the documented rule ("a halving
/// that did not cut the pause by a quarter is REVERTED") asked for a revert
/// to 64 MB and a latch at the second pause. With the switch the in-band arm
/// applies that rule: a pause that did not fall by a quarter against the
/// provoking one reverts and latches; one that did keeps the halving and
/// closes the trial. Off: the historical arm, unchanged. Opt-in because it
/// moves the trigger (pause shape) on the default pause-goal path.
pub fn young_pause_trial_judged_in_band_enabled() -> bool {
    false
}

impl TriggerFeedback {
    /// gci i2/yg: the feedback state a heap starts with.
    pub fn for_new_heap() -> Self {
        Self {
            judge_in_band: young_pause_trial_judged_in_band_enabled(),
            skip_forced: young_pause_skip_forced_enabled(), // gci i3/yg
            ..Self::default()
        }
    }
}

/// The young-trigger instrument: where the trigger IS, where it was set from,
/// and what the pause-goal loop did to it over the run.
///
/// gen r4/alloc (2026-09-23), for proposal #4 of
/// `docs/feature-designs/concurrent-gc-maturation.md` ("sweep the young
/// trigger"). Before this, a sweep of `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` could
/// not tell its own arms apart from the log: the knob sets the INITIAL
/// threshold and the CEILING of the pause-goal loop, and the loop then moves
/// the live value (`young_gc_threshold`) — halving, reverting, latching,
/// giving room back — with no counter anywhere and a `[gcpause]` line only
/// under `CRATONVM_DBG=gcpause`. Two arms whose loops both converged on the
/// same floor would look like "the knob does nothing", and nothing on a
/// default log could say that is what happened.
///
/// Per heap (it lives in `young_trigger_feedback`), not a process global. Read
/// through [`GenerationalHeap::young_trigger_census`] /
/// [`GenerationalHeap::young_trigger_census_line`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct YoungTriggerCensus {
    /// Measured pauses fed to the loop (refused cycles are not fed — see
    /// `collect_garbage`).
    pub adapt_calls: u64,
    /// Multiplicative decreases, including a halving kept after a trial that
    /// helped.
    pub halvings: u64,
    /// Trials judged unhelpful and REVERTED to the threshold they replaced.
    pub reverts: u64,
    /// Over-goal pauses left alone because the survivor volume was latched.
    pub latched_holds: u64,
    /// Additive increases (`pause * 4 < goal`), counted even when the ceiling
    /// clamps them to no change.
    pub increases: u64,
    /// Measured pauses whose cycle ALSO ran an old-generation reclamation
    /// (`gc_quiescence::old_gen_reclaimed_last_cycle`). Counted in
    /// `adapt_calls` but NOT fed to the decision (gen r4w2/alloc2): no young
    /// trigger can shorten an old-gen sweep, so such a pause neither opens nor
    /// judges a trial and leaves the threshold unchanged — see
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub adapt_on_major: u64,
    /// gci i3/yg: measured pauses of cycles the VM's allocation-failure door
    /// forced, counted in `adapt_calls` but NOT fed to the decision
    /// (`CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED`; always 0 with the switch off).
    pub adapt_skipped_forced: u64,
    /// gci i5/yg: in-band pauses (`goal / 4 <= pause <= goal`) that found a
    /// halving trial pending -- the population
    /// `CRATONVM_GC_YOUNG_PAUSE_TRIAL_JUDGED_IN_BAND` acts on. Counted with
    /// the switch on AND off (off, each one is a trial dropped unjudged; on,
    /// one judged), so a row can tell "the switch was never exercised" from
    /// "it was and changed nothing".
    pub adapt_in_band_trials: u64,
    /// The lowest threshold the loop ever stored, in bytes (0 = never adapted).
    pub trigger_low_water: usize,
    /// The live young threshold, in bytes (filled by the accessor).
    pub trigger_bytes: usize,
    /// Young from-space capacity, in bytes (filled by the accessor).
    pub semi_capacity: usize,
    /// `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` as clamped (filled by the accessor).
    pub knob_percent: usize,
    /// `CRATONVM_GC_YOUNG_PAUSE_MS`, 0 = loop off (filled by the accessor).
    pub pause_goal_ms: u64,
    /// gcf f7/c: HotSpot Serial's eden the trigger follows under
    /// `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN`, 0 = off (filled by the
    /// accessor). When non-zero `knob_percent` is not what set the trigger.
    pub serial_eden_bytes: usize,
    /// gci i1/yg: the eden the HotSpot age clock counts in under
    /// `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`, 0 = off; and the collections that
    /// aged their survivors (`ticks`) or held them at their age (`holds`).
    /// Filled by the accessor.
    pub age_clock_eden_bytes: usize,
    pub age_clock_ticks: u64,
    pub age_clock_holds: u64,
    /// gci i3/yg: bytes allocated straight in the old generation that the age
    /// clock charged as eden allocation (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`
    /// with the clock armed; 0 otherwise). Filled by the accessor.
    pub age_clock_old_direct_bytes: u64,
}

impl YoungTriggerCensus {
    /// The `[GC] young_trigger:` shutdown line. Keys are prefixed so none of
    /// them collides with another `[GC]` summary line's (`alloc_trigger_bytes=`
    /// is ZGC's).
    pub fn line(&self) -> String {
        let pct_now = if self.semi_capacity == 0 {
            0.0
        } else {
            self.trigger_bytes as f64 * 100.0 / self.semi_capacity as f64
        };
        format!(
            "[GC] young_trigger: young_trigger_bytes={} young_semi_capacity={} \
             young_trigger_pct_now={pct_now:.1} young_trigger_knob_pct={} \
             young_pause_goal_ms={} adapt_calls={} adapt_halvings={} adapt_reverts={} \
             adapt_latched_holds={} adapt_increases={} adapt_on_major={} \
             adapt_skipped_forced={} \
             young_trigger_low_water={} young_trigger_serial_eden={} \
             young_age_clock_eden={} young_age_clock_ticks={} young_age_clock_holds={} \
             young_age_clock_old_direct={} adapt_in_band_trials={}",
            self.trigger_bytes,
            self.semi_capacity,
            self.knob_percent,
            self.pause_goal_ms,
            self.adapt_calls,
            self.halvings,
            self.reverts,
            self.latched_holds,
            self.increases,
            self.adapt_on_major,
            self.adapt_skipped_forced,
            self.trigger_low_water,
            self.serial_eden_bytes,
            self.age_clock_eden_bytes,
            self.age_clock_ticks,
            self.age_clock_holds,
            self.age_clock_old_direct_bytes,
            self.adapt_in_band_trials,
        )
    }
}

/// The pause-goal decision, pure so it can be tested against the sequence of
/// pauses that exposed it.
///
/// * `pause > goal`, no trial pending, no latch: halve and open a trial.
/// * `pause > goal` with a trial pending: the halving is judged. If the pause
///   fell by at least a quarter it helped — halve again and keep trialling.
///   If not, the survivors were the pause: RESTORE the threshold the trial
///   replaced and latch on this survivor volume.
/// * `pause > goal` while latched and the survivor volume is within 2x of the
///   latched one: leave the trigger alone. A volume that has moved by more
///   than that re-arms the loop.
/// * `pause * 4 < goal`: additive increase, as before.
/// * in band (`goal / 4 <= pause <= goal`): no change; a pending trial is
///   dropped unjudged, or (gci i2/yg, `fb.judge_in_band`) judged as above.
#[allow(clippy::too_many_arguments)]
pub fn next_young_trigger(
    fb: &mut TriggerFeedback,
    current: usize,
    floor: usize,
    ceiling: usize,
    capacity: usize,
    pause_ms: u64,
    goal_ms: u64,
    bytes_copied: u64,
) -> usize {
    let within_2x = |a: u64, b: u64| a / 2 <= b && b <= a.saturating_mul(2);
    // `ceiling.max(floor)`: `Ord::clamp` PANICS when min > max, and this is a
    // pure function whose callers are trusted, not checked. The one caller
    // today passes `ceiling.max(floor)` already; this makes the function total
    // on its own terms rather than by that caller's courtesy (gen r4/alloc).
    let ceiling = ceiling.max(floor);
    if pause_ms > goal_ms {
        if let Some((restore_to, provoking_pause)) = fb.trial.take() {
            if pause_ms.saturating_mul(4) >= provoking_pause.saturating_mul(3) {
                // The halving did not buy a quarter of the pause: the live
                // set is what this collection costs, and a smaller nursery
                // only adds collections.
                fb.latched_copied = Some(bytes_copied);
                fb.census.reverts += 1;
                return restore_to.clamp(floor, ceiling);
            }
            fb.trial = Some((current, pause_ms));
            fb.census.halvings += 1;
            return (current / 2).max(floor);
        }
        if let Some(latched) = fb.latched_copied {
            if within_2x(latched, bytes_copied) {
                fb.census.latched_holds += 1;
                return current;
            }
            fb.latched_copied = None;
        }
        // Overshot: copy less next time. Multiplicative decrease, because the
        // overshoot can be large (872 ms against a 500 ms goal) and a linear
        // back-off would take many over-budget cycles to converge — each of
        // which is a missed deadline.
        fb.trial = Some((current, pause_ms));
        fb.census.halvings += 1;
        (current / 2).max(floor)
    } else if pause_ms.saturating_mul(4) < goal_ms {
        // Comfortably inside: give the trigger room back, additively, so a
        // workload that transiently spiked does not stay permanently
        // throttled. Slower than the decrease on purpose.
        fb.trial = None;
        fb.census.increases += 1;
        current.saturating_add(capacity / 32).min(ceiling)
    } else {
        // In band. gci i2/yg: under `judge_in_band` a pending trial is judged
        // here by the same rule as the over-goal arm (a pause that did not
        // fall by a quarter reverts and latches); off, it is dropped unjudged.
        if let Some((restore_to, provoking_pause)) = fb.trial.take() {
            // gci i5/yg: the switch's population, counted on both arms.
            fb.census.adapt_in_band_trials += 1;
            if fb.judge_in_band && pause_ms.saturating_mul(4) >= provoking_pause.saturating_mul(3) {
                fb.latched_copied = Some(bytes_copied);
                fb.census.reverts += 1;
                return restore_to.clamp(floor, ceiling);
            }
        }
        current
    }
}

/// gen r4w2/alloc2 (2026-09-23): a young slow-path allocation whose rounded
/// footprint is at least this many bytes has its BODY zeroed after the
/// `young_from` guard is dropped (only the header is zeroed and written under
/// the lock). Below it the memset is short enough that holding the lock costs
/// less than a second pass would, and the code is byte-for-byte what it was.
/// 64 KiB is twice `tlab::TLAB_MAX_ALLOC`, so ordinary objects and every
/// TLAB-sized request stay on the old shape. Not a tuning knob: nothing about
/// the answer changes, only how long the lock is held. See
/// `docs/feature-designs/generative-gc-architecture.md`.
pub const YOUNG_ZERO_OUTSIDE_LOCK_MIN: usize = 64 * 1024;

/// Does `try_alloc_array_humongous` zero this array's BODY after the
/// old-generation lock is released? gen r4w4/alloc4 (2026-09-24), the fix for
/// `docs/feature-designs/generative-gc-architecture.md`
/// (the reference-array residual is
/// `docs/feature-designs/generative-gc-architecture.md`).
///
/// Yes iff the switch is on (`CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED`, default
/// ON), the elements are PRIMITIVE, and the footprint reaches
/// [`YOUNG_ZERO_OUTSIDE_LOCK_MIN`] (below it a second pass costs more than the
/// lock hold it saves — the young slow path's threshold, for the same reason).
///
/// # The protocol
///
/// 1. **Under the lock:** `OldGen::alloc_unzeroed` reserves the block (the
///    free-list pop, the commit, the block-offset-table start and the
///    high-water mark are all taken exactly as `OldGen::alloc` takes them);
///    the header span is zeroed and the complete header written. From this
///    instant the block is one well-formed array whose header covers the
///    whole reserved extent, which is `alloc_unzeroed`'s obligation.
/// 2. **Unlock**, which publishes the header to the next lock holder.
/// 3. **After the lock:** the body `[ARRAY_DATA_OFFSET, reserved)` is zeroed,
///    then a release fence, then the reference is returned to the caller —
///    the first moment any other thread can learn the array's address from
///    this thread.
///
/// # Why no concurrent walker can see a half-initialised block
///
/// Between 2 and 3 the block is reachable only by address arithmetic, never by
/// a reference, and every reader that works that way reads HEADERS only:
///
/// * **The concurrent sweep slice** (`concurrent_mark.rs`'s sweep slices over
///   `OldGen::walk_objects_from`, under the old-gen lock) reads the header to
///   stride, and frees only an unmarked object in its ELIGIBILITY snapshot
///   (objects that existed at initial mark and remark) — a block allocated
///   after that is never eligible. The header is complete at step 1; the body
///   is never read.
/// * **The concurrent mark's overflow rescan** (`walk_objects_from` again)
///   calls `scan_object` on MARKED objects only, and `scan_object` reads the
///   body of a reference array or object — never of a primitive array, which
///   has no reference slots. That is why a REFERENCE array keeps the locked
///   memset: a marked-looking stale reference body is exactly the half-state
///   this protocol must not expose, and whether a new block can read as marked
///   during a cycle is `concurrent_mark.rs`'s allocate-black policy, not an
///   allocation-lane decision. (gen r5w1/oldgen5: the default-on
///   `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` zeroes a reference array
///   outside the lock too, by making it a primitive array until its body is
///   zero — [`humongous_ref_disguise`].)
/// * **The block-offset table and card scans** find object STARTS; a young
///   collection's card scan runs inside a stop-the-world pause, which cannot
///   begin while this thread is between steps 1 and 3 (it is in VM code, not
///   at a safepoint poll — the premise the young slow path's
///   outside-the-lock zeroing and `refill_tlab`'s memset already rest on).
/// * **Heap walkers** (heap dumps, JVMTI iteration, `-Xlog` census walks) run
///   at a safepoint for the same reason, and see the finished array.
/// * **Conservative candidates** are resolved only inside pauses.
///
/// A TRACING reader can reach the array only through a reference, which exists
/// only after step 3; the release fence orders the body's zeroes before any
/// store of that reference, so a marker that loads it (and synchronises on
/// that load, as the SATB / mark-queue handoff does) reads a zeroed body.
#[inline]
pub fn humongous_body_zeroes_outside_the_lock(
    switch_on: bool,
    element_type: ArrayElementType,
    total_size: usize,
) -> bool {
    switch_on
        && !matches!(element_type, ArrayElementType::Reference)
        && total_size >= YOUNG_ZERO_OUTSIDE_LOCK_MIN
}

/// gen r5w1/oldgen5 (2026-09-26) — does `try_alloc_array_humongous` zero a
/// humongous REFERENCE array's body after the old-generation lock is released?
/// Yes iff `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` (default ON since gcg g5) is on and the
/// footprint reaches [`YOUNG_ZERO_OUTSIDE_LOCK_MIN`]. The residual of the
/// primitive-array fix:
/// `docs/feature-designs/generative-gc-architecture.md`.
#[inline]
pub fn humongous_ref_body_zeroes_outside_the_lock(
    switch_on: bool,
    element_type: ArrayElementType,
    total_size: usize,
) -> bool {
    switch_on
        && matches!(element_type, ArrayElementType::Reference)
        && total_size >= YOUNG_ZERO_OUTSIDE_LOCK_MIN
}

/// gen r5w1/oldgen5 (2026-09-26) — the header a humongous REFERENCE array
/// wears while its body is zeroed outside the old-generation lock: a `long[]`
/// whose footprint is exactly the reference array's (`data_size / 8`
/// elements; `array_data_size` rounds every array body to 8 bytes, so the two
/// agree to the byte). `None` when that length does not fit a `u32`.
///
/// # The protocol (option 2 of the page, without its gap)
///
/// 1. **Under the lock:** the block is reserved unzeroed, its header span
///    zeroed, and the DISGUISE written: one well-formed PRIMITIVE array
///    covering the whole reserved extent, flagged `GC_FLAG_OLD_GEN`.
/// 2. **Unlock; zero the body; release fence.**
/// 3. **Re-lock; write the real reference-array header; unlock;** then the
///    reference is returned — the first moment anyone can learn the address
///    from this thread.
///
/// The page's worry about "a header that says not yet scannable" was a header
/// with length 0: a walker would stride over it into the unzeroed body and
/// desync. The disguise has the REAL footprint, so every walker (the block
/// offset table, the sweeps, `walk_objects*`) strides exactly as it will over
/// the finished array. The reader that made reference arrays keep the locked
/// memset — the concurrent mark's overflow rescan, which scans the body of a
/// MARKED object — reads no body of a primitive array, so a stale body is
/// never handed to it as pointers during step 2; it holds the old-gen lock, so
/// it sees the real header only after step 3 and, through that lock, the zeroes
/// of step 2. Every other reader is the primitive arm's (see
/// [`humongous_body_zeroes_outside_the_lock`]): stop-the-world readers cannot
/// run while this thread is between steps 1 and 3, and a tracing reader needs
/// the reference, which does not exist yet. The cost of the second lock is two
/// uncontended atomic operations and a 16-byte store.
pub fn humongous_ref_disguise(class_id: ClassId, data_size: usize) -> Option<ObjectHeader> {
    if data_size % 8 != 0 {
        return None;
    }
    let len = u32::try_from(data_size / 8).ok()?;
    let mut disguise = ObjectHeader::new(
        class_id,
        ObjectKind::Array,
        ArrayElementType::Long,
        len,
        len,
    );
    disguise.add_gc_flags(GC_FLAG_OLD_GEN);
    Some(disguise)
}

/// Zero `[start, start + len)`, except the part of it inside `pristine` (an
/// absolute `[lo, hi)` range the arena proved has never been written, from
/// `Arena::take_last_hand_out_pristine`), which already reads as zero.
/// `None` zeroes everything — the behaviour whenever the heap did not opt in.
///
/// # Safety
///
/// `[start, start + len)` must be writable memory exclusively owned by the
/// caller, and `pristine`, if given, must describe never-written arena bytes.
pub unsafe fn zero_span_skipping_pristine(
    start: *mut u8,
    len: usize,
    pristine: Option<(usize, usize)>,
) {
    let a = start as usize;
    let b = a.saturating_add(len);
    if let Some((lo, hi)) = pristine {
        let lo = lo.clamp(a, b);
        let hi = hi.clamp(lo, b);
        if lo < hi {
            // SAFETY: `a <= lo <= hi <= b`, so both pieces lie inside the
            // caller's span (the function contract); `[lo, hi)` is skipped
            // because it already reads as zero.
            unsafe {
                std::ptr::write_bytes(start, 0, lo - a);
                std::ptr::write_bytes(start.add(hi - a), 0, b - hi);
            }
            return;
        }
    }
    // SAFETY: the function contract — the whole span is the caller's.
    unsafe { std::ptr::write_bytes(start, 0, len) };
}

/// Was the young hand-out `[off, off + len)` carved from the arena's BUMP TAIL?
/// `cursor_before` / `cursor_after` are the arena's low bump cursor
/// (`Arena::used_low_for_compaction`) immediately before and after the
/// `Arena::alloc` that returned it, read under the same `young_from` guard.
///
/// gen r4w4/alloc4 (2026-09-24), the predicate the zero-once skip
/// (`GcFlags::gen_zero_once`) rests on. It is exact for every arm of
/// `Arena::alloc`: the two bump arms (ordinary and reserve) hand out bytes that
/// lay AT OR ABOVE the old cursor and move the cursor past them; every
/// free-list arm hands out a block BELOW the old cursor (the low free list
/// only ever holds space the cursor has passed) and leaves the cursor where it
/// was; the high-end last-resort arm hands out above `high_cursor` and also
/// leaves the low cursor alone, so `off + len <= cursor_after` fails for it.
#[inline]
pub fn hand_out_is_bump_tail(
    cursor_before: usize,
    off: usize,
    len: usize,
    cursor_after: usize,
) -> bool {
    off >= cursor_before && off.checked_add(len).is_some_and(|end| end <= cursor_after)
}

/// Debug builds only: sample one word per 4 KiB page (and the last word) of a
/// span the zero-once skip is about to leave un-memset, and say whether every
/// sampled word is zero. Release builds answer `true` without reading — the
/// O(1) first-word tripwire in [`GenerationalHeap::refill_chunk_tripwire`] and
/// the `CRATONVM_DBG_DEADREF_STORE` whole-span scan are the release checks.
///
/// # Safety
///
/// `[ptr, ptr + len)` must be mapped, readable memory the caller exclusively
/// owns, 8-aligned at `ptr`.
#[inline]
pub unsafe fn known_zero_sample_is_clean(ptr: *const u8, len: usize) -> bool {
    if !cfg!(debug_assertions) || len < 8 {
        return true;
    }
    let mut off = 0usize;
    while off + 8 <= len {
        // SAFETY: `off + 8 <= len`, so the word lies inside the caller's span;
        // `ptr` is 8-aligned and `off` is a multiple of 4096.
        if unsafe { std::ptr::read(ptr.add(off) as *const u64) } != 0 {
            return false;
        }
        off += 4096;
    }
    // SAFETY: `len >= 8`, so the last whole word of the span is inside it.
    unsafe { std::ptr::read_unaligned(ptr.add(len - 8) as *const u64) == 0 }
}

/// gcj j3/x2 R5: the least growth headroom a concurrent cycle's mark span
/// covers past the capacity (see [`GenerationalHeap::old_gen_mark_span`]).
pub const CONC_MARK_SPAN_MIN_HEADROOM: usize = 64 * 1024 * 1024;

/// gcj j3/x2 R5: `capacity + max(capacity / 4, CONC_MARK_SPAN_MIN_HEADROOM)`,
/// clamped to `[capacity, growth_max]`.
pub fn conc_mark_span_for(capacity: usize, growth_max: usize) -> usize {
    let headroom = (capacity / 4).max(CONC_MARK_SPAN_MIN_HEADROOM);
    capacity
        .saturating_add(headroom)
        .min(growth_max)
        .max(capacity)
}

/// gcj j3/x2 R4: the first word of every 4 KiB page a sunk TLAB tail
/// `[ptr, ptr + len)` touches, and its last word, all read zero. In release
/// builds too: [`GenerationalHeap::return_tlab_tail`] lets a FREE-LISTED tail
/// join the known-zero set only on a clean sample (the retraction arm only
/// clips the set, so it never asks). Not a proof -- a stray word between two
/// sampled ones is not seen -- but the next stray store of a cursor overrun
/// lands on a page start whenever it crosses one, and the whole-tail check is
/// still `CRATONVM_DBG_DEADREF_STORE`'s (`Tlab::retire`).
///
/// # Safety
///
/// `[ptr, ptr + len)` must be mapped, readable memory nobody writes
/// concurrently, 8-aligned at `ptr`.
#[inline]
pub unsafe fn sunk_tail_sample_reads_zero(ptr: *const u8, len: usize) -> bool {
    if len < 8 {
        return true;
    }
    let addr = ptr as usize;
    // SAFETY: the caller's span; every read below is a whole word inside it.
    let word = |off: usize| unsafe { std::ptr::read(ptr.add(off) as *const u64) };
    if word(0) != 0 {
        return false;
    }
    let mut page = (addr + 4096) & !4095;
    while page + 8 <= addr + len {
        if word(page - addr) != 0 {
            return false;
        }
        page += 4096;
    }
    word((len - 8) & !7) == 0
}

/// `CRATONVM_GC_SYNC_YOUNG_WIPE` — zero the evacuated semi-space inside the
/// pause, as every cycle did before 2026-09-02.
pub fn sync_young_wipe_enabled() -> bool {
    gc_flags().gc_sync_young_wipe
}

/// `CRATONVM_GC_JIT_REF_STORE_GATES` — publish the compiled-reference-store
/// barrier plan from the generational collector. Default **ON**; `=0` withholds
/// the plan, so every compiled reference store takes the full
/// `jit_putfield_object` path exactly as it did before 2026-09-02.
///
/// The A/B lever for this change, and the first thing to set if a compiled
/// store is ever suspected of missing a card or an SATB entry.
pub fn jit_ref_store_gates_enabled() -> bool {
    true
}

/// Fold one parallel-evacuation shard's tally into the driver's.
///
/// `shard` is `COPY_TALLY`'s own layout — `copy_tally_add`'s four counters
/// followed by `copy_tally_arm`'s re-encounter/copy pair — accumulated on the
/// worker that did the copying. Called on the DRIVER thread only, after the
/// completion barrier, so the tally stays a thread-local of the one thread
/// that reads it.
pub fn copy_tally_merge(shard: &[u64; 6]) {
    COPY_TALLY.with(|t| {
        let mut a = t.get();
        for i in 0..6 {
            a[i] += shard[i];
        }
        t.set(a);
    });
}

/// Read and clear the tally for this cycle.
pub fn copy_tally_take() -> [u64; 6] {
    COPY_TALLY.with(|t| {
        let a = t.get();
        t.set([0; 6]);
        a
    })
}

/// `CRATONVM_DBG_FWDWALK=1` — re-enable the post-copy `pointer_map` walk that
/// used to produce the counters above. It is now only a BUG-Z diagnostic
/// (every forward must land inside young to-space or old gen), so it is off by
/// default rather than paid for on every collection.
pub fn fwd_walk_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    // `runtime_var`, not `std::env::var`: a raw getenv bypasses the latched
    // `VmFlags` snapshot, so `CRATONVM_DBG=fwdwalk` could not reach it and a
    // test could not arrange it with `flags::with_thread_overrides`.
    *G.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok_and(|v| v != "0")    })}

/// Young-semispace geometry, published once per collection so code with no heap
/// handle can ask [`dead_young_ref_reason_global`].
///
/// `[active_lo, active_hi, inactive_lo, inactive_hi]`. All zero until the first
/// publish, which makes the predicate answer `None` — "not known to be dead" —
/// rather than guessing.
pub static YOUNG_GEO: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// The handle-free form of [`GenerationalHeap::dead_young_ref_reason`].
///
/// `Frame::set_local` is the one place a stale reference becomes a Java-visible
/// value, and it holds no heap. The existing guard there reads the vacated
/// ledger, which forgets an address the instant the allocator re-issues it —
/// exactly the window in which this workload's stale references are installed,
/// so it reports nothing. This predicate is derived from the semispace geometry
/// and the bytes at the address, and has no such blind spot.
pub fn dead_young_ref_reason_global(addr: usize) -> Option<&'static str> {
    if addr == 0 || addr % 8 != 0 {
        return None;
    }
    let g = |i: usize| YOUNG_GEO[i].load(Ordering::Relaxed);
    let (alo, ahi, ilo, ihi) = (g(0), g(1), g(2), g(3));
    if ahi == 0 && ihi == 0 {
        return None;
    }
    if addr >= ilo && addr < ihi {
        return Some("INACTIVE-SEMISPACE");
    }
    if addr >= alo && addr < ahi {
        // gen r4w5/review5 (2026-09-24): "inside the active semispace" stopped
        // meaning "mapped" once the young arenas started committing lazily and
        // `uncommit_evacuated_young` (default ON) began handing the evacuated
        // semi-space back to the OS: after the next flip that space IS the
        // active one, committed only as far as the evacuation and the mutators'
        // bumps have reached. A stale reference into the rest -- exactly what
        // `CRATONVM_DBG_DEADREF_STORE` exists to catch -- faulted here on the
        // header read. Nothing on an uncommitted page, and no header straddling
        // the end of the semispace, can be an object, so both are "dead"
        // without a read. The same screen the band verifier's object test uses
        // (`published_young_range_committed`).
        if addr.checked_add(HEADER_SIZE).is_none_or(|end| end > ahi) {
            return Some("HEADER-PAST-ACTIVE-SEMISPACE-END");
        }
        if !published_young_range_committed(addr, HEADER_SIZE) {
            return Some("UNCOMMITTED-YOUNG");
        }
        // SAFETY: 8-aligned, inside the active semispace with its whole header
        // span below the semispace's end, and that span is committed (both
        // checked just above).
        // gcd d1/e: through `header_is_zero`, which tests the mark before the
        // shape accessors (they follow a forwarded-looking mark).
        if unsafe { header_is_zero(addr) } {
            return Some("ZERO-HEADER-IN-YOUNG");
        }
    }
    None
}

/// Which young semispace, if either, contains `addr` — asked WITHOUT reading a
/// single byte of the heap.
///
/// This is the half of [`dead_young_ref_reason_global`] that survives
/// `CRATONVM_GEN_UNCOMMIT`. That function's `ZERO-HEADER-IN-YOUNG` arm
/// dereferences the header, so it cannot be asked about an address in a granule
/// the give-back has released — and a released granule is precisely the state
/// this generational defect's victims are found in
/// (`GenerationalHeap::uncommit_evacuated_young`, which the crash handler names
/// `site=unbumped-middle`). Range compares against the published geometry touch
/// nothing, so this answers whether or not the page is mapped.
///
/// (gen r4w5/review5, 2026-09-24: that arm now screens the header span through
/// the published young commit map first and answers `UNCOMMITTED-YOUNG` for a
/// released granule instead of faulting on it. It still reads the header of a
/// COMMITTED young address, which this function never does.)
///
/// `None` before the first `publish_young_geometry`, which a caller must read as
/// "no answer", not as "not young".
///
/// On its own this says only "a young address". It becomes a verdict when the
/// caller has ALREADY established that no object starts at `addr` — see
/// `is_object_address`, which screens through the commit bitmap and the arena's
/// object-start bitmap and is likewise dereference-free for a released granule.
/// A live young reference always names an object start, so "in young and not an
/// object start" names no live object.
pub fn young_geometry_span(addr: usize) -> Option<&'static str> {
    let g = |i: usize| YOUNG_GEO[i].load(Ordering::Relaxed);
    young_geometry_span_in((g(0), g(1), g(2), g(3)), addr)
}

/// [`young_geometry_span`] against an explicit geometry rather than the
/// published one. Split out so the classification is testable without writing
/// a process-global that every collection in the suite republishes.
pub fn young_geometry_span_in(geo: (usize, usize, usize, usize), addr: usize) -> Option<&'static str> {
    if addr == 0 || addr % 8 != 0 {
        return None;
    }
    let (alo, ahi, ilo, ihi) = geo;
    if ahi == 0 && ihi == 0 {
        return None;
    }
    if addr >= ilo && addr < ihi {
        return Some("INACTIVE-SEMISPACE");
    }
    if addr >= alo && addr < ahi {
        return Some("ACTIVE-SEMISPACE");
    }
    None
}

/// Cap on [`ForwardRefusalLedger`] refusals per cycle.
pub const REFUSAL_LEDGER_CAP: usize = 64;

/// The young evacuator's refusal ledger: addresses it REFUSED to forward on
/// the heap's most recent moving cycle, with the reason, plus the
/// object-start walk summary the `[forward-refused]` audit prints beside
/// them. See [`GenerationalHeap::forward_refusal_reason`].
///
/// Always on, unlike the `[forward-refused]` printout: the one reader is a
/// post-GC verifier that fires at most a handful of times per run, and the
/// question it answers has no other source. Bounded — a cycle that refuses more
/// than this has a different problem, and the census counter says so.
///
/// PER HEAP since gcd d3/m (2026-09-27): it was two process statics
/// (`LAST_CYCLE_REFUSALS`, `LAST_OBJSTART_WALK`), so with two Generational
/// VMs in one process B's moving cycle cleared A's refusals between A's
/// collection and A's post-GC verifier, which then printed
/// `evacuator_verdict=NEVER-OFFERED` for an address A's evacuator did refuse
/// (`gcd-d2h-forward-refusal-ledger-is-process-global-FIXED-20260928`).
/// The serial evacuator reaches it through [`PromotionQueue::refusal_ledger`]
/// (an `Arc` clone per moving cycle), the parallel driver through the heap.
#[derive(Default)]
pub struct ForwardRefusalLedger {
    pub refusals: parking_lot::Mutex<Vec<(usize, &'static str)>>,
    /// See `GenerationalHeap::note_objstart_walk`.
    pub objstart_walk: parking_lot::Mutex<Option<String>>,
}

impl ForwardRefusalLedger {
    /// Drop the previous cycle's refusals. Called once at the top of each
    /// moving young collection so a reader always sees THIS cycle's answer.
    pub fn reset(&self) {
        self.refusals.lock().clear();
    }

    /// Record one refusal. Cheap and unconditional: `forward_object_impl`
    /// reaches it only on a path that already declined to copy.
    pub fn record(&self, addr: usize, reason: &'static str) {
        let mut v = self.refusals.lock();
        if v.len() < REFUSAL_LEDGER_CAP && !v.iter().any(|(a, _)| *a == addr) {
            v.push((addr, reason));
        }
    }

    /// See [`GenerationalHeap::forward_refusal_reason`].
    pub fn reason(&self, addr: usize) -> Option<&'static str> {
        self.refusals
            .lock()
            .iter()
            .find(|(a, _)| *a == addr)
            .map(|(_, r)| *r)
    }
}

/// Are the identifying header words at `addr` all zero?
///
/// True of every span of young memory that is NOT an object: the tail past the
/// allocation frontier, a swept free block, and the data area of a retired
/// TLAB's tail filler. An allocation writes the header before it publishes the
/// reference, so a live young object never reads this way.
///
/// # Safety
/// `addr` must be 8-aligned and `[addr, addr + HEADER_SIZE)` must be COMMITTED
/// young memory. Containment in a semispace is not enough: the young arenas
/// commit lazily and hand the evacuated semi-space back to the OS
/// (`uncommit_evacuated_young`). The caller screens with the arena's commit map
/// (gen r4w5/review5; it used to screen with `is_in_young_either` alone).
pub unsafe fn header_is_zero(addr: usize) -> bool {
    let h = unsafe { &*(addr as *const ObjectHeader) };
    // gcd d1/e (2026-09-27): the mark FIRST. `num_slots()` / `array_length()`
    // resolve a FORWARDED mark through its target, and the bytes screened here
    // are, by the function's purpose, not known to be an object: a class-id-0
    // word followed by a forwarded-looking mark sent the old order's
    // `num_slots()` to whatever the second word named (the pin9 fault shape,
    // `docs/feature-designs/generative-gc-architecture.md`).
    // With the mark proven zero the two accessors read this header's own
    // shape word. Same conjunction, so the same answer wherever the old
    // order did not fault.
    h.mark_word.load(Ordering::Relaxed) == 0
        && h.class_id.as_u32() == 0
        && h.num_slots() == 0
        && h.array_length() == 0
}

/// `CRATONVM_DBG_OBJ_WATCH=<class-name-substring>` — the evacuation history of
/// one class's instances.
///
/// # Why an address watch is not enough
///
/// `CRATONVM_DBG_WATCH_CELL` watches ONE address. A young object does not stay
/// at one address: under GC stress it is copied every cycle, and a semispace is
/// re-served from the same base each time, so an address names a different
/// object on every pass. That makes the cell watch excellent at answering "who
/// wrote HERE" and useless at answering "what happened to THIS OBJECT" — which
/// is the question a field that reads null in old gen actually raises, because
/// the value was lost somewhere along a chain of copies whose addresses nobody
/// recorded.
///
/// This watch is keyed by CLASS instead, so it follows the object: one line per
/// evacuation, with the body words as they stand AT THE SOURCE. Chaining the
/// lines by `dst` → next line's `src` reconstructs the object's whole path, and
/// the first line whose word is zero dates the loss to a single cycle.
pub fn obj_watch_pattern() -> Option<&'static String> {
    static P: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
    .as_ref()
}

/// Sequence number for [`obj_watch_pattern`]'s lines — the copies of one object
/// have to be ordered, and the collector has no cycle counter reachable from
/// the static evacuator.
pub static OBJ_WATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// How many `[deadref-recv]` reports `GenerationalHeap::note_deadref_recv` has
/// produced. See [`deadref_recv_hits`].
pub static DEADREF_RECV_HITS: AtomicU64 = AtomicU64::new(0);

/// Stores whose RECEIVER named no live object, over the life of the process.
///
/// Only counts while `CRATONVM_DBG_DEADREF_STORE` is set — the screen is behind
/// that gate — so a zero means "armed and clean" only when the flag was on.
pub fn deadref_recv_hits() -> u64 {
    DEADREF_RECV_HITS.load(Ordering::Relaxed)
}

/// One `[OBJWATCH]` line for a copy of a watched class's instance. No-op (one
/// cached load) unless `CRATONVM_DBG_OBJ_WATCH` is set.
pub fn note_obj_watch_copy(
    header: &ObjectHeader,
    old_ptr: *mut u8,
    new_ptr: *mut u8,
    total_size: usize,
    promoted: bool,
) {
    let Some(pattern) = obj_watch_pattern() else {
        return;
    };
    let cid = header.class_id.as_u32();
    let Some((name, _)) = cratonvm_gc_core::gc::resolve_class_info(cid) else {
        return;
    };
    if !name.contains(pattern.as_str()) {
        return;
    }
    let seq = OBJ_WATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let body = total_size.saturating_sub(HEADER_SIZE);
    let mut words = String::new();
    for i in 0..(body / 8).min(16) {
        // SAFETY: `old_ptr` is the live source object, whose extent is
        // `total_size`; `HEADER_SIZE + i * 8 + 8 <= total_size` by the bound.
        let w = unsafe {
            std::ptr::read_unaligned((old_ptr as usize + HEADER_SIZE + i * 8) as *const u64)
        };
        words.push_str(&format!(" [{i}]=0x{w:x}"));
    }
    eprintln!(
        "[OBJWATCH] seq={seq} class={name} cid={cid} src=0x{:x} dst=0x{:x} size={total_size} \
         promoted={promoted} gc_age={} flags=0x{:x} srcbody:{words}",
        old_ptr as usize,
        new_ptr as usize,
        header.gc_age(),
        header.gc_flags(),
    );
}

/// The store half of [`obj_watch_pattern`]: every `set_field` whose RECEIVER is
/// an instance of the watched class, with the Rust caller.
///
/// The copy half answers "when did this field change"; this one answers "who
/// changed it", and the two questions have different suspects. A field that
/// reads null is not necessarily a field nobody wrote — it is just as often a
/// field somebody wrote NULL to, through a receiver they should not still have
/// been holding, and the interpreter's `[PUTFIELD-WATCH]` cannot see that
/// because the writer is not bytecode.
pub fn note_obj_watch_store(header: &ObjectHeader, obj_ref: ObjectRef, index: usize, value: Value) {
    let Some(pattern) = obj_watch_pattern() else {
        return;
    };
    let cid = header.class_id.as_u32();
    let Some((name, _)) = cratonvm_gc_core::gc::resolve_class_info(cid) else {
        return;
    };
    if !name.contains(pattern.as_str()) {
        return;
    }
    eprintln!(
        "[OBJWATCH] store obj=0x{:x} class={name} cid={cid} index={index} value={value:?}\n{}",
        obj_ref.as_ptr() as usize,
        std::backtrace::Backtrace::force_capture(),
    );
}

/// gcd d2/h: refuse a moving young cycle for the unrecorded precise root
/// `addr` unless the previous refusal was for this very address (`prev`, `0`
/// = none yet). The whole bound of
/// `GenerationalHeap::unrecorded_young_root_refuses_cycle`, as a pure function.
pub fn unrecorded_root_refusal_decision(prev: usize, addr: usize) -> bool {
    prev != addr
}

/// gch h2/yg: the refusal-streak step of
/// `GenerationalHeap::unrecorded_young_root_refuses_cycle`, as a pure function.
/// `(now, max)` is the current run of consecutive refused decisions and the
/// longest run so far; a refusal extends the run, anything else ends it.
pub fn unrecorded_root_refusal_streak(now: u64, max: u64, refused: bool) -> (u64, u64) {
    let now = if refused { now.saturating_add(1) } else { 0 };
    (now, max.max(now))
}



/// gci i8/pe (2026-10-13), `gengc-r5w6-sizer10-refused-young-root-dangles` (b):
/// the ROOTS a conservative word becomes, added to the guard's exclusion set
/// beside the word itself.
///
/// The take-over pass and the helper-window pass do not root a frozen word `w`
/// as `w`. On Generational, `xt_root_scan::takeover_word_probe` answers an
/// aligned in-young word with the word itself (`VmHeap::resolve_interior_for_pin`
/// is `is_heap_addr`: alignment + containment) and a misaligned one with
/// `w & !7`; and whenever the answer IS that aligned word, `word_companion_below`
/// also roots `(w & !7) - 8` -- the object that ENDS at `w`, a one-past-the-end
/// cursor. Both land in `roots` (`gc_and_alloc::stw_take_over_and_wait`,
/// `xt_roots.extend(companions)`). The ledger holds the band raw, so neither
/// derived address is in it: on every take-over-arm pinned cycle each frozen
/// young word with an object below it handed the guard an "unrecorded precise
/// root" it is not (vi4-pn `pn4_jt1_pyt_1/2`: `urr_cycles_refused` 4 / 2,
/// `urr_failed_open` 20 / 20, every PY row 0).
///
/// Sound to exclude: each address added lies at most 8 bytes below a word the
/// pinned plan pins with `PIN_WORD_SLACK` (64 bytes below) or that option B's
/// plan takes (`plan_takes_blocked_peer_words`, same slack), so whatever it
/// names stays where it is this cycle; excluding it restores, for that address
/// only, the fail-open exit the guard had before it ran on such cycles. Words
/// outside `[lo, hi]` are not added (the guard tests only from-space).
pub fn extend_with_conservative_probe_echoes(words: &mut Vec<usize>, lo: usize, hi: usize) {
    let n = words.len();
    for i in 0..n {
        let w = words[i];
        let aligned = w & !7;
        if aligned != w && aligned >= lo && aligned <= hi {
            words.push(aligned);
        }
        if let Some(below) = aligned.checked_sub(8) {
            if below >= lo && below <= hi {
                words.push(below);
            }
        }
    }
}

