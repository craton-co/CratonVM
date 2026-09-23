// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Card table for the generational garbage collector.
//!
//! The card table tracks which regions of the old generation contain
//! references to young-generation objects. Each "card" covers a fixed-size
//! region (512 bytes). When a reference store from old gen to young gen is
//! detected by the write barrier, the corresponding card is marked dirty.
//!
//! During a minor GC, only dirty cards need to be scanned for old→young
//! references, avoiding a full scan of the old generation.
//!
//! T5.5.2 wiring (HIGH-1 fix): all `&self` methods (the mutator fast
//! path) are fully concurrent — they only touch the per-thread buffer
//! and the `Mutex<Vec<usize>>` of pending offsets. The authoritative
//! `cards` / `dirty_cards` state is protected by a separate
//! `Mutex<CardCells>` inside the table so the collector can safely
//! mark/clear/drain while mutators continue to enqueue dirty offsets.
//!
//! SECURITY FIX (V6) — cross-thread buffer drain invariant: every
//! thread's dirty buffer is registered in the global `BUFFER_REGISTRY`
//! on first use. At a stop-the-world safepoint the collector calls
//! [`CardTable::flush_all`], which walks the registry and folds *every*
//! thread's buffered old→young edges into `pending_offsets` before
//! `drain_pending`. This removes the previous cross-crate obligation
//! (that each mutator flush its own buffer before parking) whose
//! violation could free a still-reachable young object (use-after-free).
//! INVARIANT: `flush_all` may run only at STW, with all mutators parked.
//!
//! SECURITY FIX (V6) — table-id scoping (regression fix): the per-thread
//! buffer is process-global, but a buffered byte-offset is meaningful only
//! relative to the `base_addr` of the *specific* `CardTable` it was dirtied
//! against. With more than one live `CardTable` (e.g. many `GenerationalHeap`
//! instances across parallel test threads) the original V6 `flush_all` drained
//! (and emptied) buffer entries belonging to OTHER tables, stealing their
//! buffered old→young offsets and causing the victim collector to miss roots
//! and free still-reachable young objects. To fix this every table is given a
//! unique [`CardTable::id`] and every buffered entry is tagged `(table_id,
//! offset)`. `flush_all`/`flush_dirty_buffer` drain ONLY entries whose
//! `table_id == self.id` (via `Vec::retain`), leaving other tables' entries
//! intact. The global cross-thread registry is unchanged, so a collector for
//! table A still drains table A's offsets buffered by OTHER threads — the V6
//! property is preserved while the cross-table theft is eliminated.

//! STATUS OF THE BUFFERED PIPELINE (gengc-round2, 2026-09-20): everything
//! below that exists to buffer a card offset per thread and fold it in at a
//! safepoint — `THREAD_DIRTY_BUFFER`, `BUFFER_REGISTRY`, `DirtyPartitions`,
//! `DirtyBufferGuard`, `thread_local_dirty`, `thread_local_dirty_addr`,
//! `flush_dirty_buffer`, `flush_all`, `drain_pending`, `pending_offsets` — has
//! had **no production caller** since gc-genpause F5.2 replaced it with
//! [`CardTable::mark_dirty_lockfree`]. It is retained DELIBERATELY, not by
//! oversight: it is the intended home for a future refinement-thread or
//! SATB-style barrier, where a per-thread queue is the point rather than an
//! indirection. A reader must not infer from its presence that a mutator uses
//! it — `gen_heap::write_barrier` does not, and neither does the JIT.
//! See `docs/internal/gaps/gengc-oldgen-dead-buffered-card-pipeline-20260920.md`.
//!
//! BECAUSE it is retained for a future caller, the two hazards that only a
//! buffered caller could trip are now CLOSED rather than merely recorded
//! (gengc-round4, 2026-09-21 — see
//! `docs/internal/gc/gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`):
//!
//!  * [`CardTable::clear_all`] FOLDS `pending_offsets` into the freshly-clean
//!    bitmap instead of dropping it. A pending offset names a real old→young
//!    store; dropping one loses the edge permanently and the next young
//!    collection frees its referent, while re-dirtying its card can only
//!    over-retain. Today's single production caller sees an empty queue, so
//!    this is a no-op for it and a correctness property for the next one.
//!  * [`DirtyPartitions::push`] filters a store whose card index equals the
//!    bucket's last entry's, so a tight loop writing one hot field collapses
//!    to a single buffered entry instead of one `usize` per iteration through
//!    an auto-flush into an unbounded shared queue.
//!
//! Neither is reachable today. That is the point: an invariant nothing
//! exercises is an invariant that rots, and these two were the rot this
//! pipeline's retention was already paying for.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

/// Number of bytes covered by a single card.
///
/// Derived from [`cratonvm_types::CARD_SIZE_BYTES`], which is the single owner:
/// `g1_cards::G1_CARD_SHIFT` and the x64 emitter's `shr` both come from the same
/// place, and the emitter's copy is baked into machine code. See that constant
/// for why three independent values bound by two comments was a hazard rather
/// than a duplication.
pub const CARD_SIZE: usize = cratonvm_types::CARD_SIZE_BYTES;

/// Card state: no old→young references detected since last GC.
pub const CARD_CLEAN: u8 = 0;

/// Card state: a reference store from old gen to young gen occurred.
pub const CARD_DIRTY: u8 = 1;

/// T5.5.2 — number of entries after which the per-thread buffer is
/// auto-flushed into the shared card table.
pub const THREAD_BUFFER_FLUSH_THRESHOLD: usize = 64;

/// How many card bytes the collector-side scans probe in one group before
/// deciding whether any of them needs individual attention.
///
/// PERF (gengc-round2, 2026-09-20). Every collector-side pass over the byte
/// map — [`CardTable::take_dirty_cards`], [`CardTable::clear_all`],
/// [`CardTable::dirty_card_indices`] — used to be a per-card
/// `load(Acquire)` + compare + branch. That is three to four cycles per card
/// of pure loop overhead on a map that is overwhelmingly clean, and it is the
/// whole of the measured cost: the round-1 probe run recorded
/// `refinement_ms=443.6` across `passes=1511` with `rset_bytes=262144`
/// (i.e. 262,144 cards) while finding `dirty_scanned=1980` cards in total —
/// about 1.3 cards per pass. Essentially all 443.6 ms was the scan finding
/// nothing.
///
/// A group probe replaces the per-card branch with one branch per
/// [`SCAN_GROUP`] cards: the loads are `Relaxed` and OR-folded, so the
/// compiler can unroll and schedule them freely (an `Acquire` load is an
/// `LDAR` on aarch64 and cannot be), and the per-card `Acquire` read is paid
/// only inside a group that actually holds something. No measurement of the
/// improvement is claimed here — this session could not build or run.
///
/// The value is a cache-line-friendly power of two and is **not tuned**.
const SCAN_GROUP: usize = 8;

/// Is any card in `group` non-clean?
///
/// `Relaxed` on purpose: this is a nomination pass, not the pass that
/// establishes ordering. Every byte it nominates is re-read with `Acquire` by
/// the caller before anything is done with it, and a byte it passes over would
/// equally have been passed over by an `Acquire` load — an acquire load does
/// not read a fresher value, it constrains what else the reader then sees.
/// Relaxed byte loads are also the only form the compiler is free to unroll
/// and schedule; an `Acquire` load is an `LDAR` on aarch64 and pins the loop
/// to one card per iteration. See [`SCAN_GROUP`].
#[inline]
fn group_has_dirty(group: &[AtomicU8]) -> bool {
    let mut acc = CARD_CLEAN;
    for card in group {
        acc |= card.load(Ordering::Relaxed);
    }
    acc != CARD_CLEAN
}

/// Sentinel for [`CardTable::scan_bound`]: no caller has published a bound, so
/// every scan covers the whole map. This is the default and it is what makes
/// the bound opt-in — a table nobody wires behaves exactly as it did before
/// the bound existed.
const SCAN_BOUND_UNSET: usize = usize::MAX;

/// SECURITY FIX (V6): a thread's dirty buffer, shared via `Arc` between the
/// owning mutator (fast path) and the global [`BUFFER_REGISTRY`] so the
/// collector can drain it at a stop-the-world safepoint even if the owning
/// thread parked without flushing.
///
/// SECURITY FIX (V6) — table-id scoping: offsets are kept partitioned BY
/// `table_id`, so a collector draining one table never consumes another
/// table's buffered offsets. The single global buffer per thread therefore
/// multiplexes the dirty edges of *all* tables that thread has touched.
///
/// PERF (gc-cardtable-perf): the partition is materialised as a small
/// `table_id -> Vec<offset>` vec-map ([`DirtyPartitions`]) rather than a flat
/// `Vec<(table_id, offset)>`. Previously every flush had to `Vec::retain`-scan
/// the *entire* shared buffer (all tables' entries) to extract one table's
/// offsets; with N live `CardTable`s that made each flush O(total buffered)
/// instead of O(this table's buffered). Partitioning lets a flush locate its
/// own bucket and `std::mem::take` it in one move, never touching foreign
/// tables' entries. The set of cards ultimately marked dirty is unchanged —
/// only the per-flush work and the layout differ.
///
/// The inner `Mutex` is contended only in the (impossible-by-construction at
/// STW) case where a mutator runs concurrently with the collector's drain. By
/// the GC's stop-the-world invariant every mutator is parked at a safepoint
/// before the collector drains, so the lock is effectively uncontended on the
/// fast path; it exists purely to make the cross-thread read at STW sound
/// (a bare `RefCell` is `!Sync` and could not legally be read by the
/// collector thread).
type ThreadBuffer = Arc<Mutex<DirtyPartitions>>;

/// PERF (gc-cardtable-perf): per-thread dirty-offset storage partitioned by
/// `CardTable::id`. Most threads touch only one or two tables, so a linear
/// vec-map keyed on `table_id` is both the smallest and the fastest structure
/// here — it avoids per-push hashing/allocation churn while letting a flush
/// find and take exactly its own table's offsets without scanning foreign
/// entries (the regression the previous flat `Vec<(table_id, offset)>` had,
/// where every flush `retain`-walked all tables' buffered offsets).
#[derive(Default)]
struct DirtyPartitions {
    /// One bucket per distinct `table_id` seen on this thread. `buckets` is
    /// expected to hold a single-digit number of entries (one per live
    /// `CardTable` the thread has dirtied), so a linear lookup is optimal.
    buckets: Vec<(u64, Vec<usize>)>,
}

impl DirtyPartitions {
    /// Append `offset` to `table_id`'s bucket, creating it on first use, and
    /// return that bucket's length so the caller can evaluate the per-table
    /// auto-flush threshold without a second lookup.
    ///
    /// # LAST-CARD FILTER (gengc-round4, 2026-09-21)
    ///
    /// A push whose CARD INDEX equals that of the bucket's last entry is
    /// dropped. This is the "cheapest dedup" that
    /// `gen-card-table-latent-hazards-20260920-RETIRED-20260921.md` asked
    /// for, and it closes the second of that page's two hazards: without it a
    /// tight loop writing one hot reference field queued one `usize` per
    /// iteration, every one of them naming the same card, into a bucket that
    /// auto-flushes at
    /// [`THREAD_BUFFER_FLUSH_THRESHOLD`] into the UNBOUNDED shared
    /// `pending_offsets` — memory proportional to the store count, and one
    /// failed CAS per entry for [`CardTable::drain_pending`] at GC start.
    /// With it, that loop collapses to a single entry.
    ///
    /// It is a last-entry compare, not a set: one predicted-not-taken branch
    /// and one already-hot load on the buffered fast path, no allocation, no
    /// hashing. It therefore filters RUNS of the same card and nothing else —
    /// an alternating `A, B, A, B` store pattern still buffers every entry,
    /// and `drain_pending`'s CAS remains the authority on what is genuinely a
    /// duplicate. That is deliberate: the exact filter is a per-thread card
    /// set, which puts an allocation on a mutator path, and the run case is
    /// the one that is unbounded.
    ///
    /// Correctness: dropping a push is only ever a LOSS of a duplicate. The
    /// surviving entry names the same card, so the card still becomes dirty at
    /// the next drain, and a card is the whole granularity of this table — two
    /// offsets inside one card are indistinguishable to every consumer. The
    /// filter can therefore never drop an old→young edge.
    #[inline]
    fn push(&mut self, table_id: u64, offset: usize) -> usize {
        for (id, offsets) in self.buckets.iter_mut() {
            if *id == table_id {
                if let Some(&last) = offsets.last() {
                    if last / CARD_SIZE == offset / CARD_SIZE {
                        return offsets.len();
                    }
                }
                offsets.push(offset);
                return offsets.len();
            }
        }
        self.buckets.push((table_id, vec![offset]));
        1
    }

    /// Take (remove and return) all buffered offsets for `table_id`, leaving
    /// other tables' buckets untouched. Returns an empty `Vec` when this
    /// thread has nothing buffered for `table_id`.
    #[inline]
    fn take(&mut self, table_id: u64) -> Vec<usize> {
        for i in 0..self.buckets.len() {
            if self.buckets[i].0 == table_id {
                // Remove the whole bucket: its offsets are about to be folded
                // into the table's shared `pending_offsets`, so the per-thread
                // entry is fully consumed. `swap_remove` is O(1) and bucket
                // ordering is irrelevant.
                return self.buckets.swap_remove(i).1;
            }
        }
        Vec::new()
    }
}

/// SECURITY FIX (V6): global intrusive registry of every thread's dirty
/// buffer. The collector walks this list at STW (via [`CardTable::flush_all`])
/// and folds every thread's buffered offsets into `pending_offsets`, closing
/// the cross-thread use-after-free hole where a mutator could buffer up to
/// `THREAD_BUFFER_FLUSH_THRESHOLD - 1` old→young edges and then park at a
/// safepoint without flushing, causing a minor GC to miss the root and free a
/// still-reachable young object.
///
/// Entries are `Arc` clones of the per-thread buffers; a thread removes its
/// entry on exit (see [`DirtyBufferGuard`]) so the registry never holds a
/// dangling buffer. Identity is matched on `Arc::as_ptr` so removal is exact.
static BUFFER_REGISTRY: Mutex<Vec<ThreadBuffer>> = Mutex::new(Vec::new());

/// SECURITY FIX (V6) — table-id scoping: monotonic source of process-unique
/// [`CardTable::id`] values. Starts at 1 so 0 can serve as a never-assigned
/// sentinel in tests/debugging. Wraparound after 2^64 tables is not a
/// practical concern.
static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

/// SECURITY FIX (V6): RAII handle stored in TLS. Holds the thread's shared
/// dirty buffer and removes it from [`BUFFER_REGISTRY`] when the thread exits,
/// so the collector never dereferences a freed buffer.
struct DirtyBufferGuard {
    buffer: ThreadBuffer,
}

impl Drop for DirtyBufferGuard {
    fn drop(&mut self) {
        // DoHead freed-while-live fix (2026-07-15): the previous behavior
        // ALWAYS deregistered here, discarding any residual buffered offsets
        // on the rationale that "a thread that is tearing down is no longer a
        // GC root and holds no live references the collector must preserve".
        // That conflated the thread's STACK roots with the HEAP edges its
        // writes created: a buffered offset records an old→young reference
        // stored INTO THE HEAP (e.g. a Tomcat worker's final responses
        // installing fresh nodes into the static `FastHttpDateFormat`
        // ConcurrentLinkedQueue via CAS), and that edge outlives the thread.
        // Discarding up to THREAD_BUFFER_FLUSH_THRESHOLD-1 such records per
        // exiting thread left the next minor GC blind to the edge, freeing a
        // still-referenced young object — under thread churn (a Tomcat
        // start/stop per test parameterization retires a whole worker pool)
        // plus GC pressure this zeroed live ConcurrentLinkedQueue nodes and
        // surfaced as per-response NPEs / truncated responses / wild-pointer
        // SIGSEGVs across the DoHead family (and, historically, the
        // BouncyCastle FixedPointTest premature reclamation this table's
        // RSET audit was built for).
        //
        // Fix: if any bucket still holds offsets, LEAVE the registry entry in
        // place. The registry's `Arc` keeps the storage alive with no owner
        // thread; the collector's `flush_all` (STW) drains it like any other
        // buffer and reaps the orphan once it is empty (see the reap logic
        // there). An empty buffer deregisters immediately, as before.
        // Lock order (registry → buffer) matches `flush_all`.
        let self_ptr = Arc::as_ptr(&self.buffer);
        let mut reg = BUFFER_REGISTRY.lock();
        if !self.buffer.lock().buckets.is_empty() {
            return;
        }
        if let Some(pos) = reg.iter().position(|b| Arc::as_ptr(b) == self_ptr) {
            reg.swap_remove(pos);
        }
    }
}

thread_local! {
    /// T5.5.2 — per-thread write buffer of byte offsets (into the
    /// region covered by the card table) that have been dirtied.
    ///
    /// Mutators write here on the fast path instead of touching the
    /// shared `CardTable`. When the buffer fills or a safepoint fires,
    /// [`CardTable::flush_dirty_buffer`] drains it into the
    /// shared-side `pending_offsets` vector under the mutex.
    ///
    /// SECURITY FIX (V6): the buffer is an `Arc<Mutex<DirtyPartitions>>`
    /// (not a bare `RefCell`) registered in [`BUFFER_REGISTRY`] on first use
    /// so the collector can drain it cross-thread at STW. Offsets are
    /// partitioned by `table_id` so a per-table drain consumes only its own
    /// entries without scanning foreign tables' offsets (PERF
    /// gc-cardtable-perf). [`DirtyBufferGuard`] deregisters it on thread exit.
    static THREAD_DIRTY_BUFFER: DirtyBufferGuard = {
        let buffer: ThreadBuffer = Arc::new(Mutex::new(DirtyPartitions::default()));
        BUFFER_REGISTRY.lock().push(Arc::clone(&buffer));
        DirtyBufferGuard { buffer }
    };
}

/// Authoritative card-bitmap state. Locked exclusively by the collector
/// during GC; never touched by the mutator fast path.
struct CardCells {
    /// One stable atomic byte per card (CARD_CLEAN or CARD_DIRTY).
    ///
    /// The x64 JIT may perform a release byte-store directly into this backing
    /// array after a reference store. The vector is never resized, so
    /// [`CardTable::jit_cards_addr`] remains valid for the table's lifetime.
    cards: Vec<AtomicU8>,
    /// Tracking list of card indices that have been dirtied since the last scan.
    dirty_cards: Vec<usize>,
}

/// A byte-map card table for tracking old→young cross-generation references.
///
/// Each byte covers `CARD_SIZE` bytes of the old generation's address space.
/// The write barrier marks cards dirty; the minor GC scans dirty cards and
/// then clears them.
///
/// All public methods take `&self`. Internally, the bitmap is locked
/// (`cells`) and the pending-offset queue is locked
/// (`pending_offsets`). The mutator fast path
/// ([`Self::thread_local_dirty_addr`]) writes only to a per-thread
/// buffer; the global state is updated lazily either when the buffer
/// hits its auto-flush threshold or when the collector calls
/// [`Self::drain_pending`] at GC start.
pub struct CardTable {
    /// SECURITY FIX (V6) — table-id scoping: process-unique identifier for
    /// this table, assigned from [`NEXT_TABLE_ID`] in [`CardTable::new`].
    /// Every offset this table buffers into a per-thread registry buffer is
    /// tagged with this id so draining is table-scoped and one table can never
    /// steal another's buffered old→young edges.
    id: u64,
    /// Base address of the memory region this table covers (immutable).
    base_addr: usize,
    /// Total size of the covered region in bytes (immutable).
    region_size: usize,
    /// Address of `cells.cards`' backing buffer, and its length.
    ///
    /// # Why the map is reachable without the mutex (gc-genpause F5.2)
    ///
    /// The JIT's inline post barrier has always written this array directly
    /// through [`Self::jit_cards_addr`] -- a single release byte-store, no
    /// lock. The Rust-side barrier could not, because the only handle it had
    /// was behind `cells`, so it went the long way round: a TLS lookup, an
    /// `Arc` deref, a `parking_lot::Mutex`, a linear scan of a table-id
    /// vec-map and a `Vec::push` that may reallocate -- per reference store,
    /// with no deduplication, so a hot field pushed the same offset thousands
    /// of times for `drain_pending` to CAS-loop over later.
    ///
    /// Caching the address here lets the two barriers be ONE implementation:
    /// a bounds check, a shift, a load and a conditional byte store. That
    /// equivalence is also the precondition for ever re-enabling
    /// `inline_card_mark_available`, which is off today because a WildFly
    /// audit found a case the emitter did not cover -- an audit that is much
    /// easier to do against one card-marking rule than against two.
    ///
    /// Stable for the table's lifetime: `cards` is allocated once in
    /// [`Self::new`] and never pushed to, resized or reallocated (only its
    /// elements are stored to), and moving the `Vec` -- as `new` does when it
    /// hands ownership to `CardCells` -- does not move its heap buffer. The
    /// same reasoning `jit_cards_addr` has always relied on.
    cards_addr: usize,
    num_cards: usize,
    /// Exclusive bitmap state — touched only by collector-side methods.
    cells: Mutex<CardCells>,
    /// T5.5.2 — pending byte offsets submitted by thread-local buffers
    /// via [`CardTable::flush_dirty_buffer`], waiting to be folded into
    /// `cards` / `dirty_cards` at the next safepoint.
    pending_offsets: Mutex<Vec<usize>>,
    /// Exclusive upper bound, in cards, on what the collector-side scans have
    /// to look at — or [`SCAN_BOUND_UNSET`] when nobody has published one.
    ///
    /// # What this is for
    ///
    /// The map covers the old generation's whole CAPACITY, but a young pause's
    /// cost is supposed to track the young live set. A 128 MiB old generation
    /// is 262,144 cards whether it holds one object or a million, and both
    /// [`Self::take_dirty_cards`] and [`Self::clear_all`] walk all of them on
    /// every moving cycle. See
    /// `docs/internal/gaps/gengc-oldgen-card-scan-is-o-heap-20260920.md`.
    ///
    /// # The contract, which is the whole of the safety argument
    ///
    /// A publisher promises: **no card at or above this index can be dirty,
    /// now or before the next publish.** A broken promise is a lost old→young
    /// root, i.e. a live young object freed — the worst failure this file has.
    ///
    /// The one bound that can honestly make that promise is the old
    /// generation's high-water mark (`OldGen::high_water`): a card index is
    /// derived from the address of an OBJECT (both barriers compute
    /// `(src_addr - old_base) / CARD_SIZE` from the holder's header), and
    /// every old-gen object lies below the high-water mark by construction.
    /// That argument survives the JIT's raw byte store, which is what makes it
    /// different from a summary map the generated code would have to maintain.
    ///
    /// It does **not** survive a stale publish: promotion during a pause moves
    /// the high-water mark, and a mutator that resumes and stores into a
    /// freshly promoted object dirties a card the old bound excludes. So a
    /// publisher must refresh the bound after any old-gen allocation and
    /// before mutators resume. The collector-side mark paths
    /// ([`Self::mark_dirty`], [`Self::mark_dirty_bulk`],
    /// [`Self::drain_pending`]) and the Rust barrier
    /// ([`Self::mark_dirty_lockfree`]) all raise the bound themselves as a
    /// belt-and-braces, so the only hole left is a card dirtied EXCLUSIVELY by
    /// generated code above a stale bound.
    ///
    /// **Nothing publishes a bound today.** `Compiler::inline_card_mark_available()`
    /// is a hard `false`, so no generated code stores into this map at all, but
    /// `jit_card_table_info` is wired unconditionally — publishing on that
    /// basis is a landmine that detonates when the emitter is re-enabled. The
    /// mechanism is here, tested, and OFF; see the gap page for the wiring and
    /// for why it must land with the emitter decision rather than before it.
    ///
    /// Monotone upward once set (see [`Self::set_scan_bound_bytes`]), so a
    /// race can only ever over-scan.
    scan_bound: AtomicUsize,
    /// Cards the bound let the collector-side scans skip, totalled over this
    /// table's life. Zero while `scan_bound` is [`SCAN_BOUND_UNSET`], which is
    /// how a reader tells "the bound is off" from "the bound is on and buying
    /// nothing". Per-table, not a process global.
    bounded_skips: AtomicU64,
}

impl CardTable {
    /// Create a new card table covering a memory region starting at `base_addr`
    /// with the given `region_size` in bytes.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        let num_cards = region_size.div_ceil(CARD_SIZE);
        // Allocate the byte map first so its buffer address can be cached
        // outside the mutex (see `cards_addr`). Moving the `Vec` into
        // `CardCells` below does not move the buffer this points at.
        let cards: Vec<AtomicU8> = (0..num_cards).map(|_| AtomicU8::new(CARD_CLEAN)).collect();
        let cards_addr = cards.as_ptr() as usize; // Cast: stable buffer base
        Self {
            // SECURITY FIX (V6): assign a process-unique id so buffered
            // offsets can be drained table-scoped (Relaxed is sufficient: we
            // only need uniqueness, not ordering relative to other memory).
            id: NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            base_addr,
            region_size,
            cards_addr,
            num_cards,
            cells: Mutex::new(CardCells {
                cards,
                dirty_cards: Vec::new(),
            }),
            pending_offsets: Mutex::new(Vec::new()),
            // Unbounded until somebody makes the promise in `scan_bound`'s
            // doc. Nothing does today.
            scan_bound: AtomicUsize::new(SCAN_BOUND_UNSET),
            bounded_skips: AtomicU64::new(0),
        }
    }

    // -----------------------------------------------------------------
    // Collector-side scan bound (gengc-round2)
    // -----------------------------------------------------------------

    /// Publish an upper bound on the covered region that any dirty card can
    /// fall in, as a byte count from [`Self::base_addr`].
    ///
    /// The intended argument is `OldGen::high_water()` — see the
    /// [`Self::scan_bound`] field doc for the safety contract, which is the
    /// whole of this feature. The bound only ever moves UP after the first
    /// publish, so an interleaving of two publishers can over-scan but never
    /// under-scan.
    ///
    /// Calling this with anything smaller than the old generation's high-water
    /// mark is a correctness bug with a premature-reclamation failure mode.
    /// If in doubt, do not call it: an unwired table scans everything, exactly
    /// as it always has.
    pub fn set_scan_bound_bytes(&self, covered_bytes: usize) {
        let want = covered_bytes.div_ceil(CARD_SIZE).min(self.num_cards);
        let mut cur = self.scan_bound.load(Ordering::Relaxed);
        loop {
            // The sentinel is `usize::MAX`, so a plain `max` would pin the
            // bound at "unbounded" for ever; the first publish replaces it
            // outright and every later one raises it.
            let new = if cur == SCAN_BOUND_UNSET {
                want
            } else {
                cur.max(want)
            };
            if new == cur {
                return;
            }
            match self.scan_bound.compare_exchange_weak(
                cur,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Exclusive card index the collector-side scans stop at. Equal to
    /// [`Self::num_cards`] when no bound has been published.
    pub fn scan_bound_cards(&self) -> usize {
        let bound = self.scan_bound.load(Ordering::Acquire);
        if bound == SCAN_BOUND_UNSET {
            self.num_cards
        } else {
            bound.min(self.num_cards)
        }
    }

    /// Cards the published bound has let the collector-side scans skip over
    /// this table's life. `0` with a bound set means the bound is inert.
    pub fn cards_skipped_by_scan_bound(&self) -> u64 {
        self.bounded_skips.load(Ordering::Relaxed)
    }

    /// Raise the bound so that `index` is inside it. Called from every mark
    /// path so a card the collector itself dirties above a stale bound cannot
    /// be lost; see the [`Self::scan_bound`] field doc for what this does and
    /// does not cover.
    #[inline]
    fn cover_card_index(&self, index: usize) {
        // One relaxed load of a read-mostly line on the fast path, and a
        // perfectly-predicted not-taken branch. The slow arm is a CAS loop
        // that runs at most once per card index the table has never seen.
        if index >= self.scan_bound.load(Ordering::Relaxed) {
            self.set_scan_bound_bytes(
                index
                    .saturating_add(1)
                    .saturating_mul(CARD_SIZE)
                    .min(self.region_size),
            );
        }
    }

    /// Account for the cards a scan did not have to look at.
    #[inline]
    fn note_bounded_skip(&self, limit: usize) {
        if limit < self.num_cards {
            self.bounded_skips
                .fetch_add((self.num_cards - limit) as u64, Ordering::Relaxed);
        }
    }

    /// Mark the card containing `addr` as dirty.
    ///
    /// Addresses outside the covered region are silently ignored. This is safe
    /// because the write barrier may fire for allocations that straddle region
    /// boundaries during concurrent GC promotion.
    ///
    /// This is the *slow* path — used by GC-internal code paths that
    /// have already established exclusive access (e.g. re-marking cards
    /// for promoted objects). The mutator write barrier MUST NOT call
    /// this directly; it should route through
    /// [`Self::thread_local_dirty_addr`].
    #[inline]
    pub fn mark_dirty(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        let index = (addr - self.base_addr) / CARD_SIZE;
        self.cover_card_index(index);
        let mut cells = self.cells.lock();
        if index < cells.cards.len()
            && cells.cards[index]
                .compare_exchange(CARD_CLEAN, CARD_DIRTY, Ordering::Release, Ordering::Relaxed)
                .is_ok()
        {
            cells.dirty_cards.push(index);
        }
    }

    /// gc-genpause F5.2 -- THE mutator write-barrier fast path: mark the card
    /// containing `addr` dirty with no lock and no allocation.
    ///
    /// A bounds check, a shift, a relaxed load and, only if the card is not
    /// already dirty, one release byte-store. This is byte-for-byte the same
    /// rule the JIT's inline post barrier emits
    /// (`Compiler::emit_inline_card_mark_regs`), which is the point: the
    /// interpreter, the natives and compiled code now dirty a card the same
    /// way, so there is one card-marking rule in this VM instead of two.
    ///
    /// # What it replaces
    ///
    /// [`Self::thread_local_dirty_addr`], which buffered the offset through a
    /// TLS lookup, an `Arc`, a `parking_lot::Mutex`, a linear table-id lookup
    /// and a growable `Vec`, for [`Self::drain_pending`] to fold into this
    /// same byte map at the next safepoint. That pipeline exists because the
    /// map was only reachable under `cells`; it is not needed now that
    /// `cards_addr` is cached, and it never deduplicated -- a field written in
    /// a loop buffered one entry per store, all of them for the same card.
    /// `duplicate_card_marks` was the counter that measured exactly that.
    ///
    /// # The conditional store is not just an optimisation
    ///
    /// Re-storing `CARD_DIRTY` over an already-dirty byte is a write to a line
    /// that every other storing mutator may hold, so an unconditional mark
    /// ping-pongs the card line between cores on exactly the workload -- many
    /// threads mutating one region -- where the barrier is hottest. Reading
    /// first keeps a steady-state hot card read-shared.
    ///
    /// # Ordering
    ///
    /// The store is `Release` and pairs with the `Acquire` load in
    /// [`Self::take_dirty_cards`], which is the same pairing the JIT's inline
    /// store already documents. The pre-read is `Relaxed`: a stale `clean`
    /// only costs a redundant store, and a stale `dirty` cannot happen -- a
    /// card is only cleared at STW, when no mutator is running this.
    ///
    /// # Why it does not touch `dirty_cards`
    ///
    /// It cannot -- that list lives under the mutex this path exists to avoid.
    /// It does not need to: `take_dirty_cards` scans the whole byte map and
    /// merges it with the list precisely because the JIT's direct stores were
    /// already invisible to it. Marks made here are found the same way.
    #[inline]
    pub fn mark_dirty_lockfree(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        let index = (addr - self.base_addr) / CARD_SIZE;
        if index >= self.num_cards {
            return;
        }
        // SAFETY: `cards_addr` is the base of a `[AtomicU8; num_cards]` that
        // lives as long as `self` and is never reallocated (see the field's
        // doc comment), and `index < num_cards` was just checked.
        let card = unsafe { &*(self.cards_addr as *const AtomicU8).add(index) };
        if card.load(Ordering::Relaxed) != CARD_DIRTY {
            card.store(CARD_DIRTY, Ordering::Release);
            // Keep a published scan bound honest for marks this path makes.
            // Costs one relaxed load of a read-mostly line when no bound is
            // published (the `SCAN_BOUND_UNSET` sentinel is `usize::MAX`, so
            // the compare is always false), which is the default.
            self.cover_card_index(index);
        } else {
            // gengc-round2: attribute the duplicate mark HERE.
            //
            // The duplicate counter used to be fed only by `drain_pending`,
            // which has drained nothing since gc-genpause F5.2 moved the
            // barrier to this function — so `duplicate_mark_ratio`, the number
            // that argues for or against a per-thread last-card filter in the
            // barrier, was structurally zero and read as "the barrier never
            // duplicates" rather than "nobody is counting". The round-1 probe
            // run confirms it: `duplicate_marks=0` beside
            // `dirty_scanned=1980`.
            //
            // This branch is the only place left that can tell a mark which
            // dirtied a clean card from one that hit an already-dirty card,
            // and the `!= CARD_DIRTY` test it hangs off is already being
            // evaluated. The counter itself is a process-global `fetch_add`,
            // which is exactly the cache-line ping-pong `gc_metrics`' header
            // refuses to put on the barrier — so it is behind the SAME gate as
            // `record_card_mark`, default off, costing one relaxed load of an
            // already-resolved read-only-shared byte when it is off.
            //
            // Like `card_marks_executed`, it therefore counts interpreter and
            // native marks only; the JIT's inline store never enters here.
            //
            // That symmetry is the point, and it supplies the honest
            // DENOMINATOR round 1 said was missing:
            // `gen_heap::write_barrier` calls `record_card_mark` on both arms
            // of this branch and under the same gate, so with
            // `CRATONVM_GC_CARD_METRICS=1`,
            // `duplicate_card_marks_barrier / card_marks_executed` is exactly
            // the fraction of Rust-barrier marks that hit an already-dirty
            // card. `gc_metrics` renders it as
            // `dup_barrier_over_card_marks=`, on the armed barrier line.
            //
            // gengc-round3: this used to bump `record_duplicate_card_marks`,
            // the BUFFERED counter that `drain_pending` also feeds, behind a
            // gate spelled out here at the call site. Two things were wrong
            // with that and both are fixed:
            //
            //  * one counter fed by a gated per-store site and an ungated
            //    per-drain site has no denominator at all — the sum is on two
            //    clocks — which is why the old `duplicate_mark_ratio` could
            //    not be repaired by changing its divisor. The numerator is now
            //    split, and this arm feeds the barrier half only;
            //  * the gate lived here, so keeping this counter armed in lockstep
            //    with its denominator was a convention rather than a
            //    structure. `record_duplicate_card_mark_barrier` now tests the
            //    gate itself, exactly as `record_card_mark` does, so the
            //    numerator and the denominator cannot be armed separately.
            //
            // See `gengc-plumbing-duplicate-card-mark-denominator-20260920.md`.
            crate::gc_metrics::record_duplicate_card_mark_barrier();
        }
    }

    /// Bulk-mark a slice of addresses dirty in a single lock acquisition.
    ///
    /// Equivalent to calling [`Self::mark_dirty`] for each address but
    /// amortises the `cells` mutex over the entire batch, which matters
    /// when the collector re-marks every deferred cross-gen card after a
    /// minor GC (formerly N lock acquisitions, now one).
    ///
    /// Addresses outside the covered region are silently ignored, matching
    /// the behaviour of [`Self::mark_dirty`].
    pub fn mark_dirty_bulk(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        let mut cells = self.cells.lock();
        let end = self.base_addr + self.region_size;
        for &addr in addrs {
            if addr < self.base_addr || addr >= end {
                continue;
            }
            let index = (addr - self.base_addr) / CARD_SIZE;
            self.cover_card_index(index);
            if index < cells.cards.len()
                && cells.cards[index]
                    .compare_exchange(CARD_CLEAN, CARD_DIRTY, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
            {
                cells.dirty_cards.push(index);
            }
        }
    }

    /// Check if a specific card is dirty.
    ///
    /// PERF (gengc-round1, 2026-09-20): reads the byte map through
    /// `cards_addr` instead of taking the collector's `cells` mutex. The lock
    /// protects `dirty_cards`, which this does not touch; the byte itself is
    /// an `AtomicU8` that [`Self::mark_dirty_lockfree`] and the JIT's inline
    /// post barrier already store into without the lock, so holding it bought
    /// nothing and cost an acquire/release per query. The callers that matter
    /// ask this in a LOOP over card indices (`gen_heap`'s two remembered-set
    /// verifiers), which turned an O(cards) scan into O(cards) uncontended
    /// mutex round trips.
    ///
    /// The acquire load and its pairing with the release store of
    /// `CARD_DIRTY` are unchanged — that pairing, not the mutex, is what makes
    /// a JIT-written mark visible here.
    pub fn is_dirty(&self, card_index: usize) -> bool {
        if card_index >= self.num_cards {
            return false;
        }
        // SAFETY: `cards_addr` is the base of a `[AtomicU8; num_cards]` that
        // lives as long as `self` and is never reallocated (see the field's
        // doc comment), and `card_index < num_cards` was just checked.
        let card = unsafe { &*(self.cards_addr as *const AtomicU8).add(card_index) };
        card.load(Ordering::Acquire) == CARD_DIRTY
    }

    /// Clear all cards (set to CARD_CLEAN), then FOLD any pending
    /// thread-local offsets back into the now-clean bitmap.
    ///
    /// # The fold, and why it is not a drop (gengc-round4, 2026-09-21)
    ///
    /// This used to end with `self.pending_offsets.lock().clear()`, justified
    /// as "they would be applied to a now-clean bitmap and represent stale
    /// work from before the clear". That justification is about ORDERING, and
    /// it held only because the single production caller
    /// (`gen_heap::collect_garbage_inner`'s Phase 3) runs at a stop-the-world
    /// point where the queue is provably empty: the mutator barrier
    /// ([`Self::mark_dirty_lockfree`]) does not buffer at all, and the cycle's
    /// own [`Self::flush_all`] + [`Self::drain_pending`] ran at the top of the
    /// same pause.
    ///
    /// Restore a buffered mutator path — which the whole buffered pipeline is
    /// deliberately retained for, see the module header — and the two stop
    /// being the same claim. An old→young edge stored BEFORE the pause but
    /// buffered under the [`THREAD_BUFFER_FLUSH_THRESHOLD`], and moved into
    /// `pending_offsets` by a peer's auto-flush rather than by the collector,
    /// is not stale at all: it is a live edge that this `clear_all` would
    /// silently discard, for good rather than for a cycle, and the next young
    /// collection would free its referent. That is a premature-reclamation
    /// bug one level up from where `take_dirty_cards`' byte reset fixed it.
    ///
    /// A pending offset always names a REAL old→young store, so re-dirtying
    /// its card can only ever over-retain. The fold is therefore correct for
    /// the hypothetical buffered caller and a no-op for the real one, whose
    /// queue is empty — `drain_pending` returns `0` on an empty queue without
    /// taking the cells lock. See
    /// `docs/internal/gc/gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`.
    ///
    /// # PERF (gc-genpause F3): read first, write only what is dirty
    ///
    /// This used to store `CARD_CLEAN` over EVERY card byte -- one release
    /// store per [`CARD_SIZE`] bytes of old gen, on every moving young cycle,
    /// paired with [`Self::take_dirty_cards`]'s own full pass in the SAME
    /// cycle. So a minor collection made two complete linear passes over the
    /// card map regardless of how many cards were actually dirty. Measured at
    /// 8-9 ns/card that is ~9 ms per cycle at a 512 MiB old gen and ~70 ms at
    /// 4 GiB -- a cost that grows with OLD-gen size inside a pause that is
    /// supposed to grow with the YOUNG live set.
    ///
    /// A steady-state card map is overwhelmingly clean, so an acquire load
    /// first and a store only on the bytes that are genuinely dirty replaces
    /// almost every write with a read. The loads still stream the whole map --
    /// the flat byte map stays the single source of truth, and the JIT's
    /// direct card store (`jit_cards_addr`) is deliberately not required to
    /// maintain any summary structure beside it -- but a plain load is far
    /// cheaper than the store it replaces.
    ///
    /// The conditional store is also strictly SAFER than the unconditional one
    /// under a hypothetical concurrent writer: a card dirtied between our load
    /// and our store SURVIVES here, where the old code wiped it. Both are only
    /// ever called at STW; this one fails in the retaining direction if that
    /// ever stops being true.
    /// # PERF (gengc-round2): one branch per [`SCAN_GROUP`] cards, not per card
    ///
    /// The loads still stream the map (bounded by [`Self::scan_bound_cards`],
    /// which is the whole map unless a caller has published a bound), but a
    /// clean group is now one OR-folded probe and one not-taken branch instead
    /// of eight. See [`SCAN_GROUP`].
    pub fn clear_all(&self) {
        let mut cells = self.cells.lock();
        let limit = self.scan_bound_cards().min(cells.cards.len());
        self.note_bounded_skip(limit);
        for group in cells.cards[..limit].chunks(SCAN_GROUP) {
            if !group_has_dirty(group) {
                continue;
            }
            for card in group {
                if card.load(Ordering::Acquire) != CARD_CLEAN {
                    card.store(CARD_CLEAN, Ordering::Release);
                }
            }
        }
        cells.dirty_cards.clear();
        drop(cells);
        // AFTER the byte map is clean and the cells lock is released — the
        // order is the whole point. `drain_pending` takes the cells lock
        // itself, and folding before the clear would simply be wiped by it.
        self.drain_pending();
    }

    /// Collect indices of currently-dirty cards into a `Vec`.
    ///
    /// Returns a snapshot under the cells lock — callers can iterate
    /// freely without holding the lock. For bulk processing prefer
    /// [`Self::take_dirty_cards`], which CONSUMES the set (leaving every
    /// returned card `CARD_CLEAN`) and folds in the O(dirty) tracking list.
    ///
    /// CORRECTION (gengc-round1, 2026-09-20): this used to say
    /// `take_dirty_cards` "uses the O(dirty) tracking list rather than
    /// scanning the whole bitmap". It does not, and cannot — the JIT's inline
    /// post barrier stores `CARD_DIRTY` straight into the byte map without
    /// touching `dirty_cards`, so the tracking list is a *subset* of the dirty
    /// set and the full acquire scan is what makes the answer complete.
    /// `take_dirty_cards` is O(total cards) exactly like this function; what it
    /// saves is the bitmap WRITE, not the read. The claim was repeated
    /// verbatim at `gen_heap.rs`'s `scan_dirty_cards_inner` call site, where it
    /// is the argument for a pause cost the maintainer then believes is
    /// proportional to the dirty set rather than to old-gen size. See
    /// `docs/internal/gaps/gengc-oldgen-card-scan-is-o-heap-20260920.md`.
    pub fn dirty_card_indices(&self) -> Vec<usize> {
        let cells = self.cells.lock();
        let limit = self.scan_bound_cards().min(cells.cards.len());
        self.note_bounded_skip(limit);
        let mut out = Vec::new();
        for (group_index, group) in cells.cards[..limit].chunks(SCAN_GROUP).enumerate() {
            if !group_has_dirty(group) {
                continue;
            }
            let base = group_index * SCAN_GROUP;
            for (j, card) in group.iter().enumerate() {
                if card.load(Ordering::Acquire) == CARD_DIRTY {
                    out.push(base + j);
                }
            }
        }
        out
    }

    /// Return the tracked list of dirty card indices and clear the tracking list
    /// AND reset those cards' bitmap bytes to `CARD_CLEAN`.
    ///
    /// Buffered Rust barriers populate `dirty_cards`; direct JIT card stores
    /// deliberately do not. We therefore merge the tracked list with one
    /// acquire scan of the atomic bitmap. This keeps the JIT mutator path to a
    /// single release byte-store while preserving the exact card set at the
    /// stop-the-world consumer boundary.
    ///
    /// B-K / bt18 fix (2026-06-14): the byte reset is load-bearing for the
    /// NON-MOVING young sweep, which (unlike the moving Cheney path) NEVER calls
    /// [`Self::clear_all`]. `mark_dirty`/`mark_dirty_bulk`/`drain_pending` only
    /// push an index back onto the tracking list when its byte transitions
    /// clean→dirty (`cards[index] != CARD_DIRTY`). If `take_dirty_cards` left the
    /// consumed bytes DIRTY, the sweep's subsequent `mark_dirty_bulk(redirty_*)`
    /// that re-establishes a surviving old→young edge would be a SILENT NO-OP —
    /// the card stays dirty-in-bitmap but absent-from-list, so the next
    /// `take_dirty_cards` never returns it and `scan_dirty_cards` never re-seeds
    /// the edge → the live young child is swept (premature reclamation,
    /// bt18 = 68273854 vs golden 68332206). Clearing the consumed bytes here
    /// restores the bitmap↔list invariant so the re-dirty genuinely re-registers
    /// the card. The moving path is unaffected: its `clear_all` wipes the whole
    /// bitmap anyway, so the early per-card clear is redundant, never harmful.
    ///
    /// # PERF (gc-genpause F3): an acquire LOAD, not an acquire RMW
    ///
    /// The scan used to be `swap(AcqRel)` on every card byte -- a locked
    /// read-modify-write per [`CARD_SIZE`] bytes of old generation, executed
    /// whether or not the byte was dirty. Held-workload measurement, scaling
    /// only the card map (`refinement_ms / passes` from `[GC] cards:`, with
    /// `dirty_scanned=0` in every arm, so the whole cost IS the scan):
    ///
    /// ```text
    ///   old gen    cards      ms/pass   ns/card
    ///    48 MiB     98,304      0.83       8.4
    ///    96 MiB    196,608      1.83       9.3
    ///   192 MiB    393,216      3.20       8.1
    ///   512 MiB  1,048,576      8.26       7.9
    /// ```
    ///
    /// Linear, ~8.3 ns/card, finding nothing. The RMW is what costs that: an
    /// `Acquire` LOAD is a plain `mov` on x86-64 and an `LDAR` on aarch64,
    /// where `swap(AcqRel)` is a `LOCK XCHG` / `LDAXRB-STLXRB` pair.
    ///
    /// The documented pairing is PRESERVED exactly. The JIT's inline post
    /// barrier performs a release byte-store of `CARD_DIRTY`; what has to
    /// happen-after it is an ACQUIRE READ of that byte, which is what the load
    /// below is. The clear then only needs to be a release store, and only on
    /// the bytes that were actually dirty -- which is what the old `swap` did
    /// on those bytes anyway. A clean byte is now read and left alone instead
    /// of being pointlessly rewritten with the value it already holds.
    ///
    /// The bitmap-to-list invariant the bt18 note below depends on is
    /// unchanged: every byte this returns an index for is left `CARD_CLEAN`.
    /// # PERF (gengc-round2): one branch per [`SCAN_GROUP`] cards, not per card
    ///
    /// The `Acquire` load is still what pairs with a release store of
    /// `CARD_DIRTY`, and it is still performed on every card this function
    /// returns an index for. What changed is how a card gets NOMINATED for
    /// that load: a `Relaxed` OR-fold over a group of [`SCAN_GROUP`] bytes
    /// decides whether any of them is worth looking at individually.
    ///
    /// That is not a weakening. Acquire does not make a load see a fresher
    /// value than Relaxed does — it constrains what OTHER memory the reader
    /// is then guaranteed to see — so the group probe and the old per-card
    /// scan have exactly the same window in which a concurrently-made mark can
    /// be missed, and the STW invariant is what rules that window out for
    /// both. A byte the probe reads as `CARD_DIRTY` is then re-read with
    /// `Acquire` before it is acted on, and coherence guarantees the second
    /// load of the same location by the same thread cannot go backwards, so
    /// the documented pairing is intact end to end.
    pub fn take_dirty_cards(&self) -> Vec<usize> {
        let mut cells = self.cells.lock();
        let mut taken = std::mem::take(&mut cells.dirty_cards);
        let limit = self.scan_bound_cards().min(cells.cards.len());
        self.note_bounded_skip(limit);
        for (group_index, group) in cells.cards[..limit].chunks(SCAN_GROUP).enumerate() {
            if !group_has_dirty(group) {
                continue;
            }
            let base = group_index * SCAN_GROUP;
            for (j, card) in group.iter().enumerate() {
                // Acquire load pairs with the JIT's release store of CARD_DIRTY.
                if card.load(Ordering::Acquire) == CARD_DIRTY {
                    card.store(CARD_CLEAN, Ordering::Release);
                    taken.push(base + j);
                }
            }
        }
        taken.sort_unstable();
        taken.dedup();
        taken
    }

    /// Get the start address of the region covered by `card_index`.
    ///
    /// Uses checked arithmetic to avoid overflow on extremely large card
    /// indices, and clamps the result to the end of the covered region so that
    /// `card_start_addr(i) <= card_end_addr(i)` holds for EVERY `i` — an
    /// out-of-range index yields the empty range `[region_end, region_end)`
    /// rather than an inverted one.
    ///
    /// CORRECTION (gengc-round1, 2026-09-20): the doc used to promise "Returns
    /// `None` if the computation would overflow", which this signature cannot
    /// do and never did — it returned a saturated `usize::MAX` while
    /// [`Self::card_end_addr`] clamped to `region_end`, so an overflowing index
    /// produced `start > end`. A caller writing the obvious
    /// `for a in (start..end).step_by(8)` gets an empty loop from that and
    /// silently skips the card; one writing `end - start` gets an underflow
    /// panic. Clamping here makes the pair consistent in the direction that
    /// cannot surprise. (No in-range index is affected: for
    /// `i < num_cards`, `i * CARD_SIZE < region_size` by construction.)
    pub fn card_start_addr(&self, card_index: usize) -> usize {
        let offset = card_index.checked_mul(CARD_SIZE).unwrap_or_else(|| {
            tracing::error!(
                "card_table: card_start_addr overflow in card_index({}) * CARD_SIZE({})",
                card_index,
                CARD_SIZE
            );
            // Return a clamped value instead of aborting the entire VM.
            // The caller should never pass an index this large; this is a
            // defensive fallback so GC can skip the card instead of crashing.
            usize::MAX
        });
        let region_end = self.base_addr.saturating_add(self.region_size);
        self.base_addr.saturating_add(offset).min(region_end)
    }

    /// Get the exclusive end address of the region covered by `card_index`.
    ///
    /// Uses checked arithmetic to avoid overflow on extremely large card indices.
    /// Clamps the result to `base_addr + region_size`.
    pub fn card_end_addr(&self, card_index: usize) -> usize {
        let next_index = card_index.saturating_add(1);
        let offset = next_index.saturating_mul(CARD_SIZE);
        let end = self.base_addr.saturating_add(offset);
        let region_end = self.base_addr.saturating_add(self.region_size);
        end.min(region_end)
    }

    /// The number of cards in this table.
    pub fn num_cards(&self) -> usize {
        self.num_cards
    }

    /// The base address of the covered region.
    pub fn base_addr(&self) -> usize {
        self.base_addr
    }

    /// The size of the covered region.
    pub fn region_size(&self) -> usize {
        self.region_size
    }

    /// Stable address of the first atomic card byte for JIT post barriers.
    ///
    /// The returned storage is valid until this `CardTable` is dropped and is
    /// never resized. A generated release byte-store of `CARD_DIRTY` is paired
    /// with the acquire scan in [`Self::take_dirty_cards`].
    pub fn jit_cards_addr(&self) -> usize {
        // gc-genpause F5.2: cached at construction, so this no longer takes
        // the collector's mutex to hand out an address that has been constant
        // since `new`.
        self.cards_addr
    }

    // -----------------------------------------------------------------
    // T5.5.2 — thread-local batched dirty path
    // -----------------------------------------------------------------

    /// T5.5.2 — fast path for the write barrier.
    ///
    /// Appends `offset` (a byte offset from [`Self::base_addr`]) to the
    /// calling thread's private buffer. This path takes no lock; it is
    /// intended to be called from the mutator for every cross-generation
    /// reference store. When the buffer reaches
    /// [`THREAD_BUFFER_FLUSH_THRESHOLD`] entries it auto-flushes into
    /// the shared pending list via [`Self::flush_dirty_buffer`].
    ///
    /// The final update to the shared bitmap is deferred to a safepoint,
    /// where the collector calls [`Self::drain_pending`] (or
    /// [`Self::flush_all`] then [`Self::drain_pending`]).
    ///
    /// # SECURITY FIX (V6) — table-id scoping
    ///
    /// The offset is appended to *this table's* bucket in the per-thread
    /// [`DirtyPartitions`]. The single per-thread buffer is shared by *all*
    /// tables this thread touches, but each table's offsets live in their own
    /// bucket so a flush of one table never touches another's entries.
    ///
    /// # PERF (gc-cardtable-perf)
    ///
    /// The auto-flush threshold is now evaluated against THIS table's bucket
    /// length (returned by [`DirtyPartitions::push`]) rather than the buffer's
    /// total length across all tables. This is a pure batching/timing detail —
    /// flushing only decides *when* buffered offsets move to `pending_offsets`,
    /// never *which* cards ultimately become dirty — so the observable card
    /// set is identical. The previous total-length policy could trip a flush
    /// on table A merely because table B had buffered a lot; per-bucket sizing
    /// is both more accurate (each table flushes on its own backlog) and keeps
    /// the fast path a single length check.
    pub fn thread_local_dirty(&self, offset: usize) {
        // SECURITY FIX (V6): push into this table's bucket of the registered
        // per-thread buffer (Arc<Mutex<DirtyPartitions>>) so a drain by another
        // table cannot consume it. The lock is uncontended on the mutator fast
        // path (only this thread touches it outside STW); it is acquirable by
        // the collector only at STW, when this thread is parked.
        let should_flush = THREAD_DIRTY_BUFFER.with(|guard| {
            let mut b = guard.buffer.lock();
            b.push(self.id, offset) >= THREAD_BUFFER_FLUSH_THRESHOLD
        });
        if should_flush {
            self.flush_dirty_buffer();
        }
    }

    /// T5.5.2 — Flush the calling thread's private dirty buffer into
    /// the shared pending list. Called on safepoint entry and whenever
    /// a per-thread buffer hits the auto-flush threshold.
    ///
    /// The offsets themselves are *not* yet resolved to card indices —
    /// that happens in [`Self::drain_pending`] under the cells lock, so
    /// the hot-path cost remains a single lock acquisition.
    ///
    /// # SECURITY FIX (V6) — table-id scoping
    ///
    /// Only this table's offsets are moved into its `pending_offsets`; entries
    /// belonging to other tables sharing this thread's buffer are left
    /// untouched, fixing the regression where one table's flush emptied
    /// another table's buffered old→young edges.
    ///
    /// # PERF (gc-cardtable-perf)
    ///
    /// Offsets are partitioned by `table_id` in the per-thread buffer, so this
    /// flush locates its own bucket and `std::mem::take`s it in one move
    /// instead of `Vec::retain`-scanning the entire shared buffer (all tables'
    /// entries). With multiple live tables this turns each flush from
    /// O(total buffered) into O(this table's buffered).
    pub fn flush_dirty_buffer(&self) {
        // Take only this table's bucket while holding the per-thread buffer
        // lock, then release it before touching `pending_offsets`. Foreign
        // tables' buckets are never inspected.
        let offsets = THREAD_DIRTY_BUFFER.with(|guard| guard.buffer.lock().take(self.id));
        if offsets.is_empty() {
            return;
        }
        self.pending_offsets.lock().extend(offsets);
    }

    /// T5.5.2 / SECURITY FIX (V6) — Drain EVERY thread's dirty buffer into
    /// the shared pending list. Called by the collector at GC start (before
    /// `scan_dirty_cards`).
    ///
    /// # SECURITY FIX (V6) — closes a cross-thread use-after-free
    ///
    /// Previously this was a thin alias for [`Self::flush_dirty_buffer`],
    /// which drains only the *calling* (collector) thread's buffer. That
    /// relied on every mutator flushing its OWN buffer before parking at a
    /// safepoint. A mutator that buffered fewer than
    /// [`THREAD_BUFFER_FLUSH_THRESHOLD`] old→young edges and then parked
    /// without flushing would leave those edges invisible to the collector;
    /// the minor GC would miss the old→young root and free a still-reachable
    /// young object (UAF when the mutator resumes and dereferences it).
    ///
    /// The fix removes that cross-thread obligation entirely: the collector
    /// now walks the global [`BUFFER_REGISTRY`] and drains every registered
    /// thread buffer itself. No VM-side safepoint flush is required for
    /// correctness.
    ///
    /// # SECURITY FIX (V6) — table-id scoping (cross-table theft fix)
    ///
    /// From every registered thread buffer this drains ONLY the entries
    /// tagged with `self.id`, leaving entries belonging to other live
    /// `CardTable`s in place (`Vec::retain`, not `Vec::append`). This fixes
    /// the regression where `flush_all` on table A emptied the buffered
    /// old→young offsets that other threads had recorded against table B,
    /// causing B's collector to miss roots and free reachable young objects.
    ///
    /// The V6 cross-thread property is preserved: because the walk still
    /// covers EVERY registered thread buffer, table A's collector drains all
    /// of table A's offsets no matter which thread buffered them — including a
    /// thread that buffered fewer than [`THREAD_BUFFER_FLUSH_THRESHOLD`] edges
    /// and then parked without flushing.
    ///
    /// # Concurrency invariant
    ///
    /// This MUST be called only when mutators are stopped at the
    /// stop-the-world safepoint. Under STW no mutator is executing the write
    /// barrier, so each per-thread buffer mutex is uncontended and a thread
    /// cannot append a new offset between our drain and the subsequent
    /// [`Self::drain_pending`]. We still take each buffer's lock (rather than
    /// reading it racily) so the cross-thread access is well-defined even if
    /// a thread is parked mid-`push`. The registry lock is held for the whole
    /// walk so a concurrently *exiting* thread cannot remove (and free) a
    /// buffer we are about to drain.
    pub fn flush_all(&self) {
        let mut reg = BUFFER_REGISTRY.lock();
        let mut i = 0;
        while i < reg.len() {
            // Lock order matches the mutator fast path (buffer-lock before
            // pending-lock) so the two can never deadlock even if, contrary
            // to the STW invariant, they were ever to run concurrently.
            //
            // PERF (gc-cardtable-perf): take only this table's bucket from
            // each thread buffer, leaving every foreign-table bucket in place
            // for that table's own collector. The take is a single bucket
            // lookup + O(1) `swap_remove`, not a `retain`-scan over all of the
            // thread's buffered offsets across tables.
            let buf = Arc::clone(&reg[i]);
            let (offsets, orphan_empty) = {
                let mut b = buf.lock();
                let offsets = b.take(self.id);
                // DoHead freed-while-live fix (2026-07-15): an exiting thread
                // with residual buffered offsets leaves its entry registered
                // (see `DirtyBufferGuard::drop`) so those heap-edge records
                // survive until a collector drains them. Reap such an orphan
                // once nothing is left in ANY bucket: strong_count == 2 means
                // registry + our local clone only (an owning thread's TLS
                // guard would make it 3), and a dead thread can never push
                // again, so an empty orphan stays empty.
                let orphan_empty = Arc::strong_count(&reg[i]) == 2 && b.buckets.is_empty();
                (offsets, orphan_empty)
            };
            if !offsets.is_empty() {
                self.pending_offsets.lock().extend(offsets);
            }
            if orphan_empty {
                reg.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// T5.5.2 — Fold any thread-submitted offsets from
    /// [`Self::pending_offsets`] into the authoritative `cards` bitmap
    /// and the tracking `dirty_cards` list.
    ///
    /// This is the single pass promised in the design: at a safepoint
    /// the GC thread calls this once and every offset becomes a card
    /// update in O(pending) time.
    ///
    /// Returns the number of distinct new cards that became dirty.
    pub fn drain_pending(&self) -> usize {
        // Snapshot pending offsets under the pending lock, then release
        // the pending lock before acquiring the cells lock so concurrent
        // mutators can continue enqueueing into `pending_offsets` while
        // we update the bitmap.
        let pending = std::mem::take(&mut *self.pending_offsets.lock());
        if pending.is_empty() {
            return 0;
        }
        // Observability (see `crate::gc_metrics`): every offset that does NOT
        // produce a clean->dirty transition is a duplicate mark — the mutator
        // buffered a card that was already dirty. This is the only place in the
        // system that can tell the two apart for the BUFFERED path, and it is
        // off the hot path (once per drain, not per store), so it is counted
        // unconditionally.
        //
        // gengc-round3: `record_duplicate_card_marks` now feeds
        // `duplicate_card_marks_buffered` and nothing else. The barrier's
        // duplicates — the ones that actually happen in production, since this
        // pipeline drains empty buffers — go to
        // `record_duplicate_card_mark_barrier` from `mark_dirty_lockfree`.
        // They are deliberately NOT summed into one counter: this one is
        // ungated and per-drain, that one is gated and per-store, and the sum
        // has no denominator. There is no `buffered_card_marks` denominator
        // for this half and there should not be one until something buffers
        // again — a counter recording the length of a queue nothing fills is
        // the inert instrumentation this round exists to remove.
        let pending_len = pending.len() as u64;
        let mut cells = self.cells.lock();
        let mut newly_dirtied = 0usize;
        for offset in pending {
            let addr = self.base_addr.wrapping_add(offset);
            if addr < self.base_addr || addr >= self.base_addr + self.region_size {
                continue;
            }
            let index = (addr - self.base_addr) / CARD_SIZE;
            self.cover_card_index(index);
            if index < cells.cards.len()
                && cells.cards[index]
                    .compare_exchange(CARD_CLEAN, CARD_DIRTY, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
            {
                cells.dirty_cards.push(index);
                newly_dirtied += 1;
            }
        }
        crate::gc_metrics::record_duplicate_card_marks(
            pending_len.saturating_sub(newly_dirtied as u64),
        );
        newly_dirtied
    }

    /// Bytes of remembered-set metadata this table currently retains.
    ///
    /// Three contributions, all of them real memory the card table costs the
    /// process for as long as it is alive:
    ///
    /// * the card byte-map itself — one `AtomicU8` per [`CARD_SIZE`] bytes of
    ///   covered region, i.e. a fixed 1/512 of the old generation;
    /// * the `dirty_cards` tracking list — one `usize` per card currently
    ///   dirty, which is what makes `take_dirty_cards` O(dirty) instead of
    ///   O(total cards);
    /// * the undrained `pending_offsets` queue — one `usize` per buffered
    ///   old→young edge not yet folded into the bitmap.
    ///
    /// Excludes the per-thread [`DirtyPartitions`] buffers, which are owned by
    /// the threads rather than by this table. Takes both locks, so this is a
    /// diagnostic call (`gc_metrics`), not something to put on an allocation
    /// path.
    pub fn retained_bytes(&self) -> usize {
        let cells = self.cells.lock();
        let map = std::mem::size_of_val(&cells.cards[..]);
        let tracking = cells.dirty_cards.capacity() * std::mem::size_of::<usize>();
        drop(cells);
        let pending = self.pending_offsets.lock().capacity() * std::mem::size_of::<usize>();
        map + tracking + pending
    }

    /// T5.5.2 — How many offsets are currently queued in the shared
    /// pending list waiting to be drained.
    pub fn pending_count(&self) -> usize {
        self.pending_offsets.lock().len()
    }

    /// T5.5.2 — Convenience wrapper: convert a raw address to an
    /// offset and route it through the thread-local fast path.
    ///
    /// Out-of-range addresses are silently dropped, matching the
    /// behaviour of [`Self::mark_dirty`].
    #[inline]
    pub fn thread_local_dirty_addr(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        self.thread_local_dirty(addr - self.base_addr);
    }
}

impl Drop for CardTable {
    /// Retire this table's id from every registered per-thread buffer.
    ///
    /// # Why a dropped table has to clean up after itself
    ///
    /// [`DirtyBufferGuard::drop`] deliberately LEAVES an exiting thread's entry
    /// in [`BUFFER_REGISTRY`] while any bucket still holds offsets, so the heap
    /// edges those offsets record survive until a collector drains them
    /// (the DoHead freed-while-live fix). The reaper is [`Self::flush_all`],
    /// and it only ever takes `self.id`'s bucket — so a bucket belonging to a
    /// table that has since been DROPPED is taken by nobody, `buckets` never
    /// becomes empty, and that registry entry is immortal. Every later
    /// `flush_all` on every other table then walks it, and the registry grows
    /// by one entry per exited thread that ever dirtied a now-dead table. Two
    /// `GenerationalHeap`s per test times a few hundred tests is enough to make
    /// that measurable; a long-lived process that creates and drops heaps makes
    /// it unbounded.
    ///
    /// Dropping the bucket here is safe precisely because the table is gone: an
    /// offset is meaningful only relative to THIS table's `base_addr`, no
    /// collector for it can ever run again, and nothing else may interpret it.
    ///
    /// Lock order is registry -> buffer, matching [`Self::flush_all`] and the
    /// mutator fast path, so this can never deadlock against either.
    fn drop(&mut self) {
        let mut reg = BUFFER_REGISTRY.lock();
        let mut i = 0;
        while i < reg.len() {
            let orphan_empty = {
                let mut b = reg[i].lock();
                // Discard rather than fold: there is no `pending_offsets` left
                // to fold into.
                drop(b.take(self.id));
                // `strong_count == 1` means the registry is the only owner —
                // an owning thread's TLS guard would make it 2 — so this is an
                // entry whose thread has exited and which now holds nothing.
                // (`flush_all` compares against 2 because it holds a local
                // clone as well.)
                Arc::strong_count(&reg[i]) == 1 && b.buckets.is_empty()
            };
            if orphan_empty {
                reg.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

impl std::fmt::Debug for CardTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cells = self.cells.lock();
        let dirty_count = cells
            .cards
            .iter()
            .filter(|card| card.load(Ordering::Acquire) == CARD_DIRTY)
            .count();
        f.debug_struct("CardTable")
            .field("num_cards", &cells.cards.len())
            .field("dirty_cards", &dirty_count)
            .field("base_addr", &format_args!("{:#x}", self.base_addr))
            .field("region_size", &self.region_size)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that touches the PROCESS-GLOBAL `gc_metrics`
    /// counters or their hot-path gate.
    ///
    /// gengc-round2. `reset_metrics_for_test` and
    /// `set_hot_path_counters_enabled` are both process-wide, the harness runs
    /// tests on parallel threads, and there are now two tests that assert an
    /// exact `duplicate_card_marks` value. Without this, adding the second one
    /// would silently turn the first into a flake — the same trap
    /// `gen_evac`'s `CAS_LOSS_ARM` was introduced for.
    static METRICS_ARM: Mutex<()> = Mutex::new(());

    /// Brute-force reference implementation of the dirty set: one
    /// [`CardTable::is_dirty`] per card, which is the per-card scan the
    /// grouped one replaced.
    fn dirty_by_brute_force(ct: &CardTable) -> Vec<usize> {
        (0..ct.num_cards()).filter(|&i| ct.is_dirty(i)).collect()
    }

    /// gengc-round2: the group probe must nominate exactly the cards the old
    /// per-card scan would have found — including at every group boundary,
    /// which is where an off-by-one in the `base + j` reconstruction would
    /// hide.
    ///
    /// This is the equivalence the perf change rests on: `take_dirty_cards`
    /// and `dirty_card_indices` now examine a card individually only if an
    /// OR-fold over its group of [`SCAN_GROUP`] said something in there was
    /// non-clean, so "same card set" is a claim about the grouping arithmetic.
    #[test]
    fn the_group_probe_finds_exactly_what_a_per_card_scan_finds() {
        // Deliberately NOT a multiple of SCAN_GROUP, so the last group is
        // short and the `chunks` remainder is exercised.
        let num = SCAN_GROUP * 5 + 3;
        let ct = CardTable::new(0x10_0000, CARD_SIZE * num);
        assert_eq!(ct.num_cards(), num);

        // First of a group, last of a group, both sides of a boundary, a lone
        // card in an otherwise clean group, and the very last (short-group)
        // card.
        let expected: Vec<usize> = vec![0, SCAN_GROUP - 1, SCAN_GROUP, SCAN_GROUP * 3 + 4, num - 1];
        for &i in &expected {
            ct.mark_dirty_lockfree(0x10_0000 + i * CARD_SIZE);
        }
        assert_eq!(dirty_by_brute_force(&ct), expected);
        assert_eq!(ct.dirty_card_indices(), expected);
        assert_eq!(ct.take_dirty_cards(), expected);
        // Consumed, so the bitmap-to-list invariant still holds.
        assert!(dirty_by_brute_force(&ct).is_empty());

        // And `clear_all` must still reach every byte through the same probe.
        for &i in &expected {
            ct.mark_dirty(0x10_0000 + i * CARD_SIZE);
        }
        assert_eq!(dirty_by_brute_force(&ct), expected);
        ct.clear_all();
        assert!(dirty_by_brute_force(&ct).is_empty());
        assert!(ct.take_dirty_cards().is_empty());
    }

    /// gengc-round2: an unwired table must behave exactly as it did before the
    /// scan bound existed — every card scanned, nothing skipped.
    #[test]
    fn an_unwired_table_scans_the_whole_map() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 40);
        assert_eq!(ct.scan_bound_cards(), ct.num_cards());
        ct.mark_dirty_lockfree(0x1000 + 39 * CARD_SIZE);
        assert_eq!(ct.take_dirty_cards(), vec![39]);
        assert_eq!(
            ct.cards_skipped_by_scan_bound(),
            0,
            "no bound is published, so no card was skipped",
        );
    }

    /// The bound is monotone upward: the first publish replaces the
    /// "unbounded" sentinel, a lower one after that is ignored, a higher one
    /// raises it. A race between two publishers can therefore only ever make
    /// the scan cover MORE cards.
    #[test]
    fn the_scan_bound_only_ever_moves_up() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 64);
        assert_eq!(ct.scan_bound_cards(), 64);

        ct.set_scan_bound_bytes(CARD_SIZE * 8);
        assert_eq!(ct.scan_bound_cards(), 8, "the first publish replaces MAX");

        ct.set_scan_bound_bytes(CARD_SIZE * 4);
        assert_eq!(ct.scan_bound_cards(), 8, "a lower publish is ignored");

        ct.set_scan_bound_bytes(CARD_SIZE * 20 + 1);
        assert_eq!(ct.scan_bound_cards(), 21, "a partial card still counts");

        ct.set_scan_bound_bytes(usize::MAX);
        assert_eq!(ct.scan_bound_cards(), 64, "and it clamps to the map");
    }

    /// A published bound stops the scans at it, and the skip counter says how
    /// much that bought — so "the bound is on and buying nothing" is
    /// distinguishable from "the bound is off".
    #[test]
    fn a_published_bound_stops_the_scan_and_says_what_it_skipped() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 1024);
        ct.set_scan_bound_bytes(CARD_SIZE * 16);
        assert_eq!(ct.scan_bound_cards(), 16);

        ct.mark_dirty_lockfree(0x1000 + 3 * CARD_SIZE);
        assert_eq!(ct.take_dirty_cards(), vec![3], "below the bound is found");
        assert_eq!(
            ct.cards_skipped_by_scan_bound(),
            (1024 - 16) as u64,
            "one pass skipped everything above the bound",
        );
    }

    /// Every mark path the COLLECTOR and the Rust barrier use raises a stale
    /// bound, so a card dirtied above one cannot be lost.
    ///
    /// This is the belt-and-braces half of the safety argument in
    /// `CardTable::scan_bound`'s doc: promotion during a pause moves the old
    /// generation's high-water mark, and the deferred-card re-mark that
    /// follows it (`mark_dirty_bulk`) lands above whatever bound was published
    /// before the pause.
    #[test]
    fn every_rust_mark_path_raises_a_stale_scan_bound() {
        let base = 0x1000usize;

        // mark_dirty_lockfree
        let ct = CardTable::new(base, CARD_SIZE * 256);
        ct.set_scan_bound_bytes(CARD_SIZE * 4);
        ct.mark_dirty_lockfree(base + 100 * CARD_SIZE);
        assert!(ct.scan_bound_cards() > 100);
        assert_eq!(ct.take_dirty_cards(), vec![100]);

        // mark_dirty
        let ct = CardTable::new(base, CARD_SIZE * 256);
        ct.set_scan_bound_bytes(CARD_SIZE * 4);
        ct.mark_dirty(base + 100 * CARD_SIZE);
        assert_eq!(ct.take_dirty_cards(), vec![100]);

        // mark_dirty_bulk — the deferred-card re-mark after promotion.
        let ct = CardTable::new(base, CARD_SIZE * 256);
        ct.set_scan_bound_bytes(CARD_SIZE * 4);
        ct.mark_dirty_bulk(&[base + 100 * CARD_SIZE, base + 200 * CARD_SIZE]);
        assert_eq!(ct.take_dirty_cards(), vec![100, 200]);

        // drain_pending — the buffered path, for as long as it exists.
        let ct = CardTable::new(base, CARD_SIZE * 256);
        ct.set_scan_bound_bytes(CARD_SIZE * 4);
        ct.thread_local_dirty(100 * CARD_SIZE);
        ct.flush_dirty_buffer();
        assert_eq!(ct.drain_pending(), 1);
        assert_eq!(ct.take_dirty_cards(), vec![100]);
    }

    /// The hazard that keeps the scan bound unpublished, pinned as a test so
    /// nobody wires it up by accident.
    ///
    /// A store made ONLY through `jit_cards_addr` — which is what generated
    /// code does, and what `Compiler::inline_card_mark_available()` would
    /// re-enable — bypasses every Rust mark path, so it cannot raise a stale
    /// bound, and a bound below it makes the card invisible to
    /// `take_dirty_cards`. That is a lost old→young root, i.e. a live young
    /// object freed.
    ///
    /// Nothing in the VM publishes a bound today, so this is latent. It is
    /// EXACTLY the reason the card-scan gap page says a bound must land with
    /// the emitter decision rather than before it. If this test ever starts
    /// failing because a raw store IS found, the mechanism has changed and the
    /// gap page's argument needs re-deriving, not deleting.
    #[test]
    fn a_raw_store_above_a_published_bound_is_why_nothing_publishes_one() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 64);
        let cards = ct.jit_cards_addr() as *const AtomicU8;

        // With no bound, the raw store is found — the property
        // `direct_jit_atomic_mark_is_visible_to_stw_consumer` pins.
        unsafe { &*cards.add(40) }.store(CARD_DIRTY, Ordering::Release);
        assert_eq!(ct.take_dirty_cards(), vec![40]);

        // With a bound below it, it is not.
        ct.set_scan_bound_bytes(CARD_SIZE * 8);
        unsafe { &*cards.add(40) }.store(CARD_DIRTY, Ordering::Release);
        assert!(
            ct.take_dirty_cards().is_empty(),
            "a bound a raw store can exceed loses the card — this is the \
             landmine, not a feature",
        );
    }

    /// gengc-round2: the barrier's duplicate marks must measure the barrier
    /// that is actually running.
    ///
    /// The counter used to be fed only by `drain_pending`, which has drained
    /// nothing since gc-genpause F5.2 moved the barrier to
    /// `mark_dirty_lockfree` — so the number `duplicate_mark_ratio` is built on
    /// was structurally zero and read as "the barrier never duplicates". The
    /// round-1 probe run recorded exactly that: `duplicate_marks=0` beside
    /// `dirty_scanned=1980`.
    ///
    /// gengc-round3: it must also stay **divisible by its denominator**. Each
    /// `mark_dirty_lockfree` below is followed by a `record_card_mark`, which
    /// is exactly what `GenerationalHeap::write_barrier` does on both arms of
    /// this branch — so this test drives the production pairing, and asserts
    /// `duplicate_card_marks_barrier <= card_marks_executed`. That inequality
    /// holds by construction and is the cheapest guard against the numerator
    /// and the denominator being fed from different places later, which is the
    /// defect this whole gap page is about.
    #[test]
    fn the_lockfree_barrier_attributes_its_own_duplicate_marks() {
        let _arm = METRICS_ARM.lock();
        crate::gc_metrics::reset_metrics_for_test();
        let restore = crate::gc_metrics::hot_path_counters_enabled();
        crate::gc_metrics::set_hot_path_counters_enabled(true);

        let ct = CardTable::new(0x1000, CARD_SIZE * 8);
        // Three distinct cards, then five more stores that all land on cards
        // already dirty: 3 clean→dirty transitions, 5 duplicates.
        for off in [
            0usize,
            CARD_SIZE,
            2 * CARD_SIZE,
            8,
            16,
            CARD_SIZE + 8,
            24,
            32,
        ] {
            ct.mark_dirty_lockfree(0x1000 + off);
            // What `write_barrier` does next, unconditionally, on both arms.
            crate::gc_metrics::record_card_mark();
        }
        assert_eq!(ct.take_dirty_cards(), vec![0, 1, 2]);
        let raw = crate::gc_metrics::gc_metrics_raw();
        assert_eq!(
            raw.duplicate_card_marks_barrier, 5,
            "five stores hit a card that was already dirty",
        );
        assert_eq!(
            raw.duplicate_card_marks_buffered, 0,
            "nothing was buffered; the barrier half must not leak into the \
             buffered half",
        );
        assert_eq!(
            raw.duplicate_card_marks, 5,
            "the legacy total is the sum of the two halves, so its value is \
             unchanged by the split",
        );
        assert_eq!(raw.card_marks_executed, 8, "eight barrier marks in total");
        assert!(
            raw.duplicate_card_marks_barrier <= raw.card_marks_executed,
            "the duplicate numerator must never exceed its denominator: \
             {} > {}",
            raw.duplicate_card_marks_barrier,
            raw.card_marks_executed,
        );
        assert_eq!(
            crate::gc_metrics::gc_metrics_report().barrier_duplicate_mark_ratio,
            5.0 / 8.0,
            "and the published ratio is that division, nothing else",
        );

        // Gated off, the barrier counts nothing — which is what keeps a
        // process-global `fetch_add` off the hottest path in the VM. The
        // numerator and the denominator disarm TOGETHER, because the gate is
        // inside `record_duplicate_card_mark_barrier` rather than at this
        // call site.
        crate::gc_metrics::set_hot_path_counters_enabled(false);
        for off in [0usize, 8, 16] {
            ct.mark_dirty_lockfree(0x1000 + off);
            crate::gc_metrics::record_card_mark();
        }
        let raw = crate::gc_metrics::gc_metrics_raw();
        assert_eq!(
            raw.duplicate_card_marks_barrier, 5,
            "the duplicate counter is gated exactly like `record_card_mark`",
        );
        assert_eq!(
            raw.card_marks_executed, 8,
            "and so is its denominator — a ratio whose two terms are armed by \
             different flags is not a ratio",
        );
        crate::gc_metrics::set_hot_path_counters_enabled(restore);
    }

    /// gc-genpause F5.2: the lock-free barrier and the buffered pipeline it
    /// replaces must dirty exactly the same cards.
    ///
    /// This is the equivalence the change rests on. The two paths reach the
    /// byte map by completely different routes -- one stores into it directly,
    /// the other queues a byte offset through a per-thread buffer for
    /// `drain_pending` to fold in at a safepoint -- so "same card set" is a
    /// claim about the address arithmetic in both, not something the types
    /// enforce.
    #[test]
    fn the_lockfree_barrier_dirties_the_same_cards_as_the_buffered_path() {
        const BASE: usize = 0x10_0000;
        const SIZE: usize = CARD_SIZE * 64;
        // Addresses spanning card boundaries, exact starts, interiors, the
        // last byte of the region, and two that share a card.
        let addrs = [
            BASE,
            BASE + 1,
            BASE + CARD_SIZE - 1,
            BASE + CARD_SIZE,
            BASE + CARD_SIZE * 7 + 13,
            BASE + CARD_SIZE * 7 + 14,
            BASE + CARD_SIZE * 63,
            BASE + SIZE - 1,
            // Out of range in both directions: neither path may record these.
            BASE - 1,
            BASE + SIZE,
            BASE + SIZE + CARD_SIZE * 4,
        ];

        let buffered = CardTable::new(BASE, SIZE);
        for &a in &addrs {
            buffered.thread_local_dirty_addr(a);
        }
        buffered.flush_all();
        buffered.drain_pending();
        let via_buffer = buffered.take_dirty_cards();

        let direct = CardTable::new(BASE, SIZE);
        for &a in &addrs {
            direct.mark_dirty_lockfree(a);
        }
        let via_direct = direct.take_dirty_cards();

        assert_eq!(
            via_direct, via_buffer,
            "the lock-free barrier must dirty exactly the cards the buffered              path did"
        );
        assert!(!via_direct.is_empty(), "the fixture must dirty something");
        // And specifically: the two addresses sharing card 7 produce ONE card.
        assert_eq!(
            via_direct.iter().filter(|&&c| c == 7).count(),
            1,
            "two stores into one card are one dirty card"
        );
    }

    /// The conditional store in `mark_dirty_lockfree` must not turn a repeat
    /// mark into a no-op that loses the card -- an already-dirty card stays
    /// dirty, and is still reported once.
    #[test]
    fn repeated_lockfree_marks_of_one_card_stay_dirty() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 4);
        for _ in 0..1000 {
            ct.mark_dirty_lockfree(0x1000 + CARD_SIZE * 2 + 8);
        }
        assert!(ct.is_dirty(2));
        assert_eq!(ct.take_dirty_cards(), vec![2]);
        // Consumed: the byte is clean again, so a re-mark genuinely
        // re-registers it (the bt18 bitmap-to-list invariant).
        assert!(!ct.is_dirty(2));
        ct.mark_dirty_lockfree(0x1000 + CARD_SIZE * 2 + 8);
        assert_eq!(ct.take_dirty_cards(), vec![2]);
    }

    /// A dropped `CardTable` must not leave its bucket behind in the
    /// per-thread buffers.
    ///
    /// The bucket is only ever removed by `flush_all`/`flush_dirty_buffer` on
    /// the OWNING table, so a bucket whose table is gone is unreachable
    /// garbage that also keeps `DirtyBufferGuard::drop` from ever deregistering
    /// an exiting thread's entry (it refuses while any bucket is non-empty),
    /// making `BUFFER_REGISTRY` grow without bound. `Drop for CardTable` is
    /// what retires it.
    ///
    /// Deliberately asserts on THIS THREAD'S OWN buffer rather than on
    /// `BUFFER_REGISTRY`'s length: the registry is process-global and every
    /// other test thread in this binary registers into it, so a length
    /// assertion would be exactly the cross-test coupling
    /// `scripts/gc-flake-gate.sh` exists to catch.
    #[test]
    fn dropping_a_table_retires_its_per_thread_bucket() {
        fn buffer_has(table_id: u64) -> bool {
            THREAD_DIRTY_BUFFER.with(|guard| {
                guard
                    .buffer
                    .lock()
                    .buckets
                    .iter()
                    .any(|(id, _)| *id == table_id)
            })
        }

        const KEEP_BASE: usize = 0x30_0000;
        const DOOMED_BASE: usize = 0x40_0000;
        let keep = CardTable::new(KEEP_BASE, CARD_SIZE * 4);
        keep.thread_local_dirty_addr(KEEP_BASE + 8);

        let doomed_id = {
            let doomed = CardTable::new(DOOMED_BASE, CARD_SIZE * 4);
            doomed.thread_local_dirty_addr(DOOMED_BASE + 8);
            let id = doomed.id;
            assert!(buffer_has(id), "the fixture must buffer an offset");
            id
        };

        assert!(
            !buffer_has(doomed_id),
            "a dropped table's bucket must not outlive it — nothing else can \
             ever take it, and while it is there the owning thread can never \
             deregister from BUFFER_REGISTRY",
        );
        // The LIVE table's bucket is untouched, and still drains.
        assert!(
            buffer_has(keep.id),
            "dropping one table must not consume another's buffered offsets",
        );
        keep.flush_all();
        keep.drain_pending();
        assert_eq!(keep.take_dirty_cards(), vec![0]);
    }

    /// gc-genpause F3: `clear_all` writes only the dirty bytes now, so prove
    /// it still leaves every card clean -- including one dirtied by the
    /// lock-free barrier, which never touches the tracking list.
    #[test]
    fn conditional_clear_all_still_clears_every_card() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 8);
        ct.mark_dirty_lockfree(0x1000 + CARD_SIZE * 3);
        ct.mark_dirty(0x1000 + CARD_SIZE * 5);
        assert!(ct.is_dirty(3) && ct.is_dirty(5));
        ct.clear_all();
        for i in 0..ct.num_cards() {
            assert!(!ct.is_dirty(i), "card {i} survived clear_all");
        }
        assert!(ct.take_dirty_cards().is_empty());
    }

    #[test]
    fn direct_jit_atomic_mark_is_visible_to_stw_consumer() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 4);
        let cards = ct.jit_cards_addr() as *const AtomicU8;
        // Model the generated x64 release byte-store for card 2.
        unsafe { &*cards.add(2) }.store(CARD_DIRTY, Ordering::Release);
        assert_eq!(ct.take_dirty_cards(), vec![2]);
        assert!(!ct.is_dirty(2));
    }

    /// `drain_pending` is the only place in the system that can tell a
    /// *buffered* card mark that dirtied a clean card from one that hit an
    /// already-dirty card, so it is where the buffered duplicate-mark counter
    /// is attributed. The arithmetic must be exact — `duplicates = offsets
    /// consumed - clean→dirty transitions`.
    ///
    /// gengc-round3: it is attributed to `duplicate_card_marks_buffered`, and
    /// this test now asserts that the barrier half stays at zero. The two are
    /// on different clocks and different gates; the argument for or against a
    /// per-thread last-card filter in the barrier is
    /// `barrier_duplicate_mark_ratio`, which this path must not touch.
    #[test]
    fn drain_pending_attributes_duplicate_card_marks() {
        // gengc-round2: `duplicate_card_marks` now has a second writer
        // (`mark_dirty_lockfree`, gated), and both this test and
        // `the_lockfree_barrier_attributes_its_own_duplicate_marks` assert an
        // exact value of a PROCESS-GLOBAL counter. Serialise them.
        let _arm = METRICS_ARM.lock();
        crate::gc_metrics::reset_metrics_for_test();
        let ct = CardTable::new(0x1000, CARD_SIZE * 4);

        // Six buffered marks landing on three distinct cards: 3 clean→dirty
        // transitions and 3 duplicates.
        //
        // gengc-round4: the cards ALTERNATE (0,1,0,1,2,0) rather than running
        // (0,0,1,1,2,0). `DirtyPartitions::push` now drops a store whose card
        // equals the bucket's last entry's, so the running order would buffer
        // four entries and one duplicate — a different measurement. The
        // alternating order defeats the last-entry filter exactly, which keeps
        // this test about the DRAIN's attribution, where it belongs. The
        // filter has its own test:
        // `a_run_of_stores_into_one_card_collapses_to_a_single_buffered_entry`.
        for offset in [0usize, CARD_SIZE, 4, CARD_SIZE + 8, 2 * CARD_SIZE, 8] {
            ct.thread_local_dirty(offset);
        }
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 6);

        assert_eq!(ct.drain_pending(), 3, "three distinct cards became dirty");
        let raw = crate::gc_metrics::gc_metrics_raw();
        assert_eq!(
            raw.duplicate_card_marks_buffered, 3,
            "the other three offsets hit a card that was already dirty",
        );
        assert_eq!(
            raw.duplicate_card_marks_barrier, 0,
            "gengc-round3: the drain feeds the BUFFERED half only. If this \
             ever moves, the per-store barrier counter is being fed from an \
             ungated per-drain site and `barrier_duplicate_mark_ratio` has \
             stopped being a fraction of `card_marks_executed`.",
        );
        assert_eq!(
            raw.duplicate_card_marks, 3,
            "the legacy total still reads exactly as it did before the split",
        );

        // A second drain with nothing pending must not invent duplicates.
        assert_eq!(ct.drain_pending(), 0);
        assert_eq!(
            crate::gc_metrics::gc_metrics_raw().duplicate_card_marks,
            3,
            "an empty drain is not a duplicate mark",
        );

        // Re-dirtying the SAME cards after they have been consumed is three
        // genuine clean→dirty transitions again, not three duplicates.
        assert_eq!(ct.take_dirty_cards().len(), 3);
        for offset in [0usize, CARD_SIZE, 2 * CARD_SIZE] {
            ct.thread_local_dirty(offset);
        }
        ct.flush_dirty_buffer();
        assert_eq!(ct.drain_pending(), 3);
        assert_eq!(crate::gc_metrics::gc_metrics_raw().duplicate_card_marks, 3);
    }

    /// The remembered set's space cost, measured rather than assumed: one byte
    /// per [`CARD_SIZE`] bytes of covered region, plus the dirty-index tracking
    /// list, plus the undrained pending queue.
    #[test]
    fn retained_bytes_accounts_for_the_byte_map_and_the_queues() {
        let region = CARD_SIZE * 64;
        let ct = CardTable::new(0x1000, region);
        let empty = ct.retained_bytes();
        assert_eq!(
            empty,
            region / CARD_SIZE,
            "an untouched table costs exactly the card byte-map (1/512 of the \
             covered region)",
        );

        for i in 0..16 {
            ct.thread_local_dirty(i * CARD_SIZE);
        }
        ct.flush_dirty_buffer();
        assert!(
            ct.retained_bytes() > empty,
            "buffered old→young edges are retained memory too",
        );
        ct.drain_pending();
        assert!(ct.retained_bytes() > empty, "so is the dirty-card index");
    }

    /// `card_start_addr` and `card_end_addr` must bracket a well-formed range
    /// for EVERY index, including ones no caller should ever pass.
    ///
    /// gengc-round1 2026-09-20. `card_end_addr` has always clamped to
    /// `region_end` while `card_start_addr` saturated to `usize::MAX`, so an
    /// out-of-range or overflowing index produced `start > end`. The two
    /// obvious things a caller does with such a pair — iterate `start..end`,
    /// or compute `end - start` — silently skip the card or panic on
    /// underflow respectively. Neither failure names the card table.
    #[test]
    fn a_card_address_pair_is_never_inverted_at_any_index() {
        // A region that is NOT a whole number of cards, so the last card is
        // partial and the clamp is load-bearing for an in-range index too.
        let region = CARD_SIZE * 4 + 16;
        let ct = CardTable::new(0x1_0000, region);
        let region_end = 0x1_0000 + region;
        assert_eq!(ct.num_cards(), 5, "the partial tail gets its own card");

        for i in 0..ct.num_cards() {
            let (s, e) = (ct.card_start_addr(i), ct.card_end_addr(i));
            assert!(s < e, "card {i} has an empty or inverted range");
            assert_eq!(s, 0x1_0000 + i * CARD_SIZE);
            assert!(e <= region_end, "card {i} runs past the covered region");
        }
        // The partial last card stops at the region end, not at a card
        // boundary.
        assert_eq!(ct.card_end_addr(4), region_end);

        // Out of range, and far enough out to overflow `index * CARD_SIZE`.
        for i in [ct.num_cards(), usize::MAX / 2, usize::MAX] {
            let (s, e) = (ct.card_start_addr(i), ct.card_end_addr(i));
            assert!(
                s <= e,
                "index {i} produced an inverted range {s:#x}..{e:#x}"
            );
            assert_eq!(e - s, 0, "an out-of-range card must cover nothing");
        }
    }

    #[test]
    fn new_card_table_all_clean() {
        let ct = CardTable::new(0x1000, 4096);
        assert_eq!(ct.num_cards(), 8); // 4096 / 512
        for i in 0..ct.num_cards() {
            assert!(!ct.is_dirty(i));
        }
    }

    #[test]
    fn mark_dirty_and_check() {
        let ct = CardTable::new(0x1000, 4096);
        // Mark card covering address 0x1000 (card 0)
        ct.mark_dirty(0x1000);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));

        // Mark card covering address 0x1400 (offset 0x400 = 1024, card 2)
        ct.mark_dirty(0x1400);
        assert!(ct.is_dirty(2));
    }

    #[test]
    fn mark_dirty_last_byte_of_card() {
        let ct = CardTable::new(0x0, 2048);
        // Last byte of card 0: offset 511
        ct.mark_dirty(511);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));

        // First byte of card 1: offset 512
        ct.mark_dirty(512);
        assert!(ct.is_dirty(1));
    }

    #[test]
    fn clear_all() {
        let ct = CardTable::new(0x0, 2048);
        ct.mark_dirty(0);
        ct.mark_dirty(512);
        ct.mark_dirty(1024);
        assert_eq!(ct.dirty_card_indices().len(), 3);

        ct.clear_all();
        assert_eq!(ct.dirty_card_indices().len(), 0);
        for i in 0..ct.num_cards() {
            assert!(!ct.is_dirty(i));
        }
    }

    #[test]
    fn dirty_card_indices() {
        let ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(0); // card 0
        ct.mark_dirty(1536); // card 3 (1536 / 512 = 3)
        ct.mark_dirty(3584); // card 7 (3584 / 512 = 7)

        let dirty: Vec<usize> = ct.dirty_card_indices();
        assert_eq!(dirty, vec![0, 3, 7]);
    }

    #[test]
    fn card_start_and_end_addr() {
        let ct = CardTable::new(0x1000, 2048);
        assert_eq!(ct.card_start_addr(0), 0x1000);
        assert_eq!(ct.card_end_addr(0), 0x1000 + 512);
        assert_eq!(ct.card_start_addr(1), 0x1000 + 512);
        assert_eq!(ct.card_end_addr(1), 0x1000 + 1024);

        // Last card's end should be clamped to base + region_size
        assert_eq!(ct.card_end_addr(3), 0x1000 + 2048);
    }

    #[test]
    fn non_aligned_region_size() {
        // 1000 bytes → 2 cards (ceil(1000/512) = 2)
        let ct = CardTable::new(0x0, 1000);
        assert_eq!(ct.num_cards(), 2);
        // Last card's end is clamped
        assert_eq!(ct.card_end_addr(1), 1000);
    }

    #[test]
    fn is_dirty_out_of_bounds_returns_false() {
        let ct = CardTable::new(0x0, 1024);
        assert!(!ct.is_dirty(999));
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn mark_dirty_outside_region_is_ignored() {
        let ct = CardTable::new(0x1000, 1024);
        // Address below base
        ct.mark_dirty(0x0);
        // Address above region
        ct.mark_dirty(0x1000 + 1024);
        ct.mark_dirty(0xFFFF);

        // No cards should be dirty
        assert_eq!(ct.dirty_card_indices().len(), 0);
    }

    #[test]
    fn mark_dirty_first_card() {
        let ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(0);
        assert!(ct.is_dirty(0));
        assert!(!ct.is_dirty(1));
    }

    #[test]
    fn mark_dirty_last_card() {
        let ct = CardTable::new(0x0, 4096);
        // Last card covers bytes 3584..4095
        ct.mark_dirty(4095);
        let last_card = ct.num_cards() - 1;
        assert!(ct.is_dirty(last_card));
    }

    #[test]
    fn dirty_multiple_regions() {
        let ct = CardTable::new(0x0, 8192);
        // Dirty cards 0, 3, 7, 15
        ct.mark_dirty(0); // card 0
        ct.mark_dirty(1536); // card 3
        ct.mark_dirty(3584); // card 7
        ct.mark_dirty(7680); // card 15

        let dirty: Vec<usize> = ct.dirty_card_indices();
        assert_eq!(dirty, vec![0, 3, 7, 15]);
    }

    #[test]
    fn clear_then_dirty_again() {
        let ct = CardTable::new(0x0, 2048);
        ct.mark_dirty(0);
        ct.mark_dirty(512);
        assert_eq!(ct.dirty_card_indices().len(), 2);

        ct.clear_all();
        assert_eq!(ct.dirty_card_indices().len(), 0);

        // Re-dirty after clear
        ct.mark_dirty(1024);
        let dirty: Vec<usize> = ct.dirty_card_indices();
        assert_eq!(dirty, vec![2]);
    }

    #[test]
    fn card_addresses_with_nonzero_base() {
        let base = 0x8000usize;
        let ct = CardTable::new(base, 2048);

        assert_eq!(ct.card_start_addr(0), 0x8000);
        assert_eq!(ct.card_end_addr(0), 0x8000 + CARD_SIZE);
        assert_eq!(ct.card_start_addr(1), 0x8000 + CARD_SIZE);
        assert_eq!(ct.card_end_addr(1), 0x8000 + 2 * CARD_SIZE);
    }

    #[test]
    fn mark_same_card_idempotent() {
        let ct = CardTable::new(0x0, 4096);
        ct.mark_dirty(100);
        ct.mark_dirty(200);
        ct.mark_dirty(300);
        // All within card 0 (offsets 0..511)
        assert!(ct.is_dirty(0));
        assert_eq!(ct.dirty_card_indices().len(), 1);
    }

    #[test]
    fn all_cards_dirty() {
        let ct = CardTable::new(0x0, 2048);
        let num = ct.num_cards();
        for i in 0..num {
            ct.mark_dirty(i * CARD_SIZE);
        }
        assert_eq!(ct.dirty_card_indices().len(), num);
    }

    #[test]
    fn single_byte_region() {
        let ct = CardTable::new(0x0, 1);
        assert_eq!(ct.num_cards(), 1);
        assert_eq!(ct.card_end_addr(0), 1);
    }

    #[test]
    fn card_size_boundary_region() {
        // Region exactly CARD_SIZE bytes
        let ct = CardTable::new(0x0, CARD_SIZE);
        assert_eq!(ct.num_cards(), 1);
        assert_eq!(ct.card_start_addr(0), 0);
        assert_eq!(ct.card_end_addr(0), CARD_SIZE);
    }

    #[test]
    fn card_size_plus_one_region() {
        let ct = CardTable::new(0x0, CARD_SIZE + 1);
        assert_eq!(ct.num_cards(), 2);
        assert_eq!(ct.card_end_addr(1), CARD_SIZE + 1);
    }

    #[test]
    fn mark_dirty_at_exact_base() {
        let ct = CardTable::new(0x5000, 4096);
        ct.mark_dirty(0x5000);
        assert!(ct.is_dirty(0));
    }

    #[test]
    fn mark_dirty_at_region_end_minus_one() {
        let ct = CardTable::new(0x5000, 4096);
        ct.mark_dirty(0x5000 + 4095);
        let last = ct.num_cards() - 1;
        assert!(ct.is_dirty(last));
    }

    #[test]
    fn mark_dirty_at_exact_region_end_is_ignored() {
        let ct = CardTable::new(0x5000, 4096);
        // Exactly at base + region_size is out of range
        ct.mark_dirty(0x5000 + 4096);
        assert_eq!(ct.dirty_card_indices().len(), 0);
    }

    // -----------------------------------------------------------------
    // T5.5.2 — Thread-local batched dirty tests
    // -----------------------------------------------------------------

    #[test]
    fn thread_local_dirty_accumulates_without_touching_cards() {
        let ct = CardTable::new(0x0, 8192);
        // Dirty one offset via the fast path — stays in the thread-local
        // buffer; the shared table sees nothing until a flush.
        ct.thread_local_dirty(0);
        // Not enough to auto-flush.
        assert_eq!(ct.pending_count(), 0);
        assert_eq!(ct.dirty_card_indices().len(), 0);

        // Explicit flush + drain.
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 1);
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(0));
    }

    #[test]
    fn thread_local_dirty_auto_flushes_at_threshold() {
        // Use a large region so the threshold-worth of distinct card
        // offsets is well-defined.
        let ct = CardTable::new(0x0, CARD_SIZE * THREAD_BUFFER_FLUSH_THRESHOLD * 2);
        // Push exactly THREAD_BUFFER_FLUSH_THRESHOLD offsets — the last
        // one should auto-flush the buffer.
        for i in 0..THREAD_BUFFER_FLUSH_THRESHOLD {
            ct.thread_local_dirty(i * CARD_SIZE);
        }
        assert_eq!(ct.pending_count(), THREAD_BUFFER_FLUSH_THRESHOLD);

        let newly = ct.drain_pending();
        assert_eq!(newly, THREAD_BUFFER_FLUSH_THRESHOLD);
        for i in 0..THREAD_BUFFER_FLUSH_THRESHOLD {
            assert!(ct.is_dirty(i));
        }
    }

    #[test]
    fn flush_dirty_buffer_is_idempotent_when_empty() {
        let ct = CardTable::new(0x0, 4096);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 0);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 0);
    }

    /// gengc-round4: the per-thread buffer now drops a store whose card equals
    /// the bucket's LAST entry's, so a run of stores into one card collapses
    /// before it ever reaches `pending_offsets`. The offsets here therefore
    /// ALTERNATE cards, which is exactly the pattern the last-entry filter
    /// cannot collapse — leaving `drain_pending`'s CAS as the authority on
    /// what is a duplicate, which is what this test is about.
    #[test]
    fn drain_pending_deduplicates_same_card() {
        let ct = CardTable::new(0x0, 4096);
        // Cards 0, 1, 0 — three buffered entries, two distinct cards.
        ct.thread_local_dirty(0);
        ct.thread_local_dirty(CARD_SIZE + 88);
        ct.thread_local_dirty(100);
        ct.flush_dirty_buffer();
        assert_eq!(
            ct.pending_count(),
            3,
            "the last-entry filter must not collapse ALTERNATING cards — only runs of the same one",
        );
        let newly = ct.drain_pending();
        // Three offsets but only two new cards: the third names card 0 again.
        assert_eq!(newly, 2);
        assert!(ct.is_dirty(0) && ct.is_dirty(1));
        assert_eq!(ct.dirty_card_indices().len(), 2);
    }

    /// gengc-round4 — hazard (2) of
    /// `gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`: a tight
    /// loop writing ONE hot reference field used to queue one `usize` per
    /// iteration, all of them naming the same card, through the 64-entry
    /// auto-flush and into the unbounded shared `pending_offsets`. The buffer is now filtered against
    /// its own last entry, so the whole run is one entry.
    #[test]
    fn a_run_of_stores_into_one_card_collapses_to_a_single_buffered_entry() {
        let ct = CardTable::new(0x0, CARD_SIZE * 4);
        // Ten thousand stores into one card — comfortably past
        // THREAD_BUFFER_FLUSH_THRESHOLD, so pre-fix this would have
        // auto-flushed ~156 times and left 10_000 entries pending.
        for i in 0..10_000usize {
            ct.thread_local_dirty(i % CARD_SIZE);
        }
        ct.flush_dirty_buffer();
        assert_eq!(
            ct.pending_count(),
            1,
            "a run of same-card stores must buffer exactly one offset",
        );
        assert_eq!(ct.drain_pending(), 1);
        assert!(ct.is_dirty(0));
        assert_eq!(ct.dirty_card_indices(), vec![0]);
    }

    #[test]
    fn drain_pending_rejects_out_of_range_offset() {
        let ct = CardTable::new(0x0, 1024);
        // Offset past region_size — should be silently dropped.
        ct.thread_local_dirty(1024);
        ct.thread_local_dirty(99999);
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        assert_eq!(newly, 0);
    }

    #[test]
    fn thread_local_dirty_addr_wraps_address_to_offset() {
        let ct = CardTable::new(0x10_0000, 4096);
        ct.thread_local_dirty_addr(0x10_0000 + 513); // card 1
        ct.flush_dirty_buffer();
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(1));
    }

    /// gengc-round4 — hazard (1) of
    /// `gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`.
    /// `clear_all` used to DROP the pending queue; it now folds it into the
    /// freshly-cleaned bitmap.
    ///
    /// The queue is still empty afterwards — that half of the old test's claim
    /// is unchanged and still asserted — but the offsets in it are now cards,
    /// not litter. Pre-fix this test's `take_dirty_cards` assertion failed,
    /// which is precisely what the page said it would.
    #[test]
    fn clear_all_folds_pending_into_the_clean_bitmap() {
        let ct = CardTable::new(0x0, 4096);
        // Two distinct cards buffered under the auto-flush threshold and
        // moved to the shared queue by a peer-style explicit flush — the
        // exact shape a buffered mutator path would present at a pause whose
        // own flush+drain has already run.
        ct.thread_local_dirty(0);
        ct.thread_local_dirty(CARD_SIZE + 8);
        ct.flush_dirty_buffer();
        assert_eq!(ct.pending_count(), 2);

        ct.clear_all();

        assert_eq!(
            ct.pending_count(),
            0,
            "the queue is consumed, not left to be re-applied next cycle",
        );
        assert_eq!(
            ct.take_dirty_cards(),
            vec![0, 1],
            "a pending offset names a REAL old→young store: dropping it loses the edge for good and the next young collection frees the referent. Re-dirtying can only over-retain.",
        );
    }

    /// The fold must not resurrect a card the clear was there to retire: only
    /// offsets that are genuinely PENDING come back.
    #[test]
    fn clear_all_folds_only_the_pending_queue_not_the_cards_it_cleared() {
        let ct = CardTable::new(0x0, CARD_SIZE * 8);
        // Card 5 is dirty in the bitmap and nowhere else.
        ct.mark_dirty(CARD_SIZE * 5);
        // Card 2 is only in the pending queue.
        ct.thread_local_dirty(CARD_SIZE * 2);
        ct.flush_dirty_buffer();

        ct.clear_all();

        assert_eq!(
            ct.take_dirty_cards(),
            vec![2],
            "card 5 was cleared and had nothing pending; card 2 was pending and comes back",
        );
    }

    #[test]
    fn flush_all_drains_this_threads_buffer() {
        // T5.5.2 — `flush_all` is the GC-entry hook. For a single table on
        // the calling thread it folds this thread's buffered offsets into
        // `pending_offsets`, equivalent to `flush_dirty_buffer`.
        let ct = CardTable::new(0x0, 4096);
        ct.thread_local_dirty(0);
        assert_eq!(ct.pending_count(), 0);
        ct.flush_all();
        assert_eq!(ct.pending_count(), 1);
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(0));
    }

    #[test]
    fn dying_thread_residual_offsets_survive_until_flush_all() {
        // DoHead freed-while-live regression (2026-07-15): a mutator thread
        // that buffers an old→young edge and EXITS without flushing (fewer
        // than THREAD_BUFFER_FLUSH_THRESHOLD entries buffered) must not lose
        // the record — the edge lives in the heap and outlives the thread.
        // Pre-fix, `DirtyBufferGuard::drop` deregistered the buffer and the
        // offsets were silently discarded; the next minor GC freed the
        // still-referenced young object (Tomcat's static FastHttpDateFormat
        // ConcurrentLinkedQueue nodes zeroing under worker-pool churn).
        let ct = std::sync::Arc::new(CardTable::new(0x0, 4096));
        let ct2 = std::sync::Arc::clone(&ct);
        std::thread::spawn(move || {
            // One buffered edge, far below the auto-flush threshold; the
            // thread exits immediately after, running DirtyBufferGuard::drop.
            ct2.thread_local_dirty(128);
        })
        .join()
        .unwrap();
        assert_eq!(
            ct.pending_count(),
            0,
            "edge must still be in the (orphaned) thread buffer, not pending"
        );
        // Collector at STW: must recover the dead thread's buffered edge and
        // reap the orphaned buffer.
        ct.flush_all();
        assert_eq!(ct.pending_count(), 1, "dead thread's edge must survive");
        let newly = ct.drain_pending();
        assert_eq!(newly, 1);
        assert!(ct.is_dirty(128 / CARD_SIZE));
        // Second flush_all: the orphan was reaped, nothing left to drain.
        ct.flush_all();
        assert_eq!(ct.pending_count(), 0);
    }

    // -----------------------------------------------------------------
    // SECURITY FIX (V6) — table-id scoping regression tests
    // -----------------------------------------------------------------

    #[test]
    fn distinct_tables_have_distinct_ids() {
        let a = CardTable::new(0x0, 4096);
        let b = CardTable::new(0x0, 4096);
        assert_ne!(a.id, b.id, "each CardTable must get a unique id");
    }

    #[test]
    fn flush_all_does_not_steal_other_tables_entries() {
        // SECURITY FIX (V6): two tables, interleaved dirties on the SAME
        // thread (so both feed the one per-thread buffer). Draining table A
        // must not consume table B's buffered offsets, and vice-versa.
        //
        // Offsets are chosen so each lands on a distinct card and the two
        // tables' dirty card sets differ, proving no theft / no leakage.
        let a = CardTable::new(0x0, 8192);
        let b = CardTable::new(0x0, 8192);

        // Interleave below the auto-flush threshold so everything stays in
        // the shared per-thread buffer until we explicitly flush.
        a.thread_local_dirty(0); // A: card 0
        b.thread_local_dirty(CARD_SIZE); // B: card 1
        a.thread_local_dirty(2 * CARD_SIZE); // A: card 2
        b.thread_local_dirty(3 * CARD_SIZE); // B: card 3

        // Drain A via flush_all: only A's offsets should land in A's pending.
        a.flush_all();
        assert_eq!(a.pending_count(), 2, "A should own exactly its 2 offsets");
        // B's offsets must still be buffered (untouched by A's flush).
        assert_eq!(
            b.pending_count(),
            0,
            "B's buffered offsets must survive A's flush"
        );

        let a_new = a.drain_pending();
        assert_eq!(a_new, 2);
        assert!(a.is_dirty(0));
        assert!(a.is_dirty(2));
        assert!(!a.is_dirty(1), "A must not have B's card 1");
        assert!(!a.is_dirty(3), "A must not have B's card 3");

        // Now drain B: its offsets were preserved through A's flush.
        b.flush_all();
        assert_eq!(b.pending_count(), 2, "B's offsets recovered intact");
        let b_new = b.drain_pending();
        assert_eq!(b_new, 2);
        assert!(b.is_dirty(1));
        assert!(b.is_dirty(3));
        assert!(!b.is_dirty(0), "B must not have A's card 0");
        assert!(!b.is_dirty(2), "B must not have A's card 2");
    }

    #[test]
    fn flush_dirty_buffer_is_table_scoped() {
        // Same property as above, but exercising the per-thread
        // `flush_dirty_buffer` path rather than the registry-wide `flush_all`.
        let a = CardTable::new(0x0, 8192);
        let b = CardTable::new(0x0, 8192);

        a.thread_local_dirty(0);
        b.thread_local_dirty(CARD_SIZE);

        a.flush_dirty_buffer();
        assert_eq!(a.pending_count(), 1);
        assert_eq!(b.pending_count(), 0, "A's flush must not take B's entry");

        b.flush_dirty_buffer();
        assert_eq!(
            b.pending_count(),
            1,
            "B's entry still available after A flushed"
        );

        assert_eq!(a.drain_pending(), 1);
        assert_eq!(b.drain_pending(), 1);
        assert!(a.is_dirty(0));
        assert!(b.is_dirty(1));
    }

    #[test]
    fn cross_thread_v6_property_preserved_per_table() {
        // SECURITY FIX (V6): a *worker* thread buffers an old→young edge for
        // table A below the auto-flush threshold and then parks WITHOUT
        // flushing. A's `flush_all` (run on the main thread) must still recover
        // that edge from the global registry — the V6 cross-thread property —
        // while leaving table B's edge (buffered on the main thread) intact.
        use std::sync::mpsc;

        let a = std::sync::Arc::new(CardTable::new(0x0, 8192));
        let b = CardTable::new(0x0, 8192);

        // Main thread buffers one edge for B (card 1), unflushed.
        b.thread_local_dirty(CARD_SIZE);

        // Worker buffers one edge for A (card 2), then parks until released so
        // its buffer is still registered while we run `flush_all`.
        let (tx_ready, rx_ready) = mpsc::channel::<()>();
        let (tx_go, rx_go) = mpsc::channel::<()>();
        let a_worker = std::sync::Arc::clone(&a);
        let handle = std::thread::spawn(move || {
            a_worker.thread_local_dirty(2 * CARD_SIZE);
            tx_ready.send(()).unwrap();
            rx_go.recv().unwrap(); // park: never flushes its own buffer
        });

        // Wait until the worker has buffered, then drain A across every
        // registered thread buffer.
        rx_ready.recv().unwrap();
        a.flush_all();
        assert_eq!(
            a.pending_count(),
            1,
            "A's collector must recover the worker thread's unflushed edge"
        );
        assert_eq!(a.drain_pending(), 1);
        assert!(a.is_dirty(2), "worker's old→young edge for A is live");

        // A's cross-thread drain must not have disturbed B's buffered edge.
        assert_eq!(b.pending_count(), 0);
        b.flush_all();
        assert_eq!(
            b.pending_count(),
            1,
            "B's edge survived A's cross-thread drain"
        );
        assert_eq!(b.drain_pending(), 1);
        assert!(b.is_dirty(1));

        // Release the worker and join.
        tx_go.send(()).unwrap();
        handle.join().unwrap();
    }
}
