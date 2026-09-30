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
//! T5.5.2 wiring (HIGH-1 fix): all `&self` methods are callable
//! concurrently. The collector's multi-byte passes are serialised by
//! `Mutex<CardCells>`; the card BYTES are `AtomicU8`s that the mutator
//! barrier ([`CardTable::mark_dirty_lockfree`]) stores into directly,
//! without that mutex, through the cached `cards_addr`. (gcd d1/d: the
//! `dirty_cards` tracking list that mutex also guarded is gone — the byte
//! scan in [`CardTable::take_dirty_cards`] found every entry of it again.)
//!
//! CORRECTION (gengc-round4/cards, 2026-09-23): this paragraph used to say
//! the mutator fast path "only touches the per-thread buffer and the
//! `Mutex<Vec<usize>>` of pending offsets". That was the T5.5.2 design; since
//! gc-genpause F5.2 the mutator path is `mark_dirty_lockfree`, which touches
//! neither — see the STATUS paragraph below.
//!
//! SCAN BOUND (gengc-round4/cards, 2026-09-23): the collector-side scans
//! stop at a SELF-MAINTAINED bound — one past the highest card index any
//! Rust mark path has ever dirtied — instead of walking the whole map. That
//! is sound only while every writer of a card byte is a method of this type,
//! so handing out the raw map address ([`CardTable::jit_cards_addr`])
//! permanently disarms the bound for that table. See the `scan_bound` field
//! doc for the whole argument.
//!
//! SUMMARY MAP (gen r4w3/cards3, 2026-09-23): below the bound, the scans read
//! a second, 64x smaller byte map first — one byte per [`SUMMARY_GROUP`]
//! cards — and descend into the card bytes of a group only when its summary
//! byte is set. Every Rust mark path sets the summary byte before the card
//! byte; the same raw-address escape that disarms the bound disarms the
//! summary. `CRATONVM_GC_CARD_SUMMARY=0` restores the flat scan. See the
//! `summary` field doc.
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
//! See `docs/internal/gc/gengc-oldgen-dead-buffered-card-pipeline-FIXED-20260923.md`.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
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

/// Cards covered by one byte of the SUMMARY map (gen r4w3/cards3).
///
/// One summary byte per 64 cards = one per 32 KiB of old generation, so a
/// 1 GiB old generation's 2 Mi-card byte map is summarised by 32 KiB — small
/// enough to stay cache-resident across a scan. The collector-side scans read
/// the summary and descend into the card bytes of a group only when its
/// summary byte is set; see [`CardTable::summary`]. A multiple of
/// [`SCAN_GROUP`], so the grouped card probe still applies inside a group.
///
/// Not tuned: a power of two that makes the summary 1/64 of the card map.
pub const SUMMARY_GROUP: usize = 64;

/// Construction-time choices for a [`CardTable`] (gen r4w3/cards3).
///
/// Per table, never a process global (AGENTS.md): [`CardTable::new`] derives
/// them from the VM's flags once, and a test builds both arms side by side
/// through [`CardTable::with_options`]. Neither may be changed after
/// construction — both describe what the MARKS in the table mean, and a
/// consumer that disagreed with its producer about that would drop edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardTableOptions {
    /// Maintain the two-level summary map and let the scans skip clean
    /// summary groups. `CRATONVM_GC_CARD_SUMMARY=0` turns it off: the scans
    /// then walk the byte map up to the scan bound exactly as before.
    pub summary: bool,
    /// The generational barrier may dirty the card of the written ELEMENT of
    /// a reference array rather than the array's header card, so the consumer
    /// must look at arrays that merely OVERLAP a dirty card, not only at
    /// objects whose header lies in one. `CRATONVM_GC_PRECISE_ARRAY_CARDS=0`
    /// turns it off (header-card marks only, the pre-2026-09-23 contract).
    pub precise_ref_array_marks: bool,
}

impl CardTableOptions {
    /// Both features off: the pre-gen-r4w3 table, byte for byte. The A arm of
    /// every equivalence test in this file.
    pub const LEGACY: Self = Self {
        summary: false,
        precise_ref_array_marks: false,
    };

    /// What the VM's flags ask for. Both default ON.
    pub fn from_flags() -> Self {
        let f = crate::gc_flags();
        Self {
            summary: f.gc_card_summary,
            precise_ref_array_marks: f.gc_precise_array_cards,
        }
    }
}

/// `JitCardView::magic` — "CARDVIEW" in ASCII. Compiled code never reads it;
/// the JIT checks it at COMPILE time before it trusts any other word of a
/// table it was handed, so a helper table that names something else (a stale
/// address, a raw card map from an older build) declines the inline barrier
/// instead of emitting loads through it.
pub const JIT_CARD_VIEW_MAGIC: usize = 0x5745_4956_4452_4143;

/// gen r4w4/cards4 — the READ-ONLY view of one [`CardTable`] that the x64 JIT's
/// inline generational post barrier consults (`CRATONVM_JIT_INLINE_CARD_MARK`).
///
/// # The protocol this exists for: compiled code never WRITES a card byte
///
/// The inline barrier's fast path is "the card of this store is already dirty,
/// so there is nothing to do". Everything else — the clean→dirty transition —
/// is a call to the collector's own barrier
/// ([`CardTable::mark_dirty_lockfree`] through `GenerationalHeap::write_barrier`),
/// which raises the self-maintained scan bound BEFORE the byte store and sets
/// the summary byte before AND after it. Every card byte is therefore still
/// written only by a method of this type, so handing THIS address out keeps
/// both the scan bound and the summary map armed. [`CardTable::jit_cards_addr`],
/// the raw-map escape, disarms both for good; this does not, because nothing
/// reachable from it stores.
///
/// Reading a card byte outside a pause is harmless: bytes are only CLEARED at
/// stop-the-world, and a stale "clean" answer only costs a redundant call.
/// A stale "dirty" cannot happen for the same reason.
///
/// # Layout
///
/// `repr(C)`, five words. The JIT cannot depend on this crate, so it carries
/// copies of the offsets below, pinned by
/// `jit/src/x64/objects.rs::r4w4_cards4_tests::the_card_view_offsets_match_the_collector_s`.
/// `old_base`, `old_end` and `cards_neg` are loaded by compiled code at RUN
/// time rather than baked, so a future coverage extension (old-gen growth)
/// that re-points them with [`CardTable::refresh_jit_card_view`] at a pause is
/// seen by already-compiled code. `flags` is read at compile time: it decides
/// the SHAPE of the emitted barrier and is fixed at construction.
///
/// `cards_neg` is the card map's address NEGATED (`wrapping_neg`) because the
/// emitter forms `&cards[idx]` as `idx - cards_neg` with the `SUB r64, m64`
/// encoding it already has (and executes in tests) rather than a new `ADD`.
#[repr(C)]
#[derive(Debug)]
pub struct JitCardView {
    /// [`JIT_CARD_VIEW_MAGIC`].
    pub magic: usize,
    /// First byte the table covers (= [`CardTable::base_addr`]).
    pub old_base: AtomicUsize,
    /// One past the last byte the table covers
    /// (= `base_addr + region_size`).
    pub old_end: AtomicUsize,
    /// `cards_addr.wrapping_neg()`: see the type doc.
    pub cards_neg: AtomicUsize,
    /// [`Self::FLAG_PRECISE_REF_ARRAYS`] when the table carries
    /// element-precise reference-array marks.
    pub flags: usize,
}

impl JitCardView {
    /// Byte offset of [`Self::magic`].
    pub const MAGIC_OFFSET: usize = 0;
    /// Byte offset of [`Self::old_base`].
    pub const OLD_BASE_OFFSET: usize = 8;
    /// Byte offset of [`Self::old_end`].
    pub const OLD_END_OFFSET: usize = 16;
    /// Byte offset of [`Self::cards_neg`].
    pub const CARDS_NEG_OFFSET: usize = 24;
    /// Byte offset of [`Self::flags`].
    pub const FLAGS_OFFSET: usize = 32;
    /// [`CardTable::precise_ref_array_marks`]: a compiled `aastore` must check
    /// (and have the barrier dirty) the ELEMENT's card, not the header's.
    pub const FLAG_PRECISE_REF_ARRAYS: usize = 1;
}

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

/// Card-bitmap state. The mutex serialises the collector's multi-byte passes
/// over `cards` and the locked mark paths; the `cards` bytes themselves are
/// ALSO stored into lock-free by [`CardTable::mark_dirty_lockfree`] through
/// the cached `cards_addr`, which is sound because every element is an
/// `AtomicU8` and the vector is never resized.
///
/// gcd d1/d (2026-09-27, `gengc-r5w5-old9-card-table-review-residuals` item
/// 2): the `dirty_cards` tracking list is gone. Every entry was pushed by a
/// locked path right after a successful clean→dirty CAS, and each of those
/// paths raises the scan bound and sets the summary byte BEFORE the CAS; card
/// bytes are cleared only by [`CardTable::take_dirty_cards`] and
/// [`CardTable::clear_all`], which cleared the list with them under this same
/// mutex. So the byte scan found every listed index again, `taken` held each
/// one twice, and the `sort_unstable` + `dedup` existed only to undo that —
/// while `mem::take` threw the list's allocation away every cycle for
/// `mark_dirty_bulk` to regrow after `clear_all`.
///
/// CORRECTION (gengc-round4/cards, 2026-09-23): this used to say the struct
/// was "never touched by the mutator fast path". Every interpreter/native
/// reference store into an old-gen object touches `cards`.
struct CardCells {
    /// One stable atomic byte per card (CARD_CLEAN or CARD_DIRTY).
    ///
    /// The x64 JIT may perform a release byte-store directly into this backing
    /// array after a reference store. The vector is never resized, so
    /// [`CardTable::jit_cards_addr`] remains valid for the table's lifetime.
    cards: Vec<AtomicU8>,
}

/// A byte-map card table for tracking old→young cross-generation references.
///
/// Each byte covers `CARD_SIZE` bytes of the old generation's address space.
/// The write barrier marks cards dirty; the minor GC scans dirty cards and
/// then clears them.
///
/// All public methods take `&self`. The mutator write barrier is
/// [`Self::mark_dirty_lockfree`]: a bounds check, a shift, a relaxed load and
/// a conditional release byte store straight into the map, no lock. The
/// collector's multi-byte passes are serialised by `cells` (gcd d1/d removed
/// the tracking list it also guarded), the (dead, retained) buffered
/// pipeline's queue is behind `pending_offsets`.
///
/// CORRECTION (gengc-round4/cards, 2026-09-23): this doc used to name
/// [`Self::thread_local_dirty_addr`] as the mutator fast path. It has had no
/// production caller since gc-genpause F5.2; see the module header.
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
    /// a bounds check, a shift, a load and a conditional byte store.
    ///
    /// gen r4w4/cards4: and now literally one. The re-enabled inline barrier
    /// (`CRATONVM_JIT_INLINE_CARD_MARK`) only reads the byte map through
    /// [`JitCardView`] and calls into [`Self::mark_dirty_lockfree`] for every
    /// clean→dirty transition, so there is a single card-WRITING rule in the
    /// VM. The WildFly clean-card observation that switched the old emitter
    /// off is analysed in
    /// `docs/internal/reviews/gengc-round4-w4-cards4-20260924.md`.
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
    /// `cards` at the next safepoint.
    pending_offsets: Mutex<Vec<usize>>,
    /// Exclusive upper bound, in cards, on what the collector-side scans have
    /// to look at. Starts at `0` and is raised by every Rust mark path (see
    /// "SELF-MAINTAINED" below); [`SCAN_BOUND_UNSET`] is no longer the initial
    /// value and survives only as a defensive "unbounded" encoding.
    ///
    /// The "contract" and "stale publish" sections below are round 2's
    /// analysis of an EXTERNALLY published bound. They still describe what
    /// [`Self::set_scan_bound_bytes`] promises, but that entry point is now
    /// redundant rather than required: a publish can only raise the
    /// self-maintained bound, never lower it.
    ///
    /// # What this is for
    ///
    /// The map covers the old generation's whole CAPACITY, but a young pause's
    /// cost is supposed to track the young live set. A 128 MiB old generation
    /// is 262,144 cards whether it holds one object or a million, and both
    /// [`Self::take_dirty_cards`] and [`Self::clear_all`] walk all of them on
    /// every moving cycle. See
    /// `docs/internal/gc/gengc-oldgen-card-scan-is-o-heap-FIXED-20260923.md`.
    ///
    /// # The contract, which is the whole of the safety argument
    ///
    /// A publisher promises: **no card at or above this index can be dirty,
    /// now or before the next publish.** A broken promise is a lost old→young
    /// root, i.e. a live young object freed — the worst failure this file has.
    ///
    /// The one bound that can honestly make that promise is the old
    /// generation's high-water mark (`OldGen::high_water`): a card index is
    /// derived from an address INSIDE an old-gen object — the holder's header
    /// for a field store, the element's slot for an element-precise
    /// reference-array store (gen r5w5/old9: this said "from the holder's
    /// header" only, which stopped being true with element-precise cards) —
    /// and every byte of every old-gen object lies below the high-water mark
    /// by construction.
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
    /// # gengc-round4/cards (2026-09-23): the bound is SELF-MAINTAINED
    ///
    /// Round 2 left this unpublished because the only honest external
    /// publisher (`OldGen::high_water`) goes stale across a pause, and the one
    /// writer that cannot raise it — the JIT's raw byte store — would then
    /// lose a card. Both problems disappear if the bound is not published at
    /// all but maintained by the writers themselves:
    ///
    /// * a new table starts at `0` ("no card has ever been dirtied"), not at
    ///   [`SCAN_BOUND_UNSET`];
    /// * every Rust mark path — [`Self::mark_dirty`], [`Self::mark_dirty_bulk`],
    ///   [`Self::drain_pending`] and the barrier [`Self::mark_dirty_lockfree`] —
    ///   calls `cover_card_index` BEFORE it stores `CARD_DIRTY`, so at every
    ///   instant every non-clean byte lies below the bound (a thread frozen
    ///   between the two can only leave the bound one card too high);
    /// * the bound never moves down, so a card dirtied in an earlier cycle and
    ///   still dirty is still covered;
    /// * the ONLY other way to write a card byte is through the raw address
    ///   [`Self::jit_cards_addr`] hands out, and handing it out sets
    ///   `raw_card_address_escaped`, after which [`Self::scan_bound_cards`]
    ///   answers `num_cards` for the rest of the table's life.
    ///
    /// So the scans cover `[0, highest card ever dirtied]` instead of the whole
    /// capacity — a young pause no longer pays for old-gen capacity that has
    /// never held an old→young edge — and the default behaviour is otherwise
    /// byte-for-byte the old one: same card set, same clearing. The residual
    /// O(high-water) term is recorded in
    /// `docs/internal/gc/gengc-oldgen-card-scan-is-o-heap-FIXED-20260923.md`.
    ///
    /// Monotone upward (see [`Self::set_scan_bound_bytes`]), so a race can
    /// only ever over-scan.
    scan_bound: AtomicUsize,
    /// Set, never cleared, the first time [`Self::jit_cards_addr`] hands the
    /// raw map address out. From then on a writer this type cannot see may
    /// store `CARD_DIRTY` anywhere in the map, so the self-maintained
    /// `scan_bound` is no longer a promise and every scan covers the whole map
    /// again (gengc-round4/cards). Per-table, not a process global.
    raw_card_address_escaped: AtomicBool,
    /// One-shot latch for the "buffered pipeline has a producer again"
    /// warning in [`Self::thread_local_dirty`] — item #6's first step in
    /// `docs/feature-designs/gc-round-20260920-generational.md`. Per-table so
    /// it is not a process global (AGENTS.md) and so unit tests that drive the
    /// buffered path on their own tables do not silence a production table.
    buffered_path_warned: AtomicBool,
    /// Cards the bound let the collector-side scans skip, totalled over this
    /// table's life. Zero while `scan_bound` is [`SCAN_BOUND_UNSET`], which is
    /// how a reader tells "the bound is off" from "the bound is on and buying
    /// nothing". Per-table, not a process global.
    bounded_skips: AtomicU64,
    /// gen r4w3/cards3 — the SUMMARY map: one byte per [`SUMMARY_GROUP`]
    /// cards, non-clean when any card of its group may be dirty.
    ///
    /// # What it is for
    ///
    /// The scan bound took old-gen CAPACITY out of the young pause; what was
    /// left is the byte map up to the highest card ever dirtied, which in a
    /// long-lived server tends to the old-gen high-water mark
    /// (`docs/internal/gc/gengc-oldgen-card-scan-is-o-heap-FIXED-20260923.md`).
    /// With the summary the scans read `bound / 64` summary bytes and descend
    /// only into groups that hold something, so a scan that finds one dirty
    /// card reads ~bound/64 + 64 bytes instead of ~bound.
    ///
    /// # The invariant, and the order that keeps it
    ///
    /// **A non-clean card byte implies a non-clean summary byte**, at every
    /// instant a scan can observe. Every Rust mark path stores the summary
    /// byte BEFORE the card byte (the same "cover first" rule as the scan
    /// bound). The lock-free barrier stores it a SECOND time AFTER the card
    /// byte ([`Self::mark_dirty_lockfree`]), because the scans clear a
    /// summary byte before they scan its group (clean-before-process, as in
    /// G1): a peer frozen between the first summary store and the card store
    /// (BUG-03 `SuspendThread`) would otherwise resume, store the card, and
    /// leave a dirty card under a clean summary byte that no later scan
    /// descends into. The second store re-arms it; a peer frozen between the
    /// card store and the second store left the first summary store in
    /// place, so the scan sees that group anyway. The locked mark paths need
    /// only the first store: they hold `cells`, and so do the scans.
    ///
    /// # The JIT
    ///
    /// The only writer that could store a card byte without the summary byte
    /// is generated code holding [`Self::jit_cards_addr`]. Handing that
    /// address out sets `raw_card_address_escaped`, and the scans then ignore
    /// the summary and walk the byte map — the same disarm the scan bound
    /// uses, so a raw store can over-scan but never be lost. No production
    /// caller hands that address out.
    ///
    /// gen r4w4/cards4: the JIT's inline barrier
    /// (`CRATONVM_JIT_INLINE_CARD_MARK=1`) is handed [`Self::jit_card_view_addr`]
    /// instead. It only READS card bytes and calls the Rust barrier for every
    /// clean→dirty transition, so it never needs this fallback.
    ///
    /// Empty (length 0) when [`CardTableOptions::summary`] is off; allocated
    /// once in [`Self::with_options`] and never resized.
    summary: Vec<AtomicU8>,
    /// [`CardTableOptions::summary`], fixed at construction.
    summary_enabled: bool,
    /// [`CardTableOptions::precise_ref_array_marks`], fixed at construction.
    precise_ref_array_marks: bool,
    /// Cards whose bytes a scan did not read because their summary byte was
    /// clean, totalled over this table's life. Per-table, not a process global.
    summary_skips: AtomicU64,
    /// gen r4w4/cards4 — the read-only view the JIT's inline barrier loads
    /// through ([`Self::jit_card_view_addr`]). Boxed so its address is stable
    /// however the owning heap moves this table; per-table, not a process
    /// global.
    jit_view: Box<JitCardView>,
}

impl CardTable {
    /// Create a new card table covering a memory region starting at `base_addr`
    /// with the given `region_size` in bytes, with the options the VM's flags
    /// ask for ([`CardTableOptions::from_flags`]).
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        Self::with_options(base_addr, region_size, CardTableOptions::from_flags())
    }

    /// [`Self::new`] with explicit [`CardTableOptions`] (gen r4w3/cards3) —
    /// how a test builds the A and B arms of the same table side by side.
    pub fn with_options(base_addr: usize, region_size: usize, options: CardTableOptions) -> Self {
        let num_cards = region_size.div_ceil(CARD_SIZE);
        let summary: Vec<AtomicU8> = if options.summary {
            (0..num_cards.div_ceil(SUMMARY_GROUP))
                .map(|_| AtomicU8::new(CARD_CLEAN))
                .collect()
        } else {
            Vec::new()
        };
        // Allocate the byte map first so its buffer address can be cached
        // outside the mutex (see `cards_addr`). Moving the `Vec` into
        // `CardCells` below does not move the buffer this points at.
        let cards: Vec<AtomicU8> = (0..num_cards).map(|_| AtomicU8::new(CARD_CLEAN)).collect();
        let cards_addr = cards.as_ptr() as usize; // Cast: stable buffer base
        let jit_view = Box::new(JitCardView {
            magic: JIT_CARD_VIEW_MAGIC,
            old_base: AtomicUsize::new(base_addr),
            old_end: AtomicUsize::new(base_addr.wrapping_add(region_size)),
            cards_neg: AtomicUsize::new(cards_addr.wrapping_neg()),
            flags: if options.precise_ref_array_marks {
                JitCardView::FLAG_PRECISE_REF_ARRAYS
            } else {
                0
            },
        });
        Self {
            // SECURITY FIX (V6): assign a process-unique id so buffered
            // offsets can be drained table-scoped (Relaxed is sufficient: we
            // only need uniqueness, not ordering relative to other memory).
            id: NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            base_addr,
            region_size,
            cards_addr,
            num_cards,
            cells: Mutex::new(CardCells { cards }),
            pending_offsets: Mutex::new(Vec::new()),
            // gengc-round4/cards: self-maintained from zero — no card has been
            // dirtied yet, and every Rust mark path raises this before it
            // stores. See the field doc.
            scan_bound: AtomicUsize::new(0),
            bounded_skips: AtomicU64::new(0),
            raw_card_address_escaped: AtomicBool::new(false),
            buffered_path_warned: AtomicBool::new(false),
            summary,
            summary_enabled: options.summary,
            precise_ref_array_marks: options.precise_ref_array_marks,
            summary_skips: AtomicU64::new(0),
            jit_view,
        }
    }

    /// The options this table was built with.
    pub fn options(&self) -> CardTableOptions {
        CardTableOptions {
            summary: self.summary_enabled,
            precise_ref_array_marks: self.precise_ref_array_marks,
        }
    }

    /// May this table hold ELEMENT-precise marks for reference arrays? When
    /// `true` the barrier (`GenerationalHeap::write_barrier_ref_array_element`)
    /// dirties the card of the written element, and the dirty-card consumer
    /// must scan the part of any reference array that overlaps a dirty card,
    /// not only objects whose header lies in one. See
    /// [`CardTableOptions::precise_ref_array_marks`].
    #[inline]
    pub fn precise_ref_array_marks(&self) -> bool {
        self.precise_ref_array_marks
    }

    /// Cards the summary map let the scans skip over this table's life. `0`
    /// with the summary on means either nothing was scanned or every group
    /// below the bound held a dirty card.
    pub fn cards_skipped_by_summary(&self) -> u64 {
        self.summary_skips.load(Ordering::Relaxed)
    }

    /// Is the summary map what the scans consult right now? `false` when it
    /// was built off, and for good once the raw map address has escaped.
    #[inline]
    fn summary_active(&self) -> bool {
        self.summary_enabled && !self.raw_card_address_escaped.load(Ordering::Acquire)
    }

    /// Set the summary byte covering card `index` — the "cover first" half of
    /// every mark path (see the `summary` field doc). A load first, so a hot
    /// group's summary line stays read-shared, exactly like the card byte.
    #[inline]
    fn summarize(&self, index: usize) {
        if !self.summary_enabled {
            return;
        }
        if let Some(s) = self.summary.get(index / SUMMARY_GROUP) {
            if s.load(Ordering::Relaxed) == CARD_CLEAN {
                s.store(CARD_DIRTY, Ordering::Release);
            }
        }
    }

    /// Visit, in ascending order, every `[lo, hi)` card span below `limit`
    /// that a scan has to read, and account for what it skipped.
    ///
    /// With the summary active that is one span per non-clean summary group,
    /// and — when `consume` is set — the group's summary byte is CLEARED
    /// before its span is handed out (clean-before-process: see the
    /// `summary` field doc for why the barrier's second summary store makes
    /// that safe against a frozen peer). Otherwise it is the single span
    /// `[0, limit)`, which is the pre-summary scan exactly.
    fn for_each_scan_span(&self, limit: usize, consume: bool, mut f: impl FnMut(usize, usize)) {
        if !self.summary_active() {
            if limit > 0 {
                f(0, limit);
            }
            return;
        }
        let groups = limit.div_ceil(SUMMARY_GROUP).min(self.summary.len());
        let mut skipped: u64 = 0;
        // The summary itself is probed [`SCAN_GROUP`] bytes at a time, the same
        // relaxed OR-fold the byte map uses: a clean run of eight summary
        // bytes (512 cards, 256 KiB of old generation) is one branch.
        for chunk_start in (0..groups).step_by(SCAN_GROUP) {
            let chunk_end = (chunk_start + SCAN_GROUP).min(groups);
            if !group_has_dirty(&self.summary[chunk_start..chunk_end]) {
                let lo = chunk_start * SUMMARY_GROUP;
                let hi = (chunk_end * SUMMARY_GROUP).min(limit);
                skipped += (hi - lo) as u64;
                continue;
            }
            for g in chunk_start..chunk_end {
                let s = &self.summary[g];
                let lo = g * SUMMARY_GROUP;
                let hi = (lo + SUMMARY_GROUP).min(limit);
                if s.load(Ordering::Acquire) == CARD_CLEAN {
                    skipped += (hi - lo) as u64;
                    continue;
                }
                if consume {
                    s.store(CARD_CLEAN, Ordering::Relaxed);
                    // The clear must be ordered before the card loads of this
                    // group, so that a mark whose summary store follows the
                    // clear is either seen in the card bytes or re-arms the
                    // summary. Scans run at STW today, where this is free.
                    //
                    // gen r5w5/old9: this fence is only HALF of what a scan
                    // concurrent with mutators would need. The shape is
                    // store-buffering: the scan stores the summary then loads
                    // the card; the barrier (`mark_dirty_lockfree`) stores the
                    // card then loads the summary (`summarize`, Relaxed), with
                    // no fence between. Without one there too, both loads can
                    // read the old value — the barrier skips the re-arm, the
                    // scan misses the card — and a dirty card sits under a
                    // clean summary that every later scan skips. Before any
                    // scan leaves the pause, add
                    // `fence(SeqCst)` between `card.store` and the second
                    // `summarize` in `mark_dirty_lockfree` (clean->dirty arm
                    // only).
                    std::sync::atomic::fence(Ordering::SeqCst);
                }
                f(lo, hi);
            }
        }
        if skipped > 0 {
            self.summary_skips.fetch_add(skipped, Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------
    // Collector-side scan bound (gengc-round2)
    // -----------------------------------------------------------------

    /// Publish an upper bound on the covered region that any dirty card can
    /// fall in, as a byte count from [`Self::base_addr`].
    ///
    /// The bound only ever moves UP, so an interleaving of two publishers can
    /// over-scan but never under-scan.
    ///
    /// gengc-round4/cards: since the bound became self-maintained (every Rust
    /// mark path raises it before it stores, and handing out the raw map
    /// address disarms it) nothing NEEDS to call this, and a call with a small
    /// value is harmless — it is a `max`. It is kept as the internal raise
    /// primitive and for a future publisher that wants to over-cover on
    /// purpose. The round-2 warning that a too-small publish was a
    /// premature-reclamation bug applied to the old UNSET-initialised table,
    /// where the first publish REPLACED "unbounded"; that replacement arm is
    /// now reachable only if something stores [`SCAN_BOUND_UNSET`], which
    /// nothing does.
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

    /// Exclusive card index the collector-side scans stop at: one past the
    /// highest card any Rust mark path has dirtied (or anything published
    /// higher). Equal to [`Self::num_cards`] once [`Self::jit_cards_addr`] has
    /// handed the raw map out, because a store through that address cannot
    /// raise the bound.
    pub fn scan_bound_cards(&self) -> usize {
        if self.raw_card_address_escaped.load(Ordering::Acquire) {
            return self.num_cards;
        }
        let bound = self.scan_bound.load(Ordering::Acquire);
        if bound == SCAN_BOUND_UNSET {
            self.num_cards
        } else {
            bound.min(self.num_cards)
        }
    }

    /// Cards the bound has let the collector-side scans skip over this table's
    /// life. `0` on a table that has been collected means the bound is inert —
    /// either the highest dirtied card is at the top of the map, or
    /// [`Self::raw_card_address_escaped`] is `true`.
    pub fn cards_skipped_by_scan_bound(&self) -> u64 {
        self.bounded_skips.load(Ordering::Relaxed)
    }

    /// Has [`Self::jit_cards_addr`] handed the raw map address out? Once it
    /// has, the self-maintained scan bound is disarmed for good and every scan
    /// walks the whole map. Exposed so a report can say WHY
    /// [`Self::cards_skipped_by_scan_bound`] is zero.
    pub fn raw_card_address_escaped(&self) -> bool {
        self.raw_card_address_escaped.load(Ordering::Acquire)
    }

    /// Raise the bound so that `index` is inside it. Called from every mark
    /// path, BEFORE that path stores `CARD_DIRTY`, so no instant exists at
    /// which a non-clean byte lies above the bound; see the
    /// [`Self::scan_bound`] field doc.
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
    /// This is the *slow* path — it takes the `cells` mutex (it used to
    /// maintain the tracking list under it; gcd d1/d removed the list, and the
    /// lock now only orders it against the collector's passes). The mutator
    /// write barrier must not call it; it uses
    /// [`Self::mark_dirty_lockfree`]. (gengc-round4/cards: this doc used to
    /// route the barrier through [`Self::thread_local_dirty_addr`], the dead
    /// buffered path.)
    #[inline]
    pub fn mark_dirty(&self, addr: usize) {
        if addr < self.base_addr || addr >= self.base_addr + self.region_size {
            return;
        }
        let index = (addr - self.base_addr) / CARD_SIZE;
        self.cover_card_index(index);
        let cells = self.cells.lock();
        // gen r4w3/cards3: summary before card, under the lock the scans hold.
        self.summarize(index);
        if let Some(card) = cells.cards.get(index) {
            // gcd d1/d: the byte is the whole record (no tracking list).
            let _ = card.compare_exchange(
                CARD_CLEAN,
                CARD_DIRTY,
                Ordering::Release,
                Ordering::Relaxed,
            );
        }
    }

    /// gc-genpause F5.2 -- THE mutator write-barrier fast path: mark the card
    /// containing `addr` dirty with no lock and no allocation.
    ///
    /// A bounds check, a shift, a relaxed load and, only if the card is not
    /// already dirty, one release byte-store. The interpreter, the natives and
    /// compiled code all dirty a card HERE: gen r4w4/cards4's inline JIT
    /// barrier (`Compiler::emit_gen_card_check`) performs only the relaxed
    /// "already dirty?" load inline and calls into this function for the
    /// clean→dirty store, so there is one card-writing rule in this VM.
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
    /// # Why the byte is the whole record
    ///
    /// `take_dirty_cards` scans the byte map (up to the scan bound this path
    /// raises, through the summary map it sets), which is what finds the JIT's
    /// direct stores too. gcd d1/d removed the locked paths' `dirty_cards`
    /// tracking list, which the scan already covered.
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
            // Keep the self-maintained scan bound covering this card, and do
            // it BEFORE the store (gengc-round4/cards): the scans stop at the
            // bound, so the invariant "every non-clean byte is below it" must
            // hold at every instant, including for a peer frozen between these
            // two lines. Covered-but-clean only over-scans; dirty-but-uncovered
            // would lose the card. Costs one relaxed load of a read-mostly line
            // and a not-taken branch except when this is a new highest card.
            self.cover_card_index(index);
            // gen r4w3/cards3: summary byte BEFORE the card byte (so a scan
            // never sees a dirty card under a clean summary) and AGAIN after
            // it (so a scan that cleared the summary while this thread was
            // frozen between the two stores cannot leave the card orphaned).
            // Both are a relaxed load of an already-set byte after the first
            // mark of a group; only this clean->dirty arm pays them. See the
            // `summary` field doc.
            self.summarize(index);
            card.store(CARD_DIRTY, Ordering::Release);
            self.summarize(index);
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
            // Like `card_marks_executed`, it counts every mark that enters
            // here. gengc-round4/cards: that is EVERY generational card mark
            // by default, compiled ones included — compiled stores reach
            // `write_barrier` through the JIT helpers. This used to say
            // "interpreter and native marks only".
            //
            // gen r5w2/alloc6: NOT under `CRATONVM_JIT_INLINE_CARD_MARK=1`
            // (gen r4w4/cards4, opt-in). The inline barrier reads the card
            // byte itself and calls in only for a CLEAN card, so a compiled
            // store that hits an already-dirty card never reaches this arm:
            // with that flag the duplicate ratio below counts interpreter,
            // native and first-touch compiled marks only, and must not be
            // compared across the flag's two arms.
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
            // See `gengc-plumbing-duplicate-card-mark-denominator-FIXED-20260923.md`.
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
        let cells = self.cells.lock();
        let end = self.base_addr + self.region_size;
        // gce e1/y: the collector's re-dirty lists (the non-moving sweep's
        // `redirty_cards`, one entry per old->young SLOT the card scan found;
        // the moving cycle's deferred re-marks) name the same card many times
        // in a row -- every slot of a wide reference array, every field of one
        // holder. A repeat of the previous index is skipped, and a card already
        // dirty is not written: `compare_exchange` is a locked read-modify-write
        // even when it fails, and a card is never cleared while this lock is
        // held (every clear takes `cells`), so "dirty" read here stays true.
        // The set of dirty cards, the summary and the scan bound end exactly
        // as the per-entry arm left them.
        let mut last = usize::MAX;
        for &addr in addrs {
            if addr < self.base_addr || addr >= end {
                continue;
            }
            let index = (addr - self.base_addr) / CARD_SIZE;
            if index == last {
                continue;
            }
            last = index;
            self.cover_card_index(index);
            self.summarize(index);
            if let Some(card) = cells.cards.get(index) {
                if card.load(Ordering::Relaxed) == CARD_DIRTY {
                    continue;
                }
                // gcd d1/d: the byte is the whole record (no tracking list).
                let _ = card.compare_exchange(
                    CARD_CLEAN,
                    CARD_DIRTY,
                    Ordering::Release,
                    Ordering::Relaxed,
                );
            }
        }
    }

    /// Check if a specific card is dirty.
    ///
    /// PERF (gengc-round1, 2026-09-20): reads the byte map through
    /// `cards_addr` instead of taking the collector's `cells` mutex. The lock
    /// serialises the multi-byte passes, which this is not; the byte itself is
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
    ///
    /// # gen r4w3/cards3: only the summary groups that hold something
    ///
    /// With the summary active the pass visits the byte map only inside
    /// groups whose summary byte is set, clearing that byte first; see
    /// [`Self::for_each_scan_span`].
    pub fn clear_all(&self) {
        let cells = self.cells.lock();
        let limit = self.scan_bound_cards().min(cells.cards.len());
        self.note_bounded_skip(limit);
        let cards = &cells.cards;
        self.for_each_scan_span(limit, true, |lo, hi| {
            for group in cards[lo..hi].chunks(SCAN_GROUP) {
                if !group_has_dirty(group) {
                    continue;
                }
                for card in group {
                    if card.load(Ordering::Acquire) != CARD_CLEAN {
                        card.store(CARD_CLEAN, Ordering::Release);
                    }
                }
            }
        });
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
    /// returned card `CARD_CLEAN`) and clears the summary bytes it reads.
    /// (gcd d3/n: this used to say it "folds in the O(dirty) tracking list";
    /// gcd d1/d removed that list, and both functions are the same bounded,
    /// summary-guided byte scan.)
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
    /// `docs/internal/gc/gengc-oldgen-card-scan-is-o-heap-FIXED-20260923.md`.
    pub fn dirty_card_indices(&self) -> Vec<usize> {
        let cells = self.cells.lock();
        let limit = self.scan_bound_cards().min(cells.cards.len());
        self.note_bounded_skip(limit);
        let mut out = Vec::new();
        // gen r4w3/cards3: read-only, so the summary is consulted but never
        // cleared (`consume == false`).
        self.for_each_scan_span(limit, false, |lo, hi| {
            for (group_index, group) in cells.cards[lo..hi].chunks(SCAN_GROUP).enumerate() {
                if !group_has_dirty(group) {
                    continue;
                }
                let base = lo + group_index * SCAN_GROUP;
                for (j, card) in group.iter().enumerate() {
                    if card.load(Ordering::Acquire) == CARD_DIRTY {
                        out.push(base + j);
                    }
                }
            }
        });
        out
    }

    /// Return the indices of every dirty card, ASCENDING and without
    /// duplicates, and reset those cards' bitmap bytes to `CARD_CLEAN`.
    ///
    /// One acquire scan of the atomic bitmap (up to the scan bound, through
    /// the summary map) is the whole answer: every mark path — the lock-free
    /// barrier, the JIT's direct stores, `mark_dirty`, `mark_dirty_bulk`,
    /// `drain_pending` — leaves its mark in the byte, inside the bound and
    /// under a set summary byte. This keeps the JIT mutator path to a single
    /// release byte-store while preserving the exact card set at the
    /// stop-the-world consumer boundary.
    ///
    /// gcd d1/d (2026-09-27): this used to MERGE a `dirty_cards` tracking list
    /// into the scan and then `sort_unstable` + `dedup` the result. The list was
    /// a subset of what the scan finds (see `CardCells`), so the sort existed
    /// only to remove the duplicates the merge made; it and the list are gone.
    /// The scan visits groups in ascending order, so the output is sorted.
    ///
    /// B-K / bt18 fix (2026-06-14): the byte reset is load-bearing for the
    /// NON-MOVING young sweep, which (unlike the moving Cheney path) NEVER calls
    /// [`Self::clear_all`]. When this function still answered from the tracking
    /// list, a consumed byte left DIRTY made the sweep's later
    /// `mark_dirty_bulk(redirty_*)` a no-op on the LIST (it pushed only on a
    /// clean→dirty CAS), so the edge was never returned again and the live young
    /// child was swept (premature reclamation, bt18 = 68273854 vs golden
    /// 68332206). With the byte scan as the whole answer a dirty byte cannot be
    /// missed, but the reset is still the consume contract: a byte left dirty
    /// would be returned, and its card rescanned, by every later call. The
    /// moving path is unaffected: its `clear_all` wipes the whole bitmap anyway,
    /// so the early per-card clear is redundant, never harmful.
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
        let cells = self.cells.lock();
        let mut taken = Vec::new();
        let limit = self.scan_bound_cards().min(cells.cards.len());
        self.note_bounded_skip(limit);
        // gen r4w3/cards3: with the summary active, only the groups whose
        // summary byte is set are read, and each such byte is cleared before
        // its group is consumed. Every card this consumes is left CARD_CLEAN
        // and every summary byte it clears covers only cards it consumed, so
        // the bt18 bitmap<->list contract and the summary invariant both hold
        // on return.
        let cards = &cells.cards;
        self.for_each_scan_span(limit, true, |lo, hi| {
            for (group_index, group) in cards[lo..hi].chunks(SCAN_GROUP).enumerate() {
                if !group_has_dirty(group) {
                    continue;
                }
                let base = lo + group_index * SCAN_GROUP;
                for (j, card) in group.iter().enumerate() {
                    // Acquire load pairs with the JIT's release store of CARD_DIRTY.
                    if card.load(Ordering::Acquire) == CARD_DIRTY {
                        card.store(CARD_CLEAN, Ordering::Release);
                        taken.push(base + j);
                    }
                }
            }
        });
        debug_assert!(
            taken.windows(2).all(|w| w[0] < w[1]),
            "take_dirty_cards: the byte scan must yield ascending, distinct indices",
        );
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
    ///
    /// # Handing the address out disarms the scan bound (gengc-round4/cards)
    ///
    /// A store through this address is a card write this type cannot see, so
    /// it cannot raise the self-maintained `scan_bound`. The first call
    /// therefore sets `raw_card_address_escaped` — BEFORE the address is
    /// returned, so no generated store can precede it — and every later scan
    /// covers the whole map. Callers that do not intend to emit raw stores
    /// must not call this merely to cache the address:
    /// `vm/src/jit/helpers.rs` stopped doing so for exactly this reason, and
    /// since gen r4w4/cards4 it fetches [`Self::jit_card_view_addr`] instead,
    /// which does not disarm anything. No production caller remains.
    pub fn jit_cards_addr(&self) -> usize {
        // SeqCst: this is a once-per-table event, and the strongest ordering
        // makes "the flag is visible before any store through the address"
        // hold without an argument about how the address travels to the
        // emitter.
        self.raw_card_address_escaped.store(true, Ordering::SeqCst);
        // gc-genpause F5.2: cached at construction, so this no longer takes
        // the collector's mutex to hand out an address that has been constant
        // since `new`.
        self.cards_addr
    }

    /// gen r4w4/cards4 — address of this table's [`JitCardView`], for the JIT's
    /// inline generational post barrier.
    ///
    /// Unlike [`Self::jit_cards_addr`] this does NOT set
    /// `raw_card_address_escaped`: the view hands out the byte map for READING
    /// only, and compiled code turns a clean card dirty exclusively by calling
    /// the collector's barrier (see the [`JitCardView`] doc). The scan bound
    /// and the summary map therefore stay armed.
    ///
    /// Valid for this table's lifetime (the view is boxed, so moving the table
    /// does not move it). Compiled code that bakes it must not outlive the
    /// heap, which is the same contract every other address in the JIT's
    /// helper table carries.
    pub fn jit_card_view_addr(&self) -> usize {
        let view: &JitCardView = &self.jit_view;
        view as *const JitCardView as usize // Cast: stable boxed address
    }

    /// The view itself, for tests and diagnostics.
    pub fn jit_card_view(&self) -> &JitCardView {
        &self.jit_view
    }

    /// Re-publish the three run-time words of the [`JitCardView`] from this
    /// table's current geometry.
    ///
    /// Nothing changes that geometry today — `base_addr`, `region_size` and
    /// the card map are fixed at construction — so this is a no-op store of
    /// the same values. It exists for a coverage extension (old-gen growth):
    /// whatever re-points `cards_addr` or widens the covered range MUST call
    /// this, at a stop-the-world point, or compiled code keeps range-testing
    /// against the old bounds (a store into the new range then takes the
    /// helper, which is safe) and, worse, indexing the old map.
    pub fn refresh_jit_card_view(&self) {
        self.jit_view
            .old_base
            .store(self.base_addr, Ordering::Release);
        self.jit_view
            .old_end
            .store(self.base_addr.wrapping_add(self.region_size), Ordering::Release);
        self.jit_view
            .cards_neg
            .store(self.cards_addr.wrapping_neg(), Ordering::Release);
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
        // gengc-round4/cards — item #6's first step
        // (`docs/feature-designs/gc-round-20260920-generational.md`): this
        // pipeline has had no production producer since gc-genpause F5.2, and
        // the retire-or-keep decision needs the fact "is it genuinely
        // unreached, or merely unreached on the paths the suite exercises?".
        // Say so, once per table, the first time anything feeds it. The other
        // half of that first step (`debug_assert!(pending.is_empty())` in
        // `clear_all`) was superseded when `clear_all` started FOLDING the
        // queue instead of dropping it — an assert there would now fire on
        // the fold's own tests — so this producer-side latch is the tripwire.
        if !self.buffered_path_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                table_id = self.id,
                offset,
                "card table: the BUFFERED card pipeline (`thread_local_dirty`) has a \
                 producer. It has had none in production since gc-genpause F5.2; the \
                 live barrier is `mark_dirty_lockfree`. Report the caller — see \
                 docs/internal/gc/gengc-oldgen-dead-buffered-card-pipeline-FIXED-20260923.md.",
            );
        }
        // SECURITY FIX (V6): push into this table's bucket of the registered
        // per-thread buffer (Arc<Mutex<DirtyPartitions>>) so a drain by another
        // table cannot consume it. The lock is uncontended on the mutator fast
        // path (only this thread touches it outside STW); it is acquirable by
        // the collector only at STW, when this thread is parked.
        //
        // `try_with` (gc-common w1-g, 2026-09-23): this pipeline is retained
        // for a future buffered caller, and for such a caller a store made from
        // another thread-local's destructor after this buffer's destructor ran
        // would make `with` panic inside a TLS destructor, i.e. abort. There is
        // no buffer left to append to, so mark the card directly -- the same
        // byte `drain_pending` would eventually have set for this offset.
        let should_flush = match THREAD_DIRTY_BUFFER.try_with(|guard| {
            let mut b = guard.buffer.lock();
            b.push(self.id, offset) >= THREAD_BUFFER_FLUSH_THRESHOLD
        }) {
            Ok(should_flush) => should_flush,
            Err(_) => {
                self.mark_dirty_lockfree(self.base_addr.wrapping_add(offset));
                return;
            }
        };
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
        // `try_with`: see `thread_local_dirty`. A destroyed buffer has nothing
        // left to flush -- its guard either deregistered an empty buffer or
        // left a non-empty one registered for `flush_all` to drain.
        let offsets = THREAD_DIRTY_BUFFER
            .try_with(|guard| guard.buffer.lock().take(self.id))
            .unwrap_or_default();
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
    /// (gcd d1/d: which is now the whole record; the tracking `dirty_cards`
    /// list is gone).
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
        // gen r5w5/old9: counted per IN-RANGE entry. `pending.len()` also
        // held the out-of-range offsets the loop skips, so they were reported
        // as duplicate marks.
        let mut in_range = 0u64;
        let cells = self.cells.lock();
        let mut newly_dirtied = 0usize;
        for offset in pending {
            let addr = self.base_addr.wrapping_add(offset);
            if addr < self.base_addr || addr >= self.base_addr + self.region_size {
                continue;
            }
            in_range += 1;
            let index = (addr - self.base_addr) / CARD_SIZE;
            self.cover_card_index(index);
            self.summarize(index);
            if index < cells.cards.len()
                && cells.cards[index]
                    .compare_exchange(CARD_CLEAN, CARD_DIRTY, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
            {
                newly_dirtied += 1;
            }
        }
        crate::gc_metrics::record_duplicate_card_marks(
            in_range.saturating_sub(newly_dirtied as u64),
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
    /// * the undrained `pending_offsets` queue — one `usize` per buffered
    ///   old→young edge not yet folded into the bitmap;
    /// * the summary map, when on.
    ///
    /// (gcd d1/d: the `dirty_cards` tracking list this used to count is gone;
    /// see `CardCells`.)
    ///
    /// Excludes the per-thread [`DirtyPartitions`] buffers, which are owned by
    /// the threads rather than by this table. Takes both locks, so this is a
    /// diagnostic call (`gc_metrics`), not something to put on an allocation
    /// path.
    pub fn retained_bytes(&self) -> usize {
        let cells = self.cells.lock();
        let map = std::mem::size_of_val(&cells.cards[..]);
        drop(cells);
        let pending = self.pending_offsets.lock().capacity() * std::mem::size_of::<usize>();
        // gen r4w3/cards3: the summary map, 1/64 of the byte map when on.
        let summary = std::mem::size_of_val(&self.summary[..]);
        map + pending + summary
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
pub(crate) mod tests {
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
    ///
    /// gen r4w3/cards3: `pub(crate)`, because the barrier-heavy randomized
    /// tests (`gen_r4w3_cards3_tests` here and in `gen_heap.rs`) make
    /// thousands of DUPLICATE lock-free marks, and one landing inside the
    /// window where an exact-count test has the gate armed would make that
    /// test fail. They take this lock too.
    pub(crate) static METRICS_ARM: Mutex<()> = Mutex::new(());

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

    /// gengc-round4/cards: the bound is self-maintained. A fresh table has
    /// dirtied nothing, so it scans nothing; each mark path raises the bound
    /// to one past the card it dirties; consuming or clearing a card never
    /// lowers it. (Round 2's `an_unwired_table_scans_the_whole_map` pinned the
    /// old UNSET-initialised default and is replaced by this.)
    #[test]
    fn the_scan_bound_tracks_the_highest_card_ever_dirtied() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 1024);
        assert_eq!(ct.scan_bound_cards(), 0, "nothing dirtied, nothing to scan");
        assert!(ct.take_dirty_cards().is_empty());
        assert_eq!(ct.cards_skipped_by_scan_bound(), 1024);

        ct.mark_dirty_lockfree(0x1000 + 39 * CARD_SIZE + 17);
        assert_eq!(ct.scan_bound_cards(), 40);
        assert_eq!(ct.take_dirty_cards(), vec![39]);
        assert_eq!(ct.cards_skipped_by_scan_bound(), 1024 + (1024 - 40));

        // Consumed — but the bound does not come back down, so a card below
        // it that is dirtied later is still inside the scan.
        assert_eq!(ct.scan_bound_cards(), 40);
        ct.mark_dirty_lockfree(0x1000 + 3 * CARD_SIZE);
        assert_eq!(ct.scan_bound_cards(), 40);
        assert_eq!(ct.take_dirty_cards(), vec![3]);

        // The last card of the map covers the whole map, and a clear keeps it.
        ct.mark_dirty(0x1000 + 1023 * CARD_SIZE);
        assert_eq!(ct.scan_bound_cards(), 1024);
        ct.clear_all();
        assert_eq!(ct.scan_bound_cards(), 1024);
        assert!(!ct.raw_card_address_escaped());
    }

    /// gengc-round4/cards: every card a Rust path can dirty is found by every
    /// scan, on a table whose bound is self-maintained — the equivalence the
    /// change rests on, checked against a brute-force `is_dirty` sweep (which
    /// ignores the bound) for all three scans and all four mark paths.
    #[test]
    fn a_self_maintained_bound_never_hides_a_dirty_card() {
        let base = 0x10_0000usize;
        let num = 300usize;
        let marks: [usize; 6] = [0, 7, 8, 150, 151, 299];
        for path in 0..4 {
            let ct = CardTable::new(base, CARD_SIZE * num);
            for &i in &marks {
                let addr = base + i * CARD_SIZE + 5;
                match path {
                    0 => ct.mark_dirty_lockfree(addr),
                    1 => ct.mark_dirty(addr),
                    2 => ct.mark_dirty_bulk(&[addr]),
                    _ => {
                        ct.thread_local_dirty_addr(addr);
                        ct.flush_dirty_buffer();
                        ct.drain_pending();
                    }
                }
            }
            let expected: Vec<usize> = marks.to_vec();
            assert_eq!(dirty_by_brute_force(&ct), expected, "path {path}");
            assert_eq!(ct.dirty_card_indices(), expected, "path {path}");
            assert_eq!(ct.take_dirty_cards(), expected, "path {path}");
            for &i in &marks {
                ct.mark_dirty_lockfree(base + i * CARD_SIZE);
            }
            ct.clear_all();
            assert!(dirty_by_brute_force(&ct).is_empty(), "path {path}");
        }
    }

    /// The bound is monotone upward: a lower publish is ignored, a higher one
    /// raises it. A race between two publishers can therefore only ever make
    /// the scan cover MORE cards.
    #[test]
    fn the_scan_bound_only_ever_moves_up() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 64);
        assert_eq!(ct.scan_bound_cards(), 0, "self-maintained: starts empty");

        ct.set_scan_bound_bytes(CARD_SIZE * 8);
        assert_eq!(ct.scan_bound_cards(), 8, "a publish raises it");

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

    /// The hazard that kept round 2's bound unpublished, and why it is now
    /// closed structurally rather than by nobody publishing.
    ///
    /// A store made ONLY through `jit_cards_addr` — which is what generated
    /// code does, and what `Compiler::inline_card_mark_available()` would
    /// re-enable — bypasses every Rust mark path, so it cannot raise the
    /// bound. Round 2 pinned the resulting loss (`a_raw_store_above_a_published
    /// _bound_is_why_nothing_publishes_one`) and asked for the mechanism to be
    /// re-derived if a raw store were ever found. It was
    /// (gengc-round4/cards): obtaining the address sets
    /// `raw_card_address_escaped`, which disarms the bound for the table's
    /// life, so a raw store anywhere — above the self-maintained bound AND
    /// above an explicit publish — is found.
    #[test]
    fn handing_out_the_raw_address_disarms_the_scan_bound() {
        let ct = CardTable::new(0x1000, CARD_SIZE * 64);
        ct.mark_dirty_lockfree(0x1000 + 2 * CARD_SIZE);
        assert_eq!(ct.scan_bound_cards(), 3);
        assert!(!ct.raw_card_address_escaped());

        let cards = ct.jit_cards_addr() as *const AtomicU8;
        assert!(ct.raw_card_address_escaped());
        assert_eq!(ct.scan_bound_cards(), 64, "the whole map again");

        // SAFETY: `cards` is the table's live `[AtomicU8; 64]` and 40 < 64.
        unsafe { &*cards.add(40) }.store(CARD_DIRTY, Ordering::Release);
        assert_eq!(ct.take_dirty_cards(), vec![2, 40]);

        // An explicit publish below the raw store cannot hide it either.
        ct.set_scan_bound_bytes(CARD_SIZE * 8);
        // SAFETY: as above.
        unsafe { &*cards.add(40) }.store(CARD_DIRTY, Ordering::Release);
        assert_eq!(
            ct.take_dirty_cards(),
            vec![40],
            "once the address has escaped, no bound may hide a raw store",
        );
        assert_eq!(ct.cards_skipped_by_scan_bound(), 0);
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
            "the lock-free barrier must dirty exactly the cards the buffered path did"
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

    /// gc-common w1-g: a buffered card mark made from ANOTHER thread-local's
    /// destructor must neither abort (`LocalKey::with` panics on a destroyed
    /// key) nor lose the card, whichever order the platform tears the two
    /// thread-locals down in.
    #[test]
    fn a_buffered_mark_from_a_tls_destructor_is_not_lost() {
        struct MarkOnDrop(Arc<CardTable>, usize);
        impl Drop for MarkOnDrop {
            fn drop(&mut self) {
                self.0.thread_local_dirty_addr(self.1);
            }
        }
        thread_local! {
            static LATE: std::cell::RefCell<Option<MarkOnDrop>> =
                const { std::cell::RefCell::new(None) };
        }
        const BASE: usize = 0x40_0000;
        let ct = Arc::new(CardTable::new(BASE, CARD_SIZE * 16));
        let ct2 = Arc::clone(&ct);
        std::thread::spawn(move || {
            // LATE registered first, the dirty buffer second: on
            // reverse-order platforms the buffer dies before LATE's drop.
            let late = MarkOnDrop(Arc::clone(&ct2), BASE + CARD_SIZE * 9);
            LATE.with(|l| *l.borrow_mut() = Some(late));
            ct2.thread_local_dirty_addr(BASE + CARD_SIZE * 2);
        })
        .join()
        .expect("a card mark from a TLS destructor must not abort the thread");
        ct.flush_all();
        ct.drain_pending();
        let taken = ct.take_dirty_cards();
        assert!(taken.contains(&9), "the destructor's card was lost: {taken:?}");
        assert!(taken.contains(&2), "the ordinary buffered card was lost: {taken:?}");
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
        // gen r4w3/cards3: the exact-size claim is about the LEGACY table; a
        // summary adds one byte per `SUMMARY_GROUP` cards, checked below.
        let ct = CardTable::with_options(0x1000, region, CardTableOptions::LEGACY);
        let empty = ct.retained_bytes();
        assert_eq!(
            empty,
            region / CARD_SIZE,
            "an untouched table costs exactly the card byte-map (1/512 of the \
             covered region)",
        );
        let with_summary = CardTable::with_options(
            0x1000,
            region,
            CardTableOptions {
                summary: true,
                precise_ref_array_marks: false,
            },
        );
        assert_eq!(
            with_summary.retained_bytes(),
            region / CARD_SIZE + (region / CARD_SIZE).div_ceil(SUMMARY_GROUP),
            "the summary map costs one byte per SUMMARY_GROUP cards",
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
        // gcd d1/d: the drain folds the queue into the byte map and releases
        // it; there is no dirty-card index beside the map any more (it was a
        // duplicate of the byte scan, see `CardCells`), so the table is back to
        // the map's own cost.
        assert_eq!(
            ct.retained_bytes(),
            empty,
            "a drained table retains only its byte map",
        );
        assert_eq!(ct.take_dirty_cards(), (0..16).collect::<Vec<usize>>());
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

/// gen r4w3/cards3 (2026-09-23): the summary map. A module of its own so the
/// wave's lanes do not all append to `mod tests`.
#[cfg(test)]
mod gen_r4w3_cards3_tests {
    use super::*;

    const ON: CardTableOptions = CardTableOptions {
        summary: true,
        precise_ref_array_marks: false,
    };

    /// Deterministic xorshift, so a failing seed can be replayed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn brute(ct: &CardTable) -> Vec<usize> {
        (0..ct.num_cards()).filter(|&i| ct.is_dirty(i)).collect()
    }

    /// The equivalence the summary rests on: over randomized interleavings of
    /// all four mark paths and all three scans, a summarised table returns
    /// exactly the card sets, and leaves exactly the byte map, that the flat
    /// (legacy) table does. The region is deliberately not a whole number of
    /// summary groups, and the first round also marks the first and last card
    /// of a group on both sides of a group boundary.
    #[test]
    fn the_summary_scan_finds_exactly_what_the_flat_scan_finds() {
        let _arm = super::tests::METRICS_ARM.lock();
        let base = 0x40_0000usize;
        let num = SUMMARY_GROUP * 7 + 13;
        for seed in 1..=24u64 {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
            let flat = CardTable::with_options(base, CARD_SIZE * num, CardTableOptions::LEGACY);
            let summ = CardTable::with_options(base, CARD_SIZE * num, ON);
            let fixed = [0, SUMMARY_GROUP - 1, SUMMARY_GROUP, 3 * SUMMARY_GROUP - 1, num - 1];
            for round in 0..40 {
                let cards: Vec<usize> = if round == 0 {
                    fixed.to_vec()
                } else {
                    (0..rng.below(12)).map(|_| rng.below(num)).collect()
                };
                for card in cards {
                    let addr = base + card * CARD_SIZE + rng.below(CARD_SIZE);
                    let path = rng.below(4);
                    for ct in [&flat, &summ] {
                        match path {
                            0 => ct.mark_dirty_lockfree(addr),
                            1 => ct.mark_dirty(addr),
                            2 => ct.mark_dirty_bulk(&[addr]),
                            _ => {
                                ct.thread_local_dirty_addr(addr);
                                ct.flush_dirty_buffer();
                                ct.drain_pending();
                            }
                        }
                    }
                }
                match rng.below(4) {
                    0 => assert_eq!(
                        summ.dirty_card_indices(),
                        flat.dirty_card_indices(),
                        "seed {seed} round {round}: dirty_card_indices"
                    ),
                    1 | 2 => assert_eq!(
                        summ.take_dirty_cards(),
                        flat.take_dirty_cards(),
                        "seed {seed} round {round}: take_dirty_cards"
                    ),
                    _ => {
                        summ.clear_all();
                        flat.clear_all();
                    }
                }
                assert_eq!(brute(&summ), brute(&flat), "seed {seed} round {round}: byte map");
            }
            // And the table's own brute force agrees with its scans at the end.
            assert_eq!(summ.dirty_card_indices(), brute(&summ), "seed {seed}");
            assert_eq!(summ.take_dirty_cards(), flat.take_dirty_cards(), "seed {seed}");
        }
    }

    /// The summary actually skips, and says how much: one dirty card at each
    /// end of a 32-group bound reads two groups and skips thirty. A second
    /// scan, after the first cleared both summary bytes, skips everything.
    #[test]
    fn the_summary_skips_clean_groups_and_counts_them() {
        let _arm = super::tests::METRICS_ARM.lock();
        let base = 0x10_0000usize;
        let groups = 32;
        let ct = CardTable::with_options(base, CARD_SIZE * SUMMARY_GROUP * groups, ON);
        let far = SUMMARY_GROUP * (groups - 1) + 1;
        ct.mark_dirty_lockfree(base + 5 * CARD_SIZE);
        ct.mark_dirty_lockfree(base + far * CARD_SIZE);
        assert_eq!(ct.scan_bound_cards(), far + 1);
        assert_eq!(ct.take_dirty_cards(), vec![5, far]);
        assert_eq!(
            ct.cards_skipped_by_summary(),
            (SUMMARY_GROUP * (groups - 2)) as u64,
            "thirty whole groups between the two marks were never read",
        );
        assert!(ct.take_dirty_cards().is_empty());
        assert_eq!(
            ct.cards_skipped_by_summary(),
            (SUMMARY_GROUP * (groups - 2) + far + 1) as u64,
            "the consuming scan cleared both summary bytes, so the second skips all",
        );
        // The flat table never counts a summary skip.
        let flat = CardTable::with_options(base, CARD_SIZE * 64, CardTableOptions::LEGACY);
        flat.mark_dirty_lockfree(base);
        assert_eq!(flat.take_dirty_cards(), vec![0]);
        assert_eq!(flat.cards_skipped_by_summary(), 0);
    }

    /// Why the lock-free barrier stores the summary byte TWICE.
    ///
    /// A peer frozen between the first summary store and its card store (BUG-03
    /// `SuspendThread`) lets a scan clear the summary byte and find the card
    /// still clean. When the peer resumes, its card store lands under a clean
    /// summary byte. With only the first store, no later scan would descend
    /// into that group again until something else in it was dirtied — the edge
    /// would be lost for good. The second store re-arms the group.
    ///
    /// Modelled by driving `mark_dirty_lockfree`'s three stores by hand around
    /// a consuming scan.
    #[test]
    fn the_barriers_second_summary_store_rearms_a_group_a_scan_cleared() {
        let _arm = super::tests::METRICS_ARM.lock();
        let base = 0x20_0000usize;
        let ct = CardTable::with_options(base, CARD_SIZE * SUMMARY_GROUP * 4, ON);
        let idx = SUMMARY_GROUP * 2 + 9;
        // SAFETY: `cards_addr` is this live table's `[AtomicU8; num_cards]`
        // and `idx < num_cards`.
        let card = unsafe { &*(ct.cards_addr as *const AtomicU8).add(idx) };

        // Peer: cover + first summary store, then frozen.
        ct.cover_card_index(idx);
        ct.summarize(idx);
        // Collector: consumes the group — clears its summary byte, finds nothing.
        assert!(ct.take_dirty_cards().is_empty());
        // Peer resumes: the card store alone leaves the hazard in place...
        card.store(CARD_DIRTY, Ordering::Release);
        assert_eq!(brute(&ct), vec![idx]);
        assert!(
            ct.dirty_card_indices().is_empty(),
            "a dirty card under a clean summary byte is invisible to the scans",
        );
        // ...and the second summary store is what closes it.
        ct.summarize(idx);
        assert_eq!(ct.take_dirty_cards(), vec![idx]);
        assert!(brute(&ct).is_empty());

        // The production barrier does all three, so the ordinary path is fine.
        ct.mark_dirty_lockfree(base + idx * CARD_SIZE);
        assert_eq!(ct.take_dirty_cards(), vec![idx]);
    }

    /// A card stored ONLY through the raw address cannot maintain the summary,
    /// so handing the address out must switch the scans back to the byte map —
    /// the summary's version of `handing_out_the_raw_address_disarms_the_scan_bound`,
    /// and the test the O(heap) page asked for ("marks a card only through
    /// `jit_cards_addr` and asserts `take_dirty_cards` still returns it").
    #[test]
    fn a_raw_card_store_is_found_once_the_address_has_escaped() {
        let base = 0x30_0000usize;
        let ct = CardTable::with_options(base, CARD_SIZE * SUMMARY_GROUP * 4, ON);
        assert!(ct.summary_active());
        let cards = ct.jit_cards_addr() as *const AtomicU8;
        assert!(!ct.summary_active(), "an escaped address disarms the summary");
        let idx = SUMMARY_GROUP * 3 + 7;
        // SAFETY: the table's live byte map; `idx < num_cards`.
        unsafe { &*cards.add(idx) }.store(CARD_DIRTY, Ordering::Release);
        assert_eq!(ct.dirty_card_indices(), vec![idx]);
        assert_eq!(ct.take_dirty_cards(), vec![idx]);
        // SAFETY: as above.
        unsafe { &*cards.add(idx) }.store(CARD_DIRTY, Ordering::Release);
        ct.clear_all();
        assert!(brute(&ct).is_empty());
    }

    /// The options are what the table was built with, and LEGACY builds no
    /// summary at all.
    #[test]
    fn the_legacy_options_build_the_pre_summary_table() {
        let ct = CardTable::with_options(0x1000, CARD_SIZE * 1000, CardTableOptions::LEGACY);
        assert_eq!(ct.options(), CardTableOptions::LEGACY);
        assert!(ct.summary.is_empty());
        assert!(!ct.precise_ref_array_marks());
        let on = CardTable::with_options(
            0x1000,
            CARD_SIZE * 1000,
            CardTableOptions {
                summary: true,
                precise_ref_array_marks: true,
            },
        );
        assert_eq!(on.summary.len(), 1000usize.div_ceil(SUMMARY_GROUP));
        assert!(on.precise_ref_array_marks());
    }
}

/// gen r4w4/cards4 (2026-09-24): the read-only [`JitCardView`] the x64 JIT's
/// inline generational post barrier consults. A module of its own, at the end
/// of the file, so parallel lanes appending tests do not conflict.
#[cfg(test)]
mod gen_r4w4_cards4_tests {
    use super::*;

    const BOTH: CardTableOptions = CardTableOptions {
        summary: true,
        precise_ref_array_marks: true,
    };

    /// Exactly the arithmetic the emitted fast path performs
    /// (`jit/src/x64/objects.rs::emit_gen_card_check`): load `old_base` and
    /// `cards_neg` from the view, `idx = (mark - old_base) >> CARD_SHIFT`,
    /// `&card = idx - cards_neg`, read the byte. `None` where the emitted code
    /// would not read at all (outside `[old_base, old_end)`).
    fn read_like_the_jit(view_addr: usize, mark: usize) -> Option<bool> {
        // SAFETY: `view_addr` is `CardTable::jit_card_view_addr` of a table
        // the caller keeps alive for the whole call.
        let view = unsafe { &*(view_addr as *const JitCardView) };
        let base = view.old_base.load(Ordering::Acquire);
        let end = view.old_end.load(Ordering::Acquire);
        if mark < base || mark >= end {
            return None;
        }
        let idx = (mark - base) >> cratonvm_types::CARD_SHIFT;
        let card = idx.wrapping_sub(view.cards_neg.load(Ordering::Acquire));
        // SAFETY: `mark` is inside the covered range, so `idx < num_cards`
        // and `card` is `&cards[idx]` of the live byte map.
        Some(unsafe { &*(card as *const AtomicU8) }.load(Ordering::Acquire) == CARD_DIRTY)
    }

    /// The view names this table, and handing it out disarms NOTHING — the
    /// whole point of the protocol, and the difference from `jit_cards_addr`.
    #[test]
    fn the_view_describes_the_table_and_does_not_disarm_the_bound_or_the_summary() {
        let base = 0x50_0000usize;
        let num = SUMMARY_GROUP * 5 + 9;
        let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
        let addr = ct.jit_card_view_addr();
        assert_ne!(addr, 0);
        assert_eq!(addr, ct.jit_card_view() as *const JitCardView as usize);
        // SAFETY: the table is alive.
        let view = unsafe { &*(addr as *const JitCardView) };
        assert_eq!(view.magic, JIT_CARD_VIEW_MAGIC);
        assert_eq!(view.old_base.load(Ordering::Acquire), base);
        assert_eq!(view.old_end.load(Ordering::Acquire), base + CARD_SIZE * num);
        assert_eq!(view.flags, JitCardView::FLAG_PRECISE_REF_ARRAYS);
        assert!(!ct.raw_card_address_escaped(), "the view is not an escape");
        assert!(ct.summary_active(), "the summary stays armed");
        ct.mark_dirty_lockfree(base + 3 * CARD_SIZE);
        assert_eq!(ct.scan_bound_cards(), 4, "the bound stays self-maintained");
        assert_eq!(ct.take_dirty_cards(), vec![3]);
        assert!(ct.cards_skipped_by_scan_bound() > 0);
        // Only now compare the negated map address against the raw one, which
        // escapes the table (and is why this assertion comes last).
        assert_eq!(
            view.cards_neg.load(Ordering::Acquire).wrapping_neg(),
            ct.jit_cards_addr()
        );
        let legacy = CardTable::with_options(base, CARD_SIZE * num, CardTableOptions::LEGACY);
        assert_eq!(legacy.jit_card_view().flags, 0);
    }

    /// The layout the JIT copies. A drift here is a barrier that reads the
    /// wrong word, so it is pinned from this side as well as the JIT's.
    #[test]
    fn the_view_layout_is_the_documented_one() {
        assert_eq!(std::mem::offset_of!(JitCardView, magic), JitCardView::MAGIC_OFFSET);
        assert_eq!(std::mem::offset_of!(JitCardView, old_base), JitCardView::OLD_BASE_OFFSET);
        assert_eq!(std::mem::offset_of!(JitCardView, old_end), JitCardView::OLD_END_OFFSET);
        assert_eq!(std::mem::offset_of!(JitCardView, cards_neg), JitCardView::CARDS_NEG_OFFSET);
        assert_eq!(std::mem::offset_of!(JitCardView, flags), JitCardView::FLAGS_OFFSET);
        assert_eq!(std::mem::size_of::<JitCardView>(), 40);
    }

    /// The card the emitted read lands on is the card the Rust barrier marks,
    /// for every address in a region that is not a whole number of cards and
    /// starts off a card boundary.
    #[test]
    fn a_card_read_through_the_view_is_the_card_the_barrier_marks() {
        let base = 0x60_0000usize + 0x48;
        let size = CARD_SIZE * 37 + 200;
        let ct = CardTable::with_options(base, size, BOTH);
        let view = ct.jit_card_view_addr();
        assert_eq!(read_like_the_jit(view, base - 8), None);
        assert_eq!(read_like_the_jit(view, base + size), None);
        for off in (0..size).step_by(40) {
            let a = base + off;
            assert_eq!(read_like_the_jit(view, a), Some(false), "{off:#x}: clean");
            ct.mark_dirty_lockfree(a);
            assert_eq!(read_like_the_jit(view, a), Some(true), "{off:#x}: marked");
            assert!(ct.is_dirty(off / CARD_SIZE));
            ct.clear_all();
        }
    }

    /// The inline protocol end to end, against a brute-force model: every
    /// "store" reads its card through the view and calls the Rust barrier only
    /// on a clean answer. The scans must return exactly the modelled set, and
    /// the bound and the summary must both still be skipping cards — which is
    /// what the old raw-store emitter could not keep.
    #[test]
    fn check_then_call_keeps_every_card_and_both_skips_armed() {
        let _arm = super::tests::METRICS_ARM.lock();
        let base = 0x70_0000usize;
        let num = SUMMARY_GROUP * 16;
        let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
        let view = ct.jit_card_view_addr();
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut model: std::collections::BTreeSet<usize> = Default::default();
        let mut calls = 0usize;
        let mut stores = 0usize;
        for round in 0..40 {
            // Stores confined to summary groups 0 and 2, so the bound (at most
            // three groups) and the summary (group 1, always clean) both have
            // something to skip.
            for _ in 0..64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let group_bytes = CARD_SIZE * SUMMARY_GROUP;
                let group = if x & 1 == 0 { 0 } else { 2 };
                let a = base + group * group_bytes + ((x >> 1) as usize % group_bytes);
                stores += 1;
                if read_like_the_jit(view, a) == Some(false) {
                    calls += 1;
                    ct.mark_dirty_lockfree(a);
                }
                model.insert((a - base) / CARD_SIZE);
            }
            let expected: Vec<usize> = model.iter().copied().collect();
            if round % 2 == 0 {
                assert_eq!(ct.take_dirty_cards(), expected, "round {round}");
            } else {
                assert_eq!(ct.dirty_card_indices(), expected, "round {round}");
                ct.clear_all();
            }
            model.clear();
        }
        assert!(calls < stores, "a dirty card must spare the call");
        assert!(!ct.raw_card_address_escaped());
        assert!(
            ct.scan_bound_cards() <= 3 * SUMMARY_GROUP,
            "the bound tracks the stores"
        );
        assert!(ct.cards_skipped_by_scan_bound() > 0, "the bound is armed");
        assert!(ct.cards_skipped_by_summary() > 0, "the summary is armed");
    }

    /// `refresh_jit_card_view` republishes the current geometry; today that is
    /// the construction-time geometry, so it is idempotent.
    #[test]
    fn refreshing_the_view_is_idempotent_for_a_fixed_table() {
        let base = 0x80_0000usize;
        let ct = CardTable::with_options(base, CARD_SIZE * 100, BOTH);
        let v = ct.jit_card_view();
        let before = (
            v.old_base.load(Ordering::Acquire),
            v.old_end.load(Ordering::Acquire),
            v.cards_neg.load(Ordering::Acquire),
        );
        ct.refresh_jit_card_view();
        let after = (
            v.old_base.load(Ordering::Acquire),
            v.old_end.load(Ordering::Acquire),
            v.cards_neg.load(Ordering::Acquire),
        );
        assert_eq!(before, after);
        assert!(!ct.raw_card_address_escaped());
        // gen r4w5/review5: the idempotence above holds for a refresh that
        // does nothing at all, so it cannot tell a working refresh from a
        // deleted body. Clobber the three run-time words and require the
        // refresh to put back exactly the table's geometry.
        v.old_base.store(1, Ordering::Release);
        v.old_end.store(2, Ordering::Release);
        v.cards_neg.store(3, Ordering::Release);
        ct.refresh_jit_card_view();
        let restored = (
            v.old_base.load(Ordering::Acquire),
            v.old_end.load(Ordering::Acquire),
            v.cards_neg.load(Ordering::Acquire),
        );
        assert_eq!(restored, before, "refresh re-derives the view from the table");
        assert_eq!(restored.0, base);
        assert_eq!(restored.1, base + CARD_SIZE * 100);
    }
}

/// gcd d1/d (2026-09-27): `take_dirty_cards` answers from the byte map alone
/// (`gengc-r5w5-old9-card-table-review-residuals`, item 2).
#[cfg(test)]
mod gcd_d1d_take_tests {
    use super::*;

    /// Every mark path — the locked one, the bulk one (out of order and with
    /// duplicates, as the collector's deferred re-marks come), the lock-free
    /// barrier and the buffered drain — is found by the byte scan, ascending
    /// and once each, on the summary table and on the legacy one; a second take
    /// finds nothing; a bulk re-mark after `clear_all` is found again.
    #[test]
    fn the_byte_scan_returns_every_mark_ascending_and_once() {
        for options in [
            CardTableOptions::LEGACY,
            CardTableOptions {
                summary: true,
                precise_ref_array_marks: false,
            },
        ] {
            let base = 0x40_0000usize;
            let ct = CardTable::with_options(base, CARD_SIZE * 512, options);
            let at = |card: usize| base + card * CARD_SIZE + 8;
            ct.mark_dirty(at(300));
            ct.mark_dirty_bulk(&[at(200), at(7), at(200), at(450), at(7)]);
            ct.mark_dirty_lockfree(at(64));
            ct.mark_dirty_lockfree(at(300));
            ct.thread_local_dirty_addr(at(100));
            ct.flush_dirty_buffer();
            assert_eq!(ct.drain_pending(), 1, "card 100 was clean");
            assert_eq!(ct.take_dirty_cards(), vec![7, 64, 100, 200, 300, 450]);
            assert!(ct.take_dirty_cards().is_empty(), "consumed");
            ct.mark_dirty_bulk(&[at(9), at(3)]);
            ct.clear_all();
            assert!(ct.take_dirty_cards().is_empty(), "cleared");
            ct.mark_dirty_bulk(&[at(11), at(5), at(11)]);
            assert_eq!(ct.take_dirty_cards(), vec![5, 11], "re-marked");
        }
    }
}

/// gce e1/y: the bulk re-mark's repeat and already-dirty skips.
#[cfg(test)]
mod gce_e1y_bulk_tests {
    use super::*;

    /// A bulk list with runs of the same card, cards already dirty, cards
    /// outside the table and a card revisited after others leaves exactly the
    /// cards (and, with the summary, exactly the groups) that one
    /// `mark_dirty` per entry leaves, and the scan bound covers them all.
    #[test]
    fn e1y_a_bulk_remark_with_repeats_dirties_what_single_marks_dirty() {
        for options in [
            CardTableOptions::LEGACY,
            CardTableOptions {
                summary: true,
                precise_ref_array_marks: true,
            },
        ] {
            let base = 0x80_0000usize;
            let bulk = CardTable::with_options(base, CARD_SIZE * 1024, options);
            let single = CardTable::with_options(base, CARD_SIZE * 1024, options);
            let at = |card: usize, off: usize| base + card * CARD_SIZE + off;
            // Card 40 dirty before the bulk call (an already-dirty skip).
            bulk.mark_dirty(at(40, 0));
            single.mark_dirty(at(40, 0));
            let list = [
                at(3, 0),
                at(3, 8),
                at(3, 16),
                at(40, 8),
                at(900, 0),
                at(900, 504),
                at(3, 24),
                base - 8,
                base + CARD_SIZE * 1024,
                at(1023, 0),
            ];
            bulk.mark_dirty_bulk(&list);
            for &a in &list {
                single.mark_dirty(a);
            }
            assert_eq!(bulk.scan_bound_cards(), single.scan_bound_cards(), "{options:?}");
            assert_eq!(bulk.dirty_card_indices(), single.dirty_card_indices(), "{options:?}");
            assert_eq!(bulk.take_dirty_cards(), vec![3, 40, 900, 1023], "{options:?}");
            assert_eq!(single.take_dirty_cards(), vec![3, 40, 900, 1023], "{options:?}");
            assert!(bulk.take_dirty_cards().is_empty(), "consumed, summary included");
            // After a consuming scan the same list marks everything again.
            bulk.mark_dirty_bulk(&list);
            assert_eq!(bulk.take_dirty_cards(), vec![3, 40, 900, 1023], "{options:?}");
        }
    }
}
